# mcp-valve

Unified MCP (Model Context Protocol) client for any MCP server.

**Install:** `cargo install mcp-valve`

## Critical Guardrails

- **Daemon required**: All tool operations (`call`, `list-tools`, `shell`) require daemon to be running first
- **Project directory matters**: Daemon state is tied to the directory where `start-daemon` was run. Wrong directory = "daemon not running" error
- **MANDATORY: Check schema before EVERY tool call**: Run `list-tools` and inspect `inputSchema` before calling any tool. Never guess field names.
- **Field names are case-sensitive**: Use exactly what `inputSchema` shows (e.g., `url` not `URL`, `ref` not `reference`)
- **Missing `--server`**: All commands except `list-servers` require `--server <name>`
- **JSON array for `--server-args`**: Use `'["--arg1", "--arg2"]'`, not plain strings

## Core Workflow (80% of use cases)

### 1. Start Daemon (Required First)

```bash
# Navigate to your project directory
cd /path/to/your/project

# Start daemon (creates .mcp-profile/<server>/ in CWD)
mcp-valve --server <name> start-daemon
# Output shows: Project: /path/to/your/project
```

### 2. Check Schema (Required Before Tool Calls)

```bash
# Get tool schema
mcp-valve --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | {
    name,
    required: .inputSchema.required,
    properties: (.inputSchema.properties | keys)
  }'
```

### 3. Call Tool

```bash
# Direct JSON
mcp-valve --server <name> call <tool> --args '{"key":"value"}'

# From stdin (for complex JSON)
echo '{"key":"value"}' | mcp-valve --server <name> call <tool> --args -
```

### 4. Stop Daemon When Done

```bash
mcp-valve --server <name> stop-daemon
```

## Error: Daemon Not Running

When you see this error:

```
Error: Daemon is not running for project '/path/to/project'

Start daemon with:
  cd /path/to/project
  mcp-valve --server <name> start-daemon
```

**Causes:**
1. Daemon was never started
2. You're in a different directory than where daemon was started
3. Daemon crashed (check `.mcp-profile/<server>/daemon.log`)

## Schema Validation Recovery

When you see validation errors like `'<field>' is a required property`:

```bash
# Step 1: List required fields
mcp-valve --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | .inputSchema.required[]'

# Step 2: Get property types
mcp-valve --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | .inputSchema.properties'
```

## Anti-patterns

| Anti-pattern | Why it fails | Correct approach |
|--------------|--------------|------------------|
| Calling tool without daemon | Daemon required | Run `start-daemon` first |
| Starting daemon in `/tmp` | State in wrong location | Start from project root |
| Calling from different directory | Different daemon instance | Always use same CWD |
| Guessing field names | Schema mismatch | Always check `list-tools` first |
| `--server-args "--gui"` | Not JSON array | `--server-args '["--gui"]'` |
| Hardcoding tool schemas | Schemas change | Query `list-tools` dynamically |

## Configuration

Config file search order:
1. `--config <path>` CLI flag
2. `MCP_VALVE_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/mcp-valve/servers.json`
4. `~/.config/mcp-valve/servers.json`
5. `~/.claude/scripts/mcp-servers.json` (legacy)

```json
{
  "server-name": {
    "command": ["executable", "arg1"],
    "default_args": ["--default-flag"],
    "supports_daemon": true,
    "description": "Server description",
    "env": {"KEY": "value"}
  }
}
```

### Template Variables in Args

| Variable | Value |
|----------|-------|
| `{profile_dir}` | `.mcp-profile/<server-name>` |
| `{cwd}` | Current working directory |
| `{pid}` | Process ID |

## Quick Reference

```bash
# List servers
mcp-valve list-servers

# Start daemon (REQUIRED FIRST)
cd /path/to/project
mcp-valve --server <name> start-daemon

# List tools
mcp-valve --server <name> list-tools

# Call tool
mcp-valve --server <name> call <tool> --args '{"key":"value"}'

# Check daemon status
mcp-valve --server <name> daemon-status

# Interactive shell
mcp-valve --server <name> shell

# Stop daemon
mcp-valve --server <name> stop-daemon
```

## Publishing to crates.io

### Pre-publish Checklist

1. **Cargo.toml metadata**:
   ```toml
   [package]
   name = "mcp-valve"
   version = "1.0.0"
   edition = "2021"
   license = "MIT"
   repository = "https://github.com/yonaka15/mcp-valve"
   keywords = ["mcp", "cli", "model-context-protocol"]
   categories = ["command-line-utilities", "development-tools"]
   ```

2. **Required files**:
   - `README.md` - Displayed on crates.io
   - `LICENSE` or `LICENSE-MIT` - Required for `license` field
   - `.gitignore` - Exclude `target/`, `Cargo.lock` (for libraries)

3. **Verify package**:
   ```bash
   cargo publish --dry-run
   cargo package --list  # Check included files
   ```

4. **Login and publish**:
   ```bash
   cargo login  # Enter API token from crates.io
   cargo publish
   ```

### Platform Limitation

This crate uses Unix-specific APIs (`nix` crate). Add to Cargo.toml:

```toml
[target.'cfg(unix)'.dependencies]
nix = { version = "0.30", features = ["process", "signal", "fs"] }
```

Or document Unix-only support in README.

## Error Messages

| Error | Cause | Fix |
|-------|-------|-----|
| `Daemon is not running for project...` | Daemon not started or wrong CWD | Start daemon in correct directory |
| `Server 'X' not found` | Missing config entry | Add to `mcp-servers.json` |
| `does not support daemon mode` | `supports_daemon: false` | Update config to `true` |
| `Daemon already running` | Existing instance | `stop-daemon` first or use different CWD |
| `Failed to connect to daemon` | Daemon crashed | Check `daemon-status`, restart if needed |
