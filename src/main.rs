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
//! separate CLI tools for each MCP server, configure servers in `~/.claude/scripts/mcp-servers.json`
//! and use this single client.
//!
//! ## Quick Start
//!
//! ```bash
//! # List configured servers
//! mcp-cli list-servers
//!
//! # List tools from a server
//! mcp-cli --server playwright list-tools
//!
//! # Call a tool
//! mcp-cli --server playwright call browser_navigate --args '{"url":"https://example.com"}'
//!
//! # Daemon mode (for servers that support it)
//! mcp-cli --server playwright start-daemon --server-args '["--gui"]'
//! mcp-cli --server playwright call browser_navigate --args '{"url":"https://example.com"}'  # Uses daemon
//! mcp-cli --server playwright daemon-status
//! mcp-cli --server playwright stop-daemon
//! ```
//!
//! ## Configuration
//!
//! Server profiles are defined in `~/.claude/scripts/mcp-servers.json`:
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
//! - ✅ Auto-fallback (daemon → STDIO)
//!
//! ## Technical Details
//!
//! - **Protocol**: MCP 2025-06-18 (JSON-RPC 2.0)
//! - **Transport**: STDIO / Unix socket (daemon)
//! - **Dependencies**: serde, serde_json, anyhow, clap, nix

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::{umask, Mode};
use nix::unistd::{setsid, Pid};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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

fn load_server_config() -> Result<ServerConfig> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let config_path = PathBuf::from(&home).join(".claude/scripts/mcp-servers.json");

    if !config_path.exists() {
        return Err(anyhow!(
            "Configuration file not found: {}\nCreate it with server profiles.",
            config_path.display()
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
#[command(name = "mcp-cli")]
#[command(about = "Unified MCP CLI - Generic MCP Protocol Client")]
#[command(version = "1.0.0")]
struct Cli {
    /// Server name from config (e.g., playwright, zen)
    #[arg(short, long)]
    server: Option<String>,

    /// Additional server arguments (JSON array, e.g., '["--gui", "--browser", "firefox"]')
    #[arg(long)]
    server_args: Option<String>,

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
                    "name": "mcp-cli",
                    "version": "1.0.0"
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

        let mut line = String::new();
        self.stdout.read_line(&mut line)?;

        let response: Value = serde_json::from_str(line.trim())
            .context("Failed to parse JSON-RPC response")?;

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

        let response = self.send_request(&request)?;
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

                return Err(anyhow!("Tool Error: {}", error_msg));
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
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
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

        eprintln!("Profile: {}", self.pid_file.parent().unwrap().display());
        eprintln!("Starting MCP daemon for '{}'...", self.server_name);

        // Build daemon command
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("__internal_daemon");
        cmd.arg("--server");
        cmd.arg(&self.server_name);

        if let Some(ref args) = extra_args {
            cmd.arg("--server-args");
            cmd.arg(serde_json::to_string(args)?);
        }

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
            return Err(anyhow!("Daemon not running for '{}'", self.server_name));
        }

        let pid_str = fs::read_to_string(&self.pid_file)?;
        let pid: i32 = pid_str.trim().parse()
            .context("Invalid PID in file")?;

        let socket_path = self.get_socket_path().ok();

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
        let profile_dir = self.pid_file.parent().unwrap();
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

fn call_via_daemon(server_name: &str, tool: &str, args: Value) -> Result<Value> {
    let daemon_mgr = DaemonManager::new(server_name);
    let socket_path = daemon_mgr.get_socket_path()
        .context("Failed to get socket path (daemon not started?)")?;

    let mut stream = UnixStream::connect(&socket_path)
        .context("Failed to connect to daemon (is it running?)")?;

    // Set timeouts
    stream.set_read_timeout(Some(Duration::from_secs(30)))
        .context("Failed to set read timeout")?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))
        .context("Failed to set write timeout")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": args
        }
    });

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

// ============================================================================
// Main
// ============================================================================

fn main() -> Result<()> {
    // Handle internal daemon command BEFORE clap parsing
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "__internal_daemon" {
        // Find --server and --server-args by manual parsing
        let server_name = args.iter()
            .position(|a| a == "--server")
            .and_then(|i| args.get(i + 1))
            .ok_or_else(|| anyhow!("__internal_daemon requires --server"))?
            .clone();

        let extra_args = args.iter()
            .position(|a| a == "--server-args")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());

        let config = load_server_config()?;
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
            let config = load_server_config()?;
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

        Commands::Call { tool, args } => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config()?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            // Parse extra server args
            let extra_args = if let Some(args_str) = &cli.server_args {
                Some(serde_json::from_str::<Vec<String>>(args_str)
                    .context("Invalid JSON in --server-args (expected array of strings)")?)
            } else {
                None
            };

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

            // Try daemon first if server supports it, fallback to STDIO
            let daemon_mgr = DaemonManager::new(&server_name);
            let result = if profile.supports_daemon && daemon_mgr.is_running().unwrap_or(false) {
                match call_via_daemon(&server_name, &tool, args_json.clone()) {
                    Ok(result) => result,
                    Err(e) => {
                        eprintln!("Daemon call failed, falling back to STDIO: {}", e);
                        let mut mcp = McpClient::start(profile, extra_args, &server_name)?;
                        mcp.call_tool(&tool, args_json)?
                    }
                }
            } else {
                let mut mcp = McpClient::start(profile, extra_args, &server_name)?;
                mcp.call_tool(&tool, args_json)?
            };

            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }

        Commands::ListTools => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config()?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            let extra_args = if let Some(args_str) = &cli.server_args {
                Some(serde_json::from_str::<Vec<String>>(args_str)
                    .context("Invalid JSON in --server-args (expected array of strings)")?)
            } else {
                None
            };

            let mut mcp = McpClient::start(profile, extra_args, &server_name)?;
            let result = mcp.list_tools()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }

        Commands::Shell => {
            let server_name = cli.server.ok_or_else(|| {
                anyhow!("--server required. Use 'list-servers' to see available servers.")
            })?;

            let config = load_server_config()?;
            let profile = config
                .servers
                .get(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found in config", server_name))?;

            let extra_args = if let Some(args_str) = &cli.server_args {
                Some(serde_json::from_str::<Vec<String>>(args_str)
                    .context("Invalid JSON in --server-args (expected array of strings)")?)
            } else {
                None
            };

            let mut mcp = McpClient::start(profile, extra_args, &server_name)?;
            println!("MCP Shell ({})", server_name);
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
                    match mcp.list_tools() {
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
                            Ok(args_json) => match mcp.call_tool(tool, args_json) {
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

            let config = load_server_config()?;
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

            let daemon_mgr = DaemonManager::new(&server_name);
            daemon_mgr.start(profile, extra_args)?;
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
