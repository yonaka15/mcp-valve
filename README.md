# mcp-valve

A unified MCP (Model Context Protocol) client that works with any MCP server through configurable profiles.

## Design Philosophy

**This tool is a thin wrapper around the MCP protocol.**

- Tools are called by providing JSON arguments that conform to the `inputSchema` from `tools/list`
- No validation or field name conversion occurs - the MCP server's schema is the source of truth
- If `tools/list` shows a field named `X`, use `X` exactly as-is in your JSON
- **Daemon mode is required** for all tool operations - this ensures consistent state management

## Installation

```bash
cargo install mcp-valve
# Or build from source:
cargo build --release
```

## Quick Start

```bash
# List configured servers
mcp-valve list-servers

# Start daemon (REQUIRED before any tool operations)
cd /path/to/your/project
mcp-valve --server playwright start-daemon

# List tools from a server
mcp-valve --server playwright list-tools

# Call a tool
mcp-valve --server playwright call browser_navigate --args '{"url":"https://example.com"}'

# Read args from stdin
echo '{"url":"https://example.com"}' | mcp-valve --server playwright call browser_navigate --args -

# Check daemon status
mcp-valve --server playwright daemon-status

# Stop daemon when done
mcp-valve --server playwright stop-daemon
```

## Configuration

Config file is searched in the following order:

1. `--config <path>` CLI flag
2. `MCP_VALVE_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/mcp-valve/servers.json`
4. `~/.config/mcp-valve/servers.json`
5. `~/.claude/scripts/mcp-servers.json` (legacy)

Example config:

```json
{
  "playwright": {
    "command": ["npx", "@playwright/mcp@latest"],
    "default_args": ["--headless"],
    "supports_daemon": true,
    "description": "Playwright browser automation",
    "env": {}
  },
  "zen": {
    "command": ["/path/to/python"],
    "default_args": ["/path/to/server.py"],
    "supports_daemon": true,
    "description": "Zen MCP multi-AI model integration",
    "env": {}
  }
}
```

### Profile Options

| Field | Type | Description |
|-------|------|-------------|
| `command` | `string[]` | Command and initial args to start the server |
| `default_args` | `string[]` | Default arguments (overridden by `--server-args`) |
| `supports_daemon` | `bool` | Enable daemon mode (required for tool operations) |
| `description` | `string` | Human-readable description |
| `env` | `object` | Environment variables to set |

### Template Variables

Arguments support template expansion:

| Variable | Expands To |
|----------|------------|
| `{profile_dir}` | `.mcp-profile/<server-name>` |
| `{pid}` | Current process ID |
| `{cwd}` | Current working directory |

## Commands

| Command | Description |
|---------|-------------|
| `list-servers` | Show all configured servers |
| `start-daemon` | Start persistent daemon (required first) |
| `list-tools` | List available tools from server |
| `call <tool>` | Call a tool with JSON arguments |
| `shell` | Interactive REPL mode |
| `daemon-status` | Check daemon status |
| `stop-daemon` | Stop running daemon |

## Daemon Mode

Daemon mode is **required** for all tool operations (`call`, `list-tools`, `shell`). This ensures:

- Consistent state management across operations
- Better performance for repeated calls
- Clear project context (daemon is tied to directory)

```bash
# Start daemon in your project directory
cd /path/to/your/project
mcp-valve --server playwright start-daemon
# Output:
# Project: /path/to/your/project
# Profile: .mcp-profile/playwright
# Starting MCP daemon for 'playwright'...
# Daemon started (PID: 12345)
# Socket: /tmp/.mcp/playwright-12345.sock

# All operations now work
mcp-valve --server playwright list-tools
mcp-valve --server playwright call browser_navigate --args '{"url":"https://example.com"}'

# Check status
mcp-valve --server playwright daemon-status
# Output:
# Project: /path/to/your/project
# Server: playwright
# Profile: .mcp-profile/playwright
# Daemon is running
#   PID: 12345
#   Socket: /tmp/.mcp/playwright-12345.sock

# Stop daemon
mcp-valve --server playwright stop-daemon
```

**Directory matters**: Daemon state is stored in `.mcp-profile/` in the current working directory. Different directories = separate daemon instances.

### Error: Daemon Not Running

If you try to call a tool without starting the daemon:

```
Error: Daemon is not running for project '/path/to/project'

Start daemon with:
  cd /path/to/project
  mcp-valve --server playwright start-daemon
```

## Technical Details

- **Protocol**: MCP 2025-06-18 (JSON-RPC 2.0)
- **Transport**: Unix socket (daemon mode)
- **Platform**: Unix-like systems (uses nix crate for process management)

## Dependencies

- `clap` - CLI parsing
- `serde` / `serde_json` - JSON serialization
- `anyhow` - Error handling
- `nix` - Unix system calls (umask, setsid, signals)

## License

MIT
