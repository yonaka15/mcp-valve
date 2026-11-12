//! Streamable HTTP transport (initialize-based revisions, 2025-03-26 to 2025-11-25).
//!
//! Every JSON-RPC message is its own HTTP POST to one MCP endpoint. The server
//! answers a request with either one JSON object or a Server-Sent Events stream
//! that ends with the response. A session id handed out on `initialize` is sent
//! back on every later request.
//!
//! Not implemented: the standalone GET stream, resuming a broken stream, OAuth,
//! the 2024-11-05 HTTP+SSE transport, and the stateless 2026-07-28 revision.
//! The session is not deleted on shutdown; servers expire sessions themselves.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The server answered 404 to a request that carried a session id: the session
/// is gone and the client has to initialize again.
#[derive(Debug)]
pub struct SessionExpired;

impl fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MCP session expired (HTTP 404)")
    }
}

impl std::error::Error for SessionExpired {}

#[derive(Clone)]
pub struct HttpTransport {
    agent: ureq::Agent,
    url: String,
    /// Extra headers from the config, with placeholders already expanded.
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl HttpTransport {
    /// Build the transport. Header placeholders are expanded here, once.
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(anyhow!(
                "Server url must start with http:// or https://, got '{}'",
                url
            ));
        }

        let mut expanded = Vec::with_capacity(headers.len());
        for (name, raw) in headers {
            expanded.push((name.clone(), expand_header_value(name, raw)?));
        }
        expanded.sort();

        let agent = ureq::Agent::config_builder()
            // Read 4xx/5xx bodies ourselves: they may carry a JSON-RPC error.
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .user_agent(concat!("mcp-valve/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();

        Ok(Self {
            agent,
            url: url.to_string(),
            headers: expanded,
            session_id: None,
            protocol_version: None,
        })
    }

    /// Remember the protocol version negotiated by `initialize`; it is sent as
    /// `MCP-Protocol-Version` on every later request.
    pub fn set_protocol_version(&mut self, version: &str) {
        self.protocol_version = Some(version.to_string());
    }

    fn post(&self, message: &Value) -> Result<ureq::http::Response<ureq::Body>> {
        let mut request = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(session) = &self.session_id {
            request = request.header("Mcp-Session-Id", session.as_str());
        }
        if let Some(version) = &self.protocol_version {
            request = request.header("MCP-Protocol-Version", version.as_str());
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        request
            .send(serde_json::to_string(message)?.as_str())
            .map_err(|e| anyhow!("HTTP request to {} failed: {}", self.url, e))
    }

    /// Send a notification (or a reply to a server request). The server answers
    /// 202 with no body; any 2xx is accepted.
    pub fn notify(&mut self, message: &Value) -> Result<()> {
        let response = self.post(message)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_body(response);
            return Err(anyhow!(
                "HTTP {} from {} for a notification: {}",
                status,
                self.url,
                brief(&body)
            ));
        }
        Ok(())
    }

    /// Send a request and return the JSON-RPC message that answers it.
    pub fn request(&mut self, message: &Value) -> Result<Value> {
        let id = &message["id"];
        let response = self.post(message)?;
        let status = response.status().as_u16();

        if status == 404 && self.session_id.is_some() {
            // Drain the body so the connection is closed cleanly.
            let _ = read_body(response);
            self.session_id = None;
            self.protocol_version = None;
            return Err(SessionExpired.into());
        }

        if let Some(session) = header_text(&response, "mcp-session-id") {
            self.session_id = Some(session);
        }

        if !(200..300).contains(&status) {
            let body = read_body(response);
            // A JSON-RPC error in the body is the server's answer: hand it on.
            if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(body.trim()) {
                if value.get("error").is_some() {
                    return Ok(value);
                }
            }
            return Err(anyhow!(
                "HTTP {} from {}: {}",
                status,
                self.url,
                brief(&body)
            ));
        }

        let is_event_stream = header_text(&response, "content-type")
            .map(|t| t.to_ascii_lowercase().starts_with("text/event-stream"))
            .unwrap_or(false);

        // Replies to server-to-client requests go out as their own POSTs.
        let mut replier = self.clone();
        let mut reply = |reply: &Value| replier.notify(reply);

        if is_event_stream {
            let mut events = SseReader::new(BufReader::new(response.into_body().into_reader()));
            crate::await_response(&mut || events.next_data(), &mut reply, id)
        } else {
            let body = read_body(response);
            if body.trim().is_empty() {
                return Err(anyhow!(
                    "HTTP {} from {} with an empty body: the server sent no response to request {}",
                    status,
                    self.url,
                    id
                ));
            }
            let mut once = Some(body);
            crate::await_response(&mut || Ok(once.take()), &mut reply, id)
        }
    }
}

fn header_text(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn read_body(response: ureq::http::Response<ureq::Body>) -> String {
    let mut text = String::new();
    let _ = response.into_body().into_reader().read_to_string(&mut text);
    text
}

fn brief(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "(empty body)".to_string();
    }
    text.chars().take(300).collect()
}

// ============================================================================
// Server-Sent Events
// ============================================================================

/// Reads the `data` payload of each event from an SSE stream.
pub struct SseReader<R: BufRead> {
    reader: R,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// The next event's data (its `data:` lines joined with newlines), or
    /// `None` when the stream has ended. Comments and the `event`, `id` and
    /// `retry` fields are ignored; events without data are skipped.
    pub fn next_data(&mut self) -> Result<Option<String>> {
        let mut data: Vec<String> = Vec::new();

        loop {
            let mut line = String::new();
            let bytes = self
                .reader
                .read_line(&mut line)
                .context("Failed to read the event stream")?;

            if bytes == 0 {
                // End of stream: deliver an event that had no closing blank line.
                return Ok(if data.is_empty() {
                    None
                } else {
                    Some(data.join("\n"))
                });
            }

            let line = line.trim_end_matches(['\n', '\r']);

            if line.is_empty() {
                if data.is_empty() {
                    continue;
                }
                return Ok(Some(data.join("\n")));
            }

            if line.starts_with(':') {
                continue;
            }

            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            } else if line == "data" {
                data.push(String::new());
            }
        }
    }
}

// ============================================================================
// Header Placeholders
// ============================================================================

/// Expand `{env:NAME}` and `{cmd:shell command}` in a header value.
///
/// - `{env:NAME}` is replaced by the environment variable; a missing variable
///   is an error.
/// - `{cmd:...}` is replaced by the trimmed stdout of the command, run with
///   `sh -c`; a failing command or empty output is an error. The command ends
///   at the first `}`.
///
/// Errors name the header and the variable or command, never the value.
pub fn expand_header_value(header: &str, raw: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = raw;

    loop {
        let next = ["{env:", "{cmd:"]
            .iter()
            .filter_map(|open| rest.find(open).map(|at| (at, *open)))
            .min();

        let Some((at, open)) = next else {
            out.push_str(rest);
            break;
        };

        out.push_str(&rest[..at]);
        let after = &rest[at + open.len()..];
        let end = after.find('}').ok_or_else(|| {
            anyhow!("Header '{}': '{}' has no closing '}}'", header, open)
        })?;
        let inner = &after[..end];
        rest = &after[end + 1..];

        if open == "{env:" {
            let value = std::env::var(inner).map_err(|_| {
                anyhow!(
                    "Header '{}': environment variable '{}' is not set",
                    header,
                    inner
                )
            })?;
            out.push_str(&value);
        } else {
            out.push_str(&run_header_command(header, inner)?);
        }
    }

    if out.contains(['\r', '\n']) {
        return Err(anyhow!("Header '{}': the value contains a line break", header));
    }

    Ok(out)
}

fn run_header_command(header: &str, command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Header '{}': failed to run '{}'", header, command))?;

    if !output.status.success() {
        return Err(anyhow!(
            "Header '{}': command '{}' failed ({})",
            header,
            command,
            output.status
        ));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err(anyhow!(
            "Header '{}': command '{}' printed nothing",
            header,
            command
        ));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn events(input: &str) -> Vec<String> {
        let mut reader = SseReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut out = Vec::new();
        while let Some(data) = reader.next_data().unwrap() {
            out.push(data);
        }
        out
    }

    #[test]
    fn sse_reads_data_and_ignores_other_fields() {
        let input = concat!(
            ": keep-alive comment\n",
            "event: message\n",
            "id: 42\n",
            "retry: 1000\n",
            "data: {\"a\":1}\n",
            "\n",
            "data:{\"b\":2}\n",
            "\n",
        );
        assert_eq!(events(input), vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn sse_joins_multi_line_data_and_handles_crlf_and_missing_final_blank_line() {
        let input = "data: {\"a\":\r\ndata: 1}\r\n\r\nid: 7\r\n\r\ndata: last";
        assert_eq!(events(input), vec!["{\"a\":\n1}", "last"]);
    }

    #[test]
    fn sse_delivers_an_event_with_empty_data_and_skips_events_without_data() {
        // "data:" with nothing after it is an event (a priming event); an
        // "id:"-only block is not.
        assert_eq!(events("id: 1\n\ndata:\n\ndata\n\n"), vec!["", ""]);
        assert!(events("").is_empty());
    }

    #[test]
    fn header_env_placeholder_is_expanded_and_missing_variable_is_an_error() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_header_value("X", "Bearer {env:HOME}!").unwrap(),
            format!("Bearer {}!", home)
        );
        assert_eq!(expand_header_value("X", "plain").unwrap(), "plain");

        let err = expand_header_value("X-API-KEY", "{env:MCP_VALVE_TEST_UNSET_VARIABLE}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("X-API-KEY"), "{err}");
        assert!(err.contains("MCP_VALVE_TEST_UNSET_VARIABLE"), "{err}");
    }

    #[test]
    fn header_cmd_placeholder_uses_trimmed_stdout_and_reports_failures() {
        assert_eq!(
            expand_header_value("X", "Bearer {cmd:printf 'tok\\n'}").unwrap(),
            "Bearer tok"
        );

        let err = expand_header_value("X", "{cmd:exit 3}").unwrap_err().to_string();
        assert!(err.contains("failed"), "{err}");

        let err = expand_header_value("X", "{cmd:true}").unwrap_err().to_string();
        assert!(err.contains("printed nothing"), "{err}");

        let err = expand_header_value("X", "{cmd:echo hi").unwrap_err().to_string();
        assert!(err.contains("no closing"), "{err}");

        let err = expand_header_value("X", "{cmd:printf 'a\\nb'}").unwrap_err().to_string();
        assert!(err.contains("line break"), "{err}");
    }

    #[test]
    fn url_must_be_http_or_https() {
        let none = HashMap::new();
        assert!(HttpTransport::new("http://127.0.0.1:1/mcp", &none).is_ok());
        assert!(HttpTransport::new("https://example.com/mcp", &none).is_ok());
        assert!(HttpTransport::new("ftp://example.com/mcp", &none).is_err());
        assert!(HttpTransport::new("127.0.0.1:8780/mcp", &none).is_err());
    }
}
