# mcp-cli

Unified MCP (Model Context Protocol) client for any MCP server.

## Critical Guardrails

- **MANDATORY: Check schema before EVERY tool call**: Run `list-tools` and inspect `inputSchema` before calling any tool. Never guess field names.
- **Field names are case-sensitive**: Use exactly what `inputSchema` shows (e.g., `url` not `URL`, `ref` not `reference`)
- **Missing `--server`**: All commands except `list-servers` require `--server <name>`
- **Daemon directory matters**: `.mcp-profile/` created in CWD. Wrong directory = separate daemon instance
- **JSON array for `--server-args`**: Use `'["--arg1", "--arg2"]'`, not plain strings

## Core Workflow (80% of use cases)

### 1. Check Schema First (Required)

```bash
# Get tool schema
mcp-cli --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | {
    name,
    required: .inputSchema.required,
    properties: (.inputSchema.properties | keys)
  }'
```

### 2. Call Tool

```bash
# Direct JSON
mcp-cli --server <name> call <tool> --args '{"key":"value"}'

# From stdin (for complex JSON)
echo '{"key":"value"}' | mcp-cli --server <name> call <tool> --args -
```

### 3. Daemon Mode (for repeated calls)

```bash
# Start (creates .mcp-profile/<server>/ in CWD)
mcp-cli --server <name> start-daemon

# Calls auto-route through daemon
mcp-cli --server <name> call <tool> --args '{...}'

# Check/stop
mcp-cli --server <name> daemon-status
mcp-cli --server <name> stop-daemon
```

## When to Use Daemon vs STDIO

| Scenario | Mode | Reason |
|----------|------|--------|
| Single tool call | STDIO | No overhead |
| Multiple sequential calls | Daemon | Avoid repeated server startup |
| Browser automation | Daemon | Maintains browser state |
| Stateless tools (e.g., AI chat) | STDIO | No state to preserve |

## Schema Validation Recovery

When you see validation errors like `'<field>' is a required property`:

```bash
# Step 1: List required fields
mcp-cli --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | .inputSchema.required[]'

# Step 2: Get property types
mcp-cli --server <name> list-tools 2>/dev/null | \
  jq '.tools[] | select(.name=="<tool>") | .inputSchema.properties'
```

## Anti-patterns

| Anti-pattern | Why it fails | Correct approach |
|--------------|--------------|------------------|
| Guessing field names | Schema mismatch | Always check `list-tools` first |
| `--server-args "--gui"` | Not JSON array | `--server-args '["--gui"]'` |
| Starting daemon in `/tmp` | State in wrong location | Start from project root |
| Hardcoding tool schemas | Schemas change | Query `list-tools` dynamically |

## Configuration

Location: `~/.claude/scripts/mcp-servers.json`

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
mcp-cli list-servers

# List tools (always do this first)
mcp-cli --server <name> list-tools

# Call tool
mcp-cli --server <name> call <tool> --args '{"key":"value"}'

# Override default args (empty array clears defaults)
mcp-cli --server <name> --server-args '[]' call <tool> --args '{}'

# Interactive shell
mcp-cli --server <name> shell
```

## Publishing to crates.io

### Pre-publish Checklist

1. **Cargo.toml metadata**:
   ```toml
   [package]
   name = "mcp-cli"
   version = "1.0.0"
   edition = "2021"
   license = "MIT"
   repository = "https://github.com/<user>/mcp-cli"
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
| `Server 'X' not found` | Missing config entry | Add to `mcp-servers.json` |
| `does not support daemon mode` | `supports_daemon: false` | Use STDIO or update config |
| `Daemon already running` | Existing instance | `stop-daemon` first or use different CWD |
| `Failed to connect to daemon` | Daemon crashed or wrong CWD | Check `daemon-status`, restart if needed |
