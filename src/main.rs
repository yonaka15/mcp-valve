//! # Unified MCP CLI
//!
//! A generic MCP (Model Context Protocol) client that works with any MCP server
//! through configurable server profiles.
//!
//! ## Design Philosophy
//!
//! **This script is a thin wrapper around the MCP protocol.**
//!
//! - Tools are called by providing JSON arguments that conform to the `inputSchema` from `tools/list`
//! - **No validation or field name conversion** occurs in this script
//! - The authoritative source of truth is the MCP server's `inputSchema`
//!
//! **Golden Rule**: If `tools/list` shows a field named `X` in the schema, use `X` exactly as-is in the JSON.
//!
//! ## Overview
//!
//! This tool provides a unified interface to any MCP server. Instead of maintaining
//! separate CLI tools for each MCP server, configure servers in `~/.config/mcp-valve/servers.json`
//! and use this single client.
//!
//! ## Quick Start
//!
//! ```bash
//! # List configured servers
//! mcp-valve list-servers
//!
//! # Start daemon (required before any tool operations)
//! cd /path/to/your/project
//! mcp-valve --server playwright start-daemon --server-args '["--gui"]'
//!
//! # List tools (requires daemon)
//! mcp-valve --server playwright list-tools
//!
//! # Call a tool (requires daemon)
//! mcp-valve --server playwright call browser_navigate --args '{"url":"https://example.com"}'
//!
//! # Check daemon status
//! mcp-valve --server playwright daemon-status
//!
//! # Stop daemon
//! mcp-valve --server playwright stop-daemon
//! ```
//!
//! ## Configuration
//!
//! Config file is searched in order: `--config` flag, `MCP_VALVE_CONFIG` env var,
//! `$XDG_CONFIG_HOME/mcp-valve/servers.json`, `~/.config/mcp-valve/servers.json`,
//! `~/.claude/scripts/mcp-servers.json` (legacy).
//!
//! Example config:
//!
//! ```json
//! {
//!   "playwright": {
//!     "command": ["npx", "@playwright/mcp@latest"],
//!     "default_args": ["--headless"],
//!     "supports_daemon": true,
//!     "description": "Playwright browser automation",
//!     "env": {}
//!   },
//!   "zen": {
//!     "command": ["/Users/yonaka/zen-mcp-server/.zen_venv/bin/python"],
//!     "default_args": ["/Users/yonaka/zen-mcp-server/server.py"],
//!     "supports_daemon": false,
//!     "description": "Zen MCP multi-AI model integration",
//!     "env": {}
//!   }
//! }
//! ```
//!
//! ## Features
//!
//! - ✅ Generic MCP protocol client
//! - ✅ JSON-based server configuration
//! - ✅ Support for any MCP server
//! - ✅ Interactive shell mode
//! - ✅ Server-specific arguments via --server-args
//! - ✅ Daemon mode with persistent state
//! - ✅ Project-aware: displays current working directory for all operations
//!
//! ## Technical Details
//!
//! - **Protocol**: MCP 2025-06-18 (JSON-RPC 2.0)
//! - **Transport**: STDIO / Unix socket (daemon)
//! - **Dependencies**: serde, serde_json, anyhow, clap, nix

#[cfg(not(unix))]
compile_error!("mcp-valve requires a Unix platform (Linux, macOS, BSD)");

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::{umask, Mode};
use nix::unistd::{setsid, Pid};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

// ============================================================================
// Configuration
// ============================================================================

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ServerProfile {
    command: Vec<String>,
    #[serde(default)]
    default_args: Vec<String>,
    #[serde(default)]
    supports_daemon: bool,
    #[serde(default)]
    description: String,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ServerConfig {
    #[serde(flatten)]
    servers: HashMap<String, ServerProfile>,
}

/// Resolves config file path with priority:
/// 1. CLI flag (--config)
/// 2. Environment variable (MCP_VALVE_CONFIG)
/// 3. XDG_CONFIG_HOME/mcp-valve/servers.json
/// 4. ~/.config/mcp-valve/servers.json
/// 5. ~/.claude/scripts/mcp-servers.json (legacy)
fn get_config_path(cli_config: Option<PathBuf>) -> Result<PathBuf> {
    // 1. CLI flag (highest priority)
    if let Some(path) = cli_config {
        return Ok(path);
    }

    // 2. Environment variable
    if let Ok(path) = std::env::var("MCP_VALVE_CONFIG") {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var("HOME").context("HOME environment variable not set")?;

    // 3. XDG_CONFIG_HOME if set
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let path = PathBuf::from(xdg).join("mcp-valve/servers.json");
        if path.exists() {
            return Ok(path);
        }
    }

    // 4. XDG default location
    let xdg_default = PathBuf::from(&home).join(".config/mcp-valve/servers.json");
    if xdg_default.exists() {
        return Ok(xdg_default);
    }

    // 5. Legacy fallback
    Ok(PathBuf::from(&home).join(".claude/scripts/mcp-servers.json"))
}

fn load_server_config(cli_config: Option<PathBuf>) -> Result<ServerConfig> {
    let config_path = get_config_path(cli_config)?;

    if !config_path.exists() {
        let home = std::env::var("HOME").unwrap_or_default();
        return Err(anyhow!(
            "Configuration file not found.\n\n\
            Searched locations (in order):\n  \
            1. --config flag or MCP_VALVE_CONFIG env var\n  \
            2. $XDG_CONFIG_HOME/mcp-valve/servers.json\n  \
            3. ~/.config/mcp-valve/servers.json\n  \
            4. ~/.claude/scripts/mcp-servers.json\n\n\
            Create a config file at: {}\n\n\
            Example:\n\
            {{\n  \
              \"server-name\": {{\n    \
                \"command\": [\"npx\", \"@example/mcp-server\"],\n    \
                \"default_args\": [],\n    \
                \"supports_daemon\": true,\n    \
                \"description\": \"Example MCP server\",\n    \
                \"env\": {{}}\n  \
              }}\n\
            }}",
            PathBuf::from(&home).join(".config/mcp-valve/servers.json").display()
        ));
    }

    let config_content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {}", config_path.display()))?;

    let config: ServerConfig = serde_json::from_str(&config_content)
        .with_context(|| format!("Invalid JSON in config: {}", config_path.display()))?;

    Ok(config)
}

// ============================================================================
// CLI Definition
// ============================================================================

#[derive(Parser)]
#[command(name = "mcp-valve")]
#[command(about = "Unified MCP CLI - Generic MCP Protocol Client")]
#[command(version)]
struct Cli {
    /// Server name from config (e.g., playwright, zen)
    #[arg(short, long)]
    server: Option<String>,

    /// Additional server arguments (JSON array, e.g., '["--gui", "--browser", "firefox"]')
    #[arg(long)]
    server_args: Option<String>,

    /// Path to config file (overrides default locations)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all configured servers
    ListServers,

    /// Call any MCP tool
    Call {
        /// Tool name (e.g., browser_navigate, chat)
        tool: String,
        /// Arguments as JSON string
        #[arg(short, long, default_value = "{}")]
        args: String,
        /// Print the tool's result instead of the MCP envelope:
        /// structuredContent when the server sent it, otherwise each
        /// content[].text exactly as the server sent it
        #[arg(long)]
        result: bool,
    },

    /// List all available tools from the server
    ListTools,

    /// Interactive shell mode
    Shell,

    /// Start background daemon (requires supports_daemon: true)
    StartDaemon,

    /// Stop background daemon
    StopDaemon,

    /// Check daemon status
    DaemonStatus,
}

// ============================================================================
// Template Variable Expansion
// ============================================================================

/// Sanitizes server name to prevent path traversal attacks
///
/// Only allows alphanumeric characters, hyphens, and underscores
fn sanitize_server_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Expands template variables in argument strings
///
/// Supported variables:
/// - {profile_dir}: .mcp-profile/<server-name> (sanitized)
/// - {pid}: Process ID
/// - {cwd}: Current working directory
///
/// Security: Server names are sanitized to prevent path traversal
fn expand_template_vars(arg: &str, server_name: &str) -> String {
    let safe_server_name = sanitize_server_name(server_name);
    let profile_dir = PathBuf::from(".mcp-profile").join(&safe_server_name);
    let profile_dir_str = profile_dir.to_str().unwrap_or("");
    let pid = std::process::id().to_string();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| ".".to_string());

    arg.replace("{profile_dir}", profile_dir_str)
        .replace("{pid}", &pid)
        .replace("{cwd}", &cwd)
}

// ============================================================================
// MCP Client (Generic)
// ============================================================================

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: u64,
}

impl McpClient {
    fn start(profile: &ServerProfile, extra_args: Option<Vec<String>>, server_name: &str) -> Result<Self> {
        eprintln!("🚀 Starting MCP server...");

        if profile.command.is_empty() {
            return Err(anyhow!("Server profile has empty command"));
        }

        let mut cmd = Command::new(&profile.command[0]);

        // Add command args (e.g., for npx: "@playwright/mcp@latest")
        if profile.command.len() > 1 {
            cmd.args(&profile.command[1..]);
        }

        // Add args: if --server-args was provided (even if empty), use it to override default_args
        // Otherwise use default_args from profile
        // Template variables are expanded for both default_args and extra_args
        let args_to_use = match extra_args {
            Some(args) => args.iter().map(|arg| expand_template_vars(arg, server_name)).collect(),
            None => profile.default_args.iter().map(|arg| expand_template_vars(arg, server_name)).collect::<Vec<String>>(),
        };
        cmd.args(&args_to_use);

        // Set environment variables
        for (key, value) in &profile.env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {:?}", profile.command))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        let mut mcp = Self {
            child,
            stdin,
            stdout,
            request_id: 0,
        };

        mcp.initialize()?;
        eprintln!("✅ MCP server ready");
        Ok(mcp)
    }

    fn initialize(&mut self) -> Result<()> {
        let init_request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-valve",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });

        self.send_request(&init_request)?;

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });

        self.send_notification(&notification)?;
        Ok(())
    }

    fn send_request(&mut self, request: &Value) -> Result<Value> {
        let request_str = serde_json::to_string(request)?;
        writeln!(self.stdin, "{}", request_str)?;
        self.stdin.flush()?;

        let response = read_response(&mut self.stdout, &mut self.stdin, &request["id"])?;

        if let Some(error) = response.get("error") {
            return Err(anyhow!("MCP Error: {}", error));
        }

        Ok(response)
    }

    fn send_notification(&mut self, notification: &Value) -> Result<()> {
        let notif_str = serde_json::to_string(notification)?;
        writeln!(self.stdin, "{}", notif_str)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn next_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        });

        let response = match self.send_request(&request) {
            Ok(resp) => resp,
            Err(e) => {
                let error_with_schema = self.format_error_with_schema(name, &e.to_string());
                return Err(anyhow!("{}", error_with_schema));
            }
        };
        let result = response["result"].clone();

        // Check for tool-level errors (isError field in result)
        if let Some(is_error) = result.get("isError").and_then(|v| v.as_bool()) {
            if is_error {
                // Extract error message from content if available
                let error_msg = result
                    .get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|item| item.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("Tool execution failed");

                let error_with_schema =
                    self.format_error_with_schema(name, &format!("Tool Error: {}", error_msg));
                return Err(anyhow!("{}", error_with_schema));
            }
        }

        Ok(result)
    }

    fn list_tools(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/list",
            "params": {}
        });

        let response = self.send_request(&request)?;
        Ok(response["result"].clone())
    }

    /// Get the inputSchema for a specific tool
    fn get_tool_schema(&mut self, tool_name: &str) -> Option<Value> {
        self.list_tools()
            .ok()
            .and_then(|result| result.get("tools").cloned())
            .and_then(|tools| tools.as_array().cloned())
            .and_then(|tools| {
                tools
                    .into_iter()
                    .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(tool_name))
            })
            .and_then(|tool| tool.get("inputSchema").cloned())
    }

    /// Format error message with tool schema appended
    fn format_error_with_schema(&mut self, tool_name: &str, error_msg: &str) -> String {
        match self.get_tool_schema(tool_name) {
            Some(schema) => {
                let schema_str = serde_json::to_string_pretty(&schema)
                    .unwrap_or_else(|_| schema.to_string());
                format!(
                    "{}\n\nSchema for tool '{}':\n{}",
                    error_msg, tool_name, schema_str
                )
            }
            None => error_msg.to_string(),
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ============================================================================
// Project Context
// ============================================================================

/// Get the current project path (current working directory)
fn get_project_path() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| ".".to_string())
}

/// Format error message when daemon is not running
fn daemon_not_running_error(server_name: &str) -> anyhow::Error {
    let project = get_project_path();
    anyhow!(
        "Daemon is not running for project '{}'\n\n\
        Start daemon with:\n  \
        cd {}\n  \
        mcp-valve --server {} start-daemon",
        project, project, server_name
    )
}

/// Format error message when the server's config does not enable daemon mode.
///
/// Tool operations only run through the daemon, so starting it is not the
/// fix here: the config field is. Name the field and the file that holds it.
/// A missing field deserializes to `false`, so the text covers both cases.
fn daemon_disabled_message(server_name: &str, config_path: &str, project: &str) -> String {
    format!(
        "Server '{}' does not have \"supports_daemon\": true in its config \
        (the field is missing or false)\n\n\
        Tool operations (call, list-tools, shell) need daemon mode.\n\
        Set \"supports_daemon\": true for '{}' in:\n  \
        {}\n\n\
        Then start the daemon:\n  \
        cd {}\n  \
        mcp-valve --server {} start-daemon",
        server_name, server_name, config_path, project, server_name
    )
}

/// Pick the error for a tool operation that found no running daemon:
/// a config problem when daemon mode is disabled, otherwise "not running".
fn daemon_unavailable_error(
    server_name: &str,
    profile: &ServerProfile,
    cli_config: Option<PathBuf>,
) -> anyhow::Error {
    if profile.supports_daemon {
        return daemon_not_running_error(server_name);
    }

    let config_path = get_config_path(cli_config)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "your mcp-valve config file".to_string());

    anyhow!(
        "{}",
        daemon_disabled_message(server_name, &config_path, &get_project_path())
    )
}

/// Arguments for the `__internal_daemon` process that `start-daemon` spawns.
///
/// The config path is always passed, so the daemon loads the same file the
/// client resolved (`--config`, `MCP_VALVE_CONFIG`, XDG or legacy) instead of
/// resolving one again on its own.
fn internal_daemon_args(
    server_name: &str,
    extra_args: Option<&[String]>,
    config_path: &Path,
) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = vec![
        "__internal_daemon".into(),
        "--server".into(),
        server_name.into(),
        "--config".into(),
        config_path.as_os_str().to_owned(),
    ];

    if let Some(extra) = extra_args {
        args.push("--server-args".into());
        args.push(serde_json::to_string(extra)?.into());
    }

    Ok(args)
}

// ============================================================================
// Daemon Management
// ============================================================================

struct DaemonManager {
    server_name: String,
    pid_file: PathBuf,
}

impl DaemonManager {
    fn new(server_name: &str) -> Self {
        let safe_server_name = sanitize_server_name(server_name);
        let profile_dir = PathBuf::from(".mcp-profile")
            .join(&safe_server_name);

        // Ensure profile directory exists with secure permissions (0700)
        if !profile_dir.exists() {
            let old_umask = umask(Mode::from_bits_truncate(0o077));
            fs::create_dir_all(&profile_dir)
                .expect("Failed to create daemon profile directory");
            umask(old_umask);
        }

        Self {
            server_name: server_name.to_string(),
            pid_file: profile_dir.join("daemon.pid"),
        }
    }

    fn get_socket_path(&self) -> Result<PathBuf> {
        // Read daemon PID from file
        let pid_str = fs::read_to_string(&self.pid_file)
            .context("Failed to read PID file")?;
        let pid = pid_str.trim();

        // Socket path includes PID to avoid conflicts
        Ok(PathBuf::from("/tmp/.mcp").join(format!("{}-{}.sock", self.server_name, pid)))
    }

    fn is_running(&self) -> Result<bool> {
        if !self.pid_file.exists() {
            return Ok(false);
        }

        let pid_str = fs::read_to_string(&self.pid_file)
            .context("Failed to read PID file")?;
        let pid = pid_str.trim().parse::<i32>()
            .with_context(|| format!("Invalid PID in file: '{}'", pid_str.trim()))?;

        // Check if process exists using kill with signal 0
        // This doesn't send any signal but checks if process exists and we have permission
        match kill(Pid::from_raw(pid), None) {
            Ok(_) => Ok(true),  // Process exists
            Err(nix::errno::Errno::ESRCH) => Ok(false),  // No such process
            Err(nix::errno::Errno::EPERM) => Ok(true),   // Process exists but no permission
            Err(_) => Ok(false),  // Other errors, assume not running
        }
    }

    fn start(
        &self,
        profile: &ServerProfile,
        extra_args: Option<Vec<String>>,
        config_path: &Path,
    ) -> Result<()> {
        if !profile.supports_daemon {
            return Err(anyhow!(
                "Server '{}' does not support daemon mode (supports_daemon: false)",
                self.server_name
            ));
        }

        if self.is_running()? {
            return Err(anyhow!("Daemon already running for '{}'", self.server_name));
        }

        let project = get_project_path();
        eprintln!("Project: {}", project);
        eprintln!("Profile: {}", self.pid_file.parent().unwrap().display());
        eprintln!("Starting MCP daemon for '{}'...", self.server_name);

        // Build daemon command
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(internal_daemon_args(
            &self.server_name,
            extra_args.as_deref(),
            config_path,
        )?);

        // Create log file for daemon stderr
        let profile_dir = self.pid_file.parent().unwrap();
        let log_file = std::fs::File::create(profile_dir.join("daemon.log"))
            .context("Failed to create daemon log file")?;

        // Fork daemon process with proper daemonization
        let child = unsafe {
            cmd.pre_exec(|| {
                // Create new session to detach from controlling terminal
                setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })
        }
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .context("Failed to spawn daemon process")?;

        let child_pid = child.id();

        // Write PID file
        fs::write(&self.pid_file, child_pid.to_string())
            .context("Failed to write PID file")?;

        // Construct expected socket path based on child PID
        let expected_socket = PathBuf::from("/tmp/.mcp")
            .join(format!("{}-{}.sock", self.server_name, child_pid));

        // Wait for socket file to appear
        for i in 0..50 {
            if expected_socket.exists() {
                eprintln!("Daemon started (PID: {})", child_pid);
                eprintln!("Socket: {}", expected_socket.display());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));

            // After 2 seconds, check if process is still alive
            if i == 20 {
                // Use kill with signal 0 to check if process exists
                if kill(Pid::from_raw(child_pid as i32), None).is_err() {
                    fs::remove_file(&self.pid_file).ok();
                    return Err(anyhow!(
                        "Daemon process exited unexpectedly. Check {}/daemon.log",
                        profile_dir.display()
                    ));
                }
            }
        }

        // Timeout
        fs::remove_file(&self.pid_file).ok();
        Err(anyhow!(
            "Daemon failed to start - socket file not created within 5 seconds. Check {}/daemon.log",
            profile_dir.display()
        ))
    }

    fn stop(&self) -> Result<()> {
        if !self.is_running()? {
            return Err(daemon_not_running_error(&self.server_name));
        }

        let project = get_project_path();
        let pid_str = fs::read_to_string(&self.pid_file)?;
        let pid: i32 = pid_str.trim().parse()
            .context("Invalid PID in file")?;

        let socket_path = self.get_socket_path().ok();

        eprintln!("Project: {}", project);
        eprintln!("Stopping daemon (PID: {})...", pid);

        // Send SIGTERM
        kill(Pid::from_raw(pid), Signal::SIGTERM)
            .context("Failed to send SIGTERM")?;

        // Wait for graceful shutdown
        for _ in 0..10 {
            if !self.is_running()? {
                fs::remove_file(&self.pid_file).ok();
                if let Some(ref sp) = socket_path {
                    if sp.exists() {
                        fs::remove_file(sp).ok();
                    }
                }
                eprintln!("Daemon stopped");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        // Force kill
        kill(Pid::from_raw(pid), Signal::SIGKILL)
            .context("Failed to send SIGKILL")?;

        fs::remove_file(&self.pid_file).ok();
        if let Some(ref sp) = socket_path {
            if sp.exists() {
                fs::remove_file(sp).ok();
            }
        }

        eprintln!("Daemon stopped (forced)");
        Ok(())
    }

    fn status(&self) -> Result<()> {
        let project = get_project_path();
        let profile_dir = self.pid_file.parent().unwrap();
        println!("Project: {}", project);
        println!("Server: {}", self.server_name);
        println!("Profile: {}", profile_dir.display());

        if self.is_running()? {
            let pid_str = fs::read_to_string(&self.pid_file)?;
            let socket_path = self.get_socket_path()?;
            println!("Daemon is running");
            println!("  PID: {}", pid_str.trim());
            println!("  Socket: {}", socket_path.display());
        } else {
            println!("Daemon is not running");
            if self.pid_file.exists() {
                eprintln!("Warning: Stale PID file found, cleaning up...");
                let socket_path = self.get_socket_path().ok();
                fs::remove_file(&self.pid_file).ok();
                if let Some(sp) = socket_path {
                    if sp.exists() {
                        fs::remove_file(&sp).ok();
                    }
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// Unix Socket Communication
// ============================================================================

fn run_daemon(server_name: &str, profile: &ServerProfile, extra_args: Option<Vec<String>>) -> Result<()> {
    // Use /tmp for socket with daemon's own PID
    let socket_dir = PathBuf::from("/tmp/.mcp");

    // Ensure socket directory exists with secure permissions
    if !socket_dir.exists() {
        let old_umask = umask(Mode::from_bits_truncate(0o077));
        fs::create_dir_all(&socket_dir)
            .context("Failed to create socket directory")?;
        umask(old_umask);
    }

    let socket_path = socket_dir.join(format!("{}-{}.sock", server_name, std::process::id()));

    // Clean up old socket
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)
        .context("Failed to bind Unix socket")?;

    // Restrict socket permissions to owner only (0600)
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .context("Failed to set socket permissions")?;

    eprintln!("Daemon listening on {:?}", socket_path);

    // Start MCP server instance
    let mut mcp = McpClient::start(profile, extra_args, server_name)?;

    // Handle connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = handle_client(&mut mcp, stream) {
                    eprintln!("Client error: {}", e);
                }
            }
            Err(e) => {
                eprintln!("Connection error: {}", e);
            }
        }
    }

    Ok(())
}

fn handle_client(mcp: &mut McpClient, mut stream: UnixStream) -> Result<()> {
    const MAX_REQUEST_SIZE: usize = 1024 * 1024; // 1MB limit

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::with_capacity(8192);
    reader.read_line(&mut line)?;

    if line.len() > MAX_REQUEST_SIZE {
        return Err(anyhow!("Request too large: {} bytes", line.len()));
    }

    let request: Value = serde_json::from_str(line.trim())
        .context("Invalid JSON-RPC request")?;

    let method = request["method"].as_str()
        .ok_or_else(|| anyhow!("Missing method"))?;

    let response = match method {
        "tools/call" => {
            let params = &request["params"];
            let tool_name = params["name"].as_str()
                .ok_or_else(|| anyhow!("Missing tool name"))?;
            let args = params["arguments"].clone();

            match mcp.call_tool(tool_name, args) {
                Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"message": e.to_string()}
                }),
            }
        }
        "tools/list" => {
            match mcp.list_tools() {
                Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"message": e.to_string()}
                }),
            }
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"message": format!("Unknown method: {}", method)}
        }),
    };

    let response_str = serde_json::to_string(&response)?;
    writeln!(stream, "{}", response_str)?;

    Ok(())
}

fn connect_to_daemon(server_name: &str) -> Result<UnixStream> {
    let daemon_mgr = DaemonManager::new(server_name);
    let socket_path = daemon_mgr.get_socket_path()
        .context("Failed to get socket path (daemon not started?)")?;

    let stream = UnixStream::connect(&socket_path)
        .context("Failed to connect to daemon (is it running?)")?;

    // Set timeouts
    stream.set_read_timeout(Some(Duration::from_secs(30)))
        .context("Failed to set read timeout")?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))
        .context("Failed to set write timeout")?;

    Ok(stream)
}

fn send_daemon_request(mut stream: UnixStream, request: Value) -> Result<Value> {
    let request_str = serde_json::to_string(&request)?;
    writeln!(stream, "{}", request_str)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response: Value = serde_json::from_str(line.trim())
        .context("Invalid JSON-RPC response")?;

    if let Some(error) = response.get("error") {
        return Err(anyhow!("Daemon error: {}", error));
    }

    Ok(response["result"].clone())
}

fn call_via_daemon(server_name: &str, tool: &str, args: Value) -> Result<Value> {
    let stream = connect_to_daemon(server_name)?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": args
        }
    });

    send_daemon_request(stream, request)
}

fn list_tools_via_daemon(server_name: &str) -> Result<Value> {
    let stream = connect_to_daemon(server_name)?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    send_daemon_request(stream, request)
}

// ============================================================================
// Response Matching
// ============================================================================

/// Read server messages until the response to request `id` arrives.
///
/// A server may write other messages before the response. Each one is handled
/// and skipped, so it can never be mistaken for the response (which would also
/// leave the real response in the pipe for the next request):
///
/// - a notification (`method`, no `id`), e.g. `notifications/progress`: logged
/// - a server-to-client request (`method` and `id`): answered, because the
///   server may wait for the answer before it continues. `ping` gets an empty
///   result; anything else gets "Method not found", since this client
///   implements no client-side features
/// - a response carrying a different `id`: logged
/// - a line that is not a JSON object: logged
///
/// An error with a null `id` is returned as the response: JSON-RPC uses it
/// when the server could not read the request at all, and requests are sent
/// one at a time, so it can only be about the request in flight.
///
/// There is no timeout and no limit on skipped messages: a tool may report
/// progress for as long as it runs.
///
/// Logging goes to stderr, which is `daemon.log` for the daemon process.
fn read_response<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    id: &Value,
) -> Result<Value> {
    let brief = |text: &str| -> String { text.chars().take(200).collect() };

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("Failed to read from MCP server")?;
        if bytes == 0 {
            return Err(anyhow!(
                "MCP server closed its output before answering request {}",
                id
            ));
        }

        let text = line.trim();
        if text.is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(text) {
            Ok(value @ Value::Object(_)) => value,
            _ => {
                eprintln!(
                    "Skipping output that is not a JSON-RPC object: {}",
                    brief(text)
                );
                continue;
            }
        };

        let message_id = message.get("id").filter(|v| !v.is_null());

        if let Some(method) = message.get("method").and_then(|m| m.as_str()) {
            let Some(server_request_id) = message_id else {
                eprintln!("Skipping notification from server: {}", method);
                continue;
            };

            let reply = if method == "ping" {
                json!({"jsonrpc": "2.0", "id": server_request_id, "result": {}})
            } else {
                eprintln!("Declining request from server: {}", method);
                json!({
                    "jsonrpc": "2.0",
                    "id": server_request_id,
                    "error": {"code": -32601, "message": format!("Method not found: {}", method)}
                })
            };
            writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
            writer.flush()?;
            continue;
        }

        if message_id == Some(id) {
            return Ok(message);
        }

        if message_id.is_none() && message.get("error").is_some() {
            return Ok(message);
        }

        eprintln!(
            "Skipping response with unexpected id (waiting for {}): {}",
            id,
            brief(text)
        );
    }
}

// ============================================================================
// Result Unwrapping (call --result)
// ============================================================================

/// Turn a `tools/call` result into the documents `call --result` prints.
///
/// - `structuredContent` present and not null (any JSON value): that value
///   alone, as JSON.
/// - otherwise one document per `content` item: a text item's text exactly as
///   the server sent it, and a non-text item (image, audio, resource, ...) as
///   its JSON object.
/// - a result with neither field is printed whole, so nothing is hidden.
///
/// Text is never parsed and re-serialised: doing so would rewrite integers
/// beyond u64 as floats and reorder object keys. JSON text is already valid
/// input for jq as it is.
fn unwrap_tool_result(result: &Value) -> Vec<String> {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());

    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return vec![pretty(structured)];
    }

    let Some(items) = result.get("content").and_then(|c| c.as_array()) else {
        return vec![pretty(result)];
    };

    items
        .iter()
        .map(|item| {
            let is_text = item.get("type").and_then(|t| t.as_str()) == Some("text");
            match item.get("text").and_then(|t| t.as_str()) {
                Some(text) if is_text => text.to_string(),
                _ => pretty(item),
            }
        })
        .collect()
}

// ============================================================================
// Main
// ============================================================================

fn main() -> Result<()> {
    // Handle internal daemon command BEFORE clap parsing
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "__internal_daemon" {
        // Find --server, --server-args, and --config by manual parsing
        let server_name = args.iter()
            .position(|a| a == "--server")
            .and_then(|i| args.get(i + 1))
            .ok_or_else(|| anyhow!("__internal_daemon requires --server"))?
            .clone();

        let extra_args = args.iter()
            .position(|a| a == "--server-args")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());

        let cli_config = args.iter()
            .position(|a| a == "--config" || a == "-c")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from);

        let config = load_server_config(cli_config)?;
        let profile = config.servers.get(&server_name)
            .ok_or_else(|| anyhow!("Server '{}' not found", server_name))?;

        return run_daemon(&server_name, profile, extra_args);
    }

    // Filter out empty arguments
    let filtered_args: Vec<String> = std::env::args()
        .filter(|arg| !arg.is_empty())
        .collect();

    let cli = Cli::parse_from(filtered_args);

    match cli.command {
        Commands::ListServers => {
            let config = load_server_config(cli.config.clone())?;
            println!("Configured MCP servers:\n");
            for (name, profile) in config.servers {
                let desc = if profile.description.is_empty() {
                    "No description"
                } else {
                    &profile.description
                };
                println!("  {}: {}", name, desc);
                println!("    Command: {:?}", profile.command);
                if !profile.default_args.is_empty() {
                    println!("    Default args: {:?}", profile.default_args);
                }
                if profile.supports_daemon {
                    println!("    Daemon support: yes");
                }
                println!();
            }
            Ok(())
        }

        Commands::Call { tool, args, result: unwrap } => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config(cli.config.clone())?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            // Require daemon to be running
            let daemon_mgr = DaemonManager::new(&server_name);
            if !daemon_mgr.is_running().unwrap_or(false) {
                return Err(daemon_unavailable_error(
                    &server_name,
                    profile,
                    cli.config.clone(),
                ));
            }

            // Parse tool arguments
            let json_str = if args == "-" {
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .context("Failed to read JSON from stdin")?;
                buffer
            } else {
                args
            };

            let args_json: Value =
                serde_json::from_str(&json_str).context("Invalid JSON arguments")?;

            let result = call_via_daemon(&server_name, &tool, args_json)?;

            if unwrap {
                for doc in unwrap_tool_result(&result) {
                    println!("{}", doc);
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Ok(())
        }

        Commands::ListTools => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config(cli.config.clone())?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            // Require daemon to be running
            let daemon_mgr = DaemonManager::new(&server_name);
            if !daemon_mgr.is_running().unwrap_or(false) {
                return Err(daemon_unavailable_error(
                    &server_name,
                    profile,
                    cli.config.clone(),
                ));
            }

            let result = list_tools_via_daemon(&server_name)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }

        Commands::Shell => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config(cli.config.clone())?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            // Require daemon to be running
            let daemon_mgr = DaemonManager::new(&server_name);
            if !daemon_mgr.is_running().unwrap_or(false) {
                return Err(daemon_unavailable_error(
                    &server_name,
                    profile,
                    cli.config.clone(),
                ));
            }

            let project = get_project_path();
            println!("MCP Shell ({}) - Project: {}", server_name, project);
            println!("Commands: call <tool> [json], list-tools, exit");
            println!();

            loop {
                print!("mcp> ");
                std::io::stdout().flush()?;

                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let input = input.trim();

                if input.is_empty() {
                    continue;
                }

                if input == "exit" || input == "quit" {
                    break;
                }

                if input == "list-tools" {
                    match list_tools_via_daemon(&server_name) {
                        Ok(result) => println!("{}", serde_json::to_string_pretty(&result)?),
                        Err(e) => eprintln!("Error: {}", e),
                    }
                    continue;
                }

                // Parse "call tool_name args" format
                if let Some(rest) = input.strip_prefix("call ") {
                    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                    if !parts.is_empty() {
                        let tool = parts[0];
                        let args = parts.get(1).unwrap_or(&"{}");

                        match serde_json::from_str(args) {
                            Ok(args_json) => match call_via_daemon(&server_name, tool, args_json) {
                                Ok(result) => {
                                    println!("{}", serde_json::to_string_pretty(&result)?)
                                }
                                Err(e) => eprintln!("Error: {}", e),
                            },
                            Err(e) => eprintln!("Invalid JSON args: {}", e),
                        }
                    } else {
                        eprintln!("Usage: call <tool_name> [json_args]");
                    }
                } else {
                    eprintln!("Usage: call <tool_name> [json_args] | list-tools | exit");
                }
            }

            println!("Goodbye!");
            Ok(())
        }

        Commands::StartDaemon => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required")
            })?;

            let config = load_server_config(cli.config.clone())?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            let extra_args = if let Some(args_str) = &cli.server_args {
                Some(serde_json::from_str::<Vec<String>>(args_str)
                    .context("Invalid JSON in --server-args")?)
            } else {
                None
            };

            // Resolve the config file once and hand it to the daemon, made
            // absolute so it does not depend on the daemon's working directory.
            let config_path = get_config_path(cli.config.clone())?;
            let config_path = std::path::absolute(&config_path).unwrap_or(config_path);

            let daemon_mgr = DaemonManager::new(&server_name);
            daemon_mgr.start(profile, extra_args, &config_path)?;
            Ok(())
        }

        Commands::StopDaemon => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required")
            })?;

            let daemon_mgr = DaemonManager::new(&server_name);
            daemon_mgr.stop()?;
            Ok(())
        }

        Commands::DaemonStatus => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required")
            })?;

            let daemon_mgr = DaemonManager::new(&server_name);
            daemon_mgr.status()?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(supports_daemon: bool) -> ServerProfile {
        ServerProfile {
            command: vec!["true".to_string()],
            default_args: vec![],
            supports_daemon,
            description: String::new(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn disabled_daemon_names_the_config_field_and_file() {
        let err = daemon_unavailable_error(
            "demo",
            &profile(false),
            Some(PathBuf::from("/tmp/servers.json")),
        )
        .to_string();

        assert!(err.contains("\"supports_daemon\": true"), "{err}");
        assert!(err.contains("missing or false"), "{err}");
        assert!(err.contains("/tmp/servers.json"), "{err}");
        assert!(err.contains(&format!("cd {}", get_project_path())), "{err}");
        assert!(!err.contains("Daemon is not running"), "{err}");
    }

    #[test]
    fn enabled_daemon_still_says_not_running() {
        let err = daemon_unavailable_error(
            "demo",
            &profile(true),
            Some(PathBuf::from("/tmp/servers.json")),
        )
        .to_string();

        assert!(err.contains("Daemon is not running"), "{err}");
        assert!(err.contains(&format!("cd {}", get_project_path())), "{err}");
        assert!(err.contains("mcp-valve --server demo start-daemon"), "{err}");
    }

    #[test]
    fn unwrap_prefers_structured_content_of_any_json_type() {
        let object = json!({
            "content": [{"type": "text", "text": "summary"}],
            "structuredContent": {"a": 1}
        });
        assert_eq!(unwrap_tool_result(&object), vec!["{\n  \"a\": 1\n}"]);

        let array = json!({
            "content": [{"type": "text", "text": "summary"}],
            "structuredContent": [1, 2]
        });
        assert_eq!(unwrap_tool_result(&array), vec!["[\n  1,\n  2\n]"]);
    }

    #[test]
    fn unwrap_treats_null_structured_content_as_absent() {
        let result = json!({
            "content": [{"type": "text", "text": "real answer"}],
            "structuredContent": null
        });
        assert_eq!(unwrap_tool_result(&result), vec!["real answer"]);
    }

    #[test]
    fn unwrap_prints_text_exactly_as_sent() {
        // Key order, a 30-digit integer, exponent notation and bare scalars
        // must all survive: none of them is parsed or re-serialised.
        let json_text = r#"{"z":1,"a":123456789012345678901234567890,"e":1E5}"#;
        let result = json!({
            "content": [
                {"type": "text", "text": json_text},
                {"type": "text", "text": "plain words\nsecond line"},
                {"type": "text", "text": "42"}
            ]
        });
        assert_eq!(
            unwrap_tool_result(&result),
            vec![json_text, "plain words\nsecond line", "42"]
        );
    }

    #[test]
    fn unwrap_prints_non_text_items_as_json_and_hides_nothing() {
        let image = json!({"type": "image", "data": "AAAA", "mimeType": "image/png"});
        let result = json!({ "content": [image.clone()] });
        assert_eq!(
            unwrap_tool_result(&result),
            vec![serde_json::to_string_pretty(&image).unwrap()]
        );

        let empty = json!({ "content": [] });
        assert!(unwrap_tool_result(&empty).is_empty());

        let neither = json!({ "resultType": "input_required", "inputRequests": {} });
        assert_eq!(
            unwrap_tool_result(&neither),
            vec![serde_json::to_string_pretty(&neither).unwrap()]
        );
    }

    #[test]
    fn call_result_flag_is_off_by_default_and_parses() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mcp-valve", "--server", "s", "call", "t"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().command {
                Commands::Call { result, .. } => result,
                _ => panic!("expected the call subcommand"),
            }
        };

        assert!(!parse(&[]));
        assert!(parse(&["--result"]));
        assert!(parse(&["--args", "{}", "--result"]));
    }

    fn read_with(input: &str, id: u64) -> (Result<Value>, String) {
        let mut reader = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut written: Vec<u8> = Vec::new();
        let result = read_response(&mut reader, &mut written, &json!(id));
        (result, String::from_utf8(written).unwrap())
    }

    #[test]
    fn read_response_skips_notifications_and_other_output() {
        let input = concat!(
            "\n",
            "server starting, not json\n",
            "[1, 2, 3]\n",
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"result":{"stale":true}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
            "\n",
        );
        let (result, written) = read_with(input, 7);

        assert_eq!(result.unwrap()["result"], json!({"ok": true}));
        assert!(written.is_empty(), "nothing should be sent back: {written}");
    }

    #[test]
    fn read_response_answers_server_requests_before_continuing() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":"srv-1","method":"ping"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"roots/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
            "\n",
        );
        let (result, written) = read_with(input, 7);

        // The server request reuses id 7; it must not be taken for the response.
        assert_eq!(result.unwrap()["result"], json!({"ok": true}));

        let replies: Vec<Value> = written
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], json!({"jsonrpc": "2.0", "id": "srv-1", "result": {}}));
        assert_eq!(replies[1]["id"], json!(7));
        assert_eq!(replies[1]["error"]["code"], json!(-32601));
    }

    #[test]
    fn read_response_returns_error_responses_and_reports_eof() {
        let (result, _) = read_with(
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32602,\"message\":\"bad\"}}\n",
            7,
        );
        assert_eq!(result.unwrap()["error"]["code"], json!(-32602));

        // An error with a null id (the server could not read the request)
        // belongs to the request in flight.
        let (result, _) = read_with(
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}\n",
            7,
        );
        assert_eq!(result.unwrap()["error"]["code"], json!(-32700));

        // A result with a null id is not a response to anything: skipped.
        let (result, _) = read_with(
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n",
            7,
        );
        assert_eq!(result.unwrap()["result"], json!({"ok": true}));

        let (result, _) = read_with(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n",
            7,
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("closed its output"), "{err}");
    }

    #[test]
    fn internal_daemon_args_always_carry_the_config_path() {
        let args = internal_daemon_args("demo", None, Path::new("/tmp/other.json")).unwrap();
        assert_eq!(
            args,
            ["__internal_daemon", "--server", "demo", "--config", "/tmp/other.json"]
        );

        let extra = vec!["--gui".to_string()];
        let args =
            internal_daemon_args("demo", Some(&extra), Path::new("/tmp/other.json")).unwrap();
        assert_eq!(
            args,
            [
                "__internal_daemon",
                "--server",
                "demo",
                "--config",
                "/tmp/other.json",
                "--server-args",
                r#"["--gui"]"#
            ]
        );
    }

    #[test]
    fn omitted_supports_daemon_field_is_reported_as_disabled() {
        let config: ServerConfig =
            serde_json::from_str(r#"{"demo": {"command": ["true"]}}"#).unwrap();
        let profile = config.servers.get("demo").unwrap();
        assert!(!profile.supports_daemon);

        let err =
            daemon_unavailable_error("demo", profile, Some(PathBuf::from("/tmp/servers.json")))
                .to_string();

        assert!(err.contains("missing or false"), "{err}");
        assert!(!err.contains("Daemon is not running"), "{err}");
    }
}
