# mcp-tap

A unified MCP (Model Context Protocol) client that works with any MCP server through configurable profiles.

## Design Philosophy

**This tool is a thin wrapper around the MCP protocol.**

- Tools are called by providing JSON arguments that conform to the `inputSchema` from `tools/list`
- No validation or field name conversion occurs - the MCP server's schema is the source of truth
- If `tools/list` shows a field named `X`, use `X` exactly as-is in your JSON

## Installation

```bash
cargo install mcp-tap
# Or build from source:
cargo build --release
```

## Quick Start

```bash
# List configured servers
mcp-tap list-servers

# List tools from a server
mcp-tap --server playwright list-tools

# Call a tool
mcp-tap --server playwright call browser_navigate --args '{"url":"https://example.com"}'

# Read args from stdin
echo '{"url":"https://example.com"}' | mcp-tap --server playwright call browser_navigate --args -

# Daemon mode (persistent connection)
mcp-tap --server playwright start-daemon --server-args '["--gui"]'
mcp-tap --server playwright call browser_click --args '{"element":"Submit","ref":"e1"}'
mcp-tap --server playwright daemon-status
mcp-tap --server playwright stop-daemon
```

## Configuration

Server profiles are defined in `~/.claude/scripts/mcp-servers.json`:

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
    "supports_daemon": false,
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
| `supports_daemon` | `bool` | Enable daemon mode for persistent connections |
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
| `list-tools` | List available tools from server |
| `call <tool>` | Call a tool with JSON arguments |
| `shell` | Interactive REPL mode |
| `start-daemon` | Start persistent daemon |
| `stop-daemon` | Stop running daemon |
| `daemon-status` | Check daemon status |

## Daemon Mode

For servers that support it (`supports_daemon: true`), daemon mode keeps a persistent MCP connection:

```bash
# Start daemon (creates .mcp-profile/<server>/ in current directory)
mcp-tap --server playwright start-daemon

# Calls automatically route through daemon
mcp-tap --server playwright call browser_navigate --args '{"url":"https://example.com"}'

# Check status
mcp-tap --server playwright daemon-status

# Stop daemon
mcp-tap --server playwright stop-daemon
```

**Directory matters**: Daemon state is stored in `.mcp-profile/` in the current working directory. Different directories = separate daemon instances.

### Auto-Fallback

When daemon is running, `call` automatically uses it. If daemon fails, falls back to STDIO mode.

## Technical Details

- **Protocol**: MCP 2025-06-18 (JSON-RPC 2.0)
- **Transport**: STDIO (default) / Unix socket (daemon)
- **Platform**: Unix-like systems (uses nix crate for process management)

## Dependencies

- `clap` - CLI parsing
- `serde` / `serde_json` - JSON serialization
- `anyhow` - Error handling
- `nix` - Unix system calls (umask, setsid, signals)

## License

MIT
