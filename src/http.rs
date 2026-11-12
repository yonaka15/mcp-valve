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
//!
//! Redirects are not followed: the configured headers usually carry a secret,
//! and a redirect would send them to whatever host the response names.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Longest silence accepted while waiting for or reading a response. The clock
/// restarts with every read, so a tool that keeps reporting progress may run
/// for longer; a stalled connection ends the request instead of blocking the
/// daemon for good.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Upper bound for the body of an error response, which is only quoted.
const ERROR_BODY_LIMIT: u64 = 64 * 1024;

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
            .timeout_per_read(Some(IDLE_TIMEOUT))
            .timeout_per_write(Some(Duration::from_secs(60)))
            // Never forward the configured headers to another location.
            .max_redirects(0)
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

    /// Forget the session, so that the next `initialize` starts a new one.
    pub fn reset_session(&mut self) {
        self.session_id = None;
        self.protocol_version = None;
    }

    /// A 404 answer to a request that carried a session id means the session
    /// is gone. Returns the error to raise in that case.
    fn session_expired(&mut self, status: u16) -> Option<anyhow::Error> {
        if status == 404 && self.session_id.is_some() {
            self.reset_session();
            return Some(SessionExpired.into());
        }
        None
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
            let body = read_error_body(response);
            if let Some(expired) = self.session_expired(status) {
                return Err(expired);
            }
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

        if !(200..300).contains(&status) {
            // Reading the body also lets the connection close cleanly.
            let body = read_error_body(response);
            if let Some(expired) = self.session_expired(status) {
                return Err(expired);
            }
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

        // The session id is assigned by the answer to `initialize` and by
        // nothing else; a header on any other response is ignored.
        if message["method"] == "initialize" {
            if let Some(session) = header_text(&response, "mcp-session-id") {
                self.session_id = Some(session);
            }
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

fn read_error_body(response: ureq::http::Response<ureq::Body>) -> String {
    let mut text = String::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(ERROR_BODY_LIMIT)
        .read_to_string(&mut text);
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
    at_start: bool,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            at_start: true,
        }
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

            let mut line = line.trim_end_matches(['\n', '\r']);

            // A stream may begin with a UTF-8 byte order mark.
            if self.at_start {
                self.at_start = false;
                line = line.strip_prefix('\u{feff}').unwrap_or(line);
            }

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
///   `sh -c`; a failing command or empty output is an error. Braces inside
///   the command must be balanced (`awk '{print $1}'` and `${VAR}` are fine).
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
        let end = closing_brace(after).ok_or_else(|| {
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
    if out.chars().any(|c| c.is_control()) {
        return Err(anyhow!(
            "Header '{}': the value contains a control character",
            header
        ));
    }

    Ok(out)
}

/// Position of the `}` that closes a placeholder whose text starts at `text`,
/// counting nested braces. `None` when the braces never balance.
fn closing_brace(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(at),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
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

    #[test]
    fn sse_ignores_a_leading_byte_order_mark() {
        assert_eq!(events("\u{feff}data: first\n\ndata: second\n\n"), vec!["first", "second"]);
    }

    #[test]
    fn header_cmd_placeholder_allows_balanced_braces() {
        assert_eq!(
            expand_header_value("X", "{cmd:echo 'a b' | awk '{print $2}'} and {env:HOME}").unwrap(),
            format!("b and {}", std::env::var("HOME").unwrap())
        );
        assert_eq!(expand_header_value("X", "{cmd:V=tok; echo ${V}}").unwrap(), "tok");

        let err = expand_header_value("X", "{cmd:printf 'a\\tb'}").unwrap_err().to_string();
        assert!(err.contains("control character"), "{err}");
    }

    // ------------------------------------------------------------------------
    // Transport tests against a scripted HTTP server on 127.0.0.1
    // ------------------------------------------------------------------------

    use serde_json::json;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    struct Seen {
        headers: HashMap<String, String>,
        body: Value,
    }

    /// Serves `responses` in order, one per connection, and returns what each
    /// request looked like. The client must make exactly that many requests.
    fn serve(responses: Vec<String>) -> (String, JoinHandle<Vec<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());

        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());

                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();

                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }

                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();

                seen.push(Seen {
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });

                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            seen
        });

        (url, handle)
    }

    fn http(status: &str, content_type: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    fn json_ok(extra_headers: &str, body: Value) -> String {
        http("200 OK", "application/json", extra_headers, &body.to_string())
    }

    fn init_message() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "capabilities": {}}})
    }

    fn init_ok(session: &str) -> String {
        json_ok(
            &format!("Mcp-Session-Id: {session}\r\n"),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-06-18"}}),
        )
    }

    fn list_message(id: u64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}})
    }

    fn list_ok(id: u64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})
    }

    fn transport(url: &str) -> HttpTransport {
        let headers = HashMap::from([("X-API-KEY".to_string(), "k-123".to_string())]);
        HttpTransport::new(url, &headers).unwrap()
    }

    #[test]
    fn session_and_version_headers_follow_initialize() {
        let (url, server) = serve(vec![
            init_ok("s1"),
            http("202 Accepted", "text/plain", "", ""),
            // A session header on a later response must not replace the session.
            json_ok("Mcp-Session-Id: other\r\n", list_ok(2)),
            json_ok("", list_ok(3)),
        ]);
        let mut t = transport(&url);

        let init = t.request(&init_message()).unwrap();
        t.set_protocol_version(init["result"]["protocolVersion"].as_str().unwrap());
        t.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).unwrap();
        assert_eq!(t.request(&list_message(2)).unwrap(), list_ok(2));
        assert_eq!(t.request(&list_message(3)).unwrap(), list_ok(3));

        let seen = server.join().unwrap();
        assert_eq!(seen[0].body["method"], "initialize");
        assert!(!seen[0].headers.contains_key("mcp-session-id"));
        assert!(!seen[0].headers.contains_key("mcp-protocol-version"));
        for request in &seen {
            assert_eq!(request.headers["x-api-key"], "k-123");
            assert_eq!(request.headers["accept"], "application/json, text/event-stream");
            assert_eq!(request.headers["content-type"], "application/json");
        }
        for request in &seen[1..] {
            assert_eq!(request.headers["mcp-session-id"], "s1");
            assert_eq!(request.headers["mcp-protocol-version"], "2025-06-18");
        }
    }

    #[test]
    fn a_404_with_a_session_reports_session_expired_and_forgets_the_session() {
        let (url, server) = serve(vec![
            init_ok("s1"),
            http("404 Not Found", "text/plain", "", "unknown session"),
            json_ok("", list_ok(3)),
        ]);
        let mut t = transport(&url);
        t.request(&init_message()).unwrap();

        let err = t.request(&list_message(2)).unwrap_err();
        assert!(err.downcast_ref::<SessionExpired>().is_some(), "{err}");

        // The session is forgotten: the next request carries no session id.
        t.request(&list_message(3)).unwrap();
        let seen = server.join().unwrap();
        assert_eq!(seen[1].headers["mcp-session-id"], "s1");
        assert!(!seen[2].headers.contains_key("mcp-session-id"));
    }

    #[test]
    fn a_404_to_a_notification_also_reports_session_expired() {
        let (url, server) = serve(vec![
            init_ok("s1"),
            http("404 Not Found", "text/plain", "", "unknown session"),
        ]);
        let mut t = transport(&url);
        t.request(&init_message()).unwrap();

        let err = t
            .notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .unwrap_err();
        assert!(err.downcast_ref::<SessionExpired>().is_some(), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn an_event_stream_is_read_to_the_response_and_server_requests_are_answered() {
        let response = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}});
        let pretty = serde_json::to_string_pretty(&response).unwrap();
        let mut stream = String::from("\u{feff}: hello\n\nid: 1\ndata:\n\n");
        stream.push_str("event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n");
        stream.push_str("data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-ping\",\"method\":\"ping\"}\n\n");
        for line in pretty.lines() {
            stream.push_str(&format!("data: {line}\n"));
        }
        stream.push('\n');

        let (url, server) = serve(vec![
            http("200 OK", "text/event-stream; charset=utf-8", "", &stream),
            http("202 Accepted", "text/plain", "", ""),
        ]);
        let mut t = transport(&url);

        assert_eq!(t.request(&list_message(7)).unwrap(), response);

        let seen = server.join().unwrap();
        assert_eq!(seen[1].body, json!({"jsonrpc": "2.0", "id": "srv-ping", "result": {}}));
    }

    #[test]
    fn error_statuses_are_reported_and_redirects_are_not_followed() {
        let rpc_error = json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32602, "message": "bad"}});
        let (url, server) = serve(vec![
            http("400 Bad Request", "application/json", "", &rpc_error.to_string()),
            http("500 Internal Server Error", "text/plain", "", "kaboom"),
            http("302 Found", "text/plain", "Location: http://127.0.0.1:1/elsewhere\r\n", ""),
            http("200 OK", "application/json", "", ""),
        ]);
        let mut t = transport(&url);

        // A JSON-RPC error body is the server's answer, whatever the status.
        assert_eq!(t.request(&list_message(2)).unwrap(), rpc_error);

        let err = t.request(&list_message(3)).unwrap_err().to_string();
        assert!(err.contains("HTTP 500") && err.contains("kaboom"), "{err}");

        // Port 1 refuses connections: following the redirect would give a
        // connection error instead of this status.
        let err = t.request(&list_message(4)).unwrap_err().to_string();
        assert!(err.contains("HTTP 302"), "{err}");

        let err = t.request(&list_message(5)).unwrap_err().to_string();
        assert!(err.contains("empty body"), "{err}");

        assert_eq!(server.join().unwrap().len(), 4);
    }
}
