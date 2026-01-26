# rusty-restart-claude

A Rust MCP proxy for hot-reloading MCP servers during development with Claude Code.

## Problem

When using Claude Code to develop an MCP server that Claude is connected to, you need a way to restart the server to pick up code changes without losing your Claude Code session.

## Solution

This tool wraps your MCP server as a transparent proxy, injecting two additional tools:

- **`restart_server`** - Claude can call this to restart the wrapped server and pick up code changes
- **`server_status`** - Check if the server is running, uptime, restart count

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
# Binary at target/release/rusty-restart-claude
```

## Usage

### Configure in Claude Code

Add to your `~/.claude.json` or project's `.claude/settings.local.json`:

```json
{
  "mcpServers": {
    "my-server": {
      "command": "rusty-restart-claude",
      "args": ["wrap", "--name", "my-server", "--", "node", "./dist/index.js"]
    }
  }
}
```

### CLI

```bash
# Wrap an MCP server
rusty-restart-claude wrap --name <server-name> -- <command> [args...]

# Example: wrap a Node.js MCP server
rusty-restart-claude wrap --name my-mcp -- node ./dist/index.js

# Example: wrap a Python MCP server
rusty-restart-claude wrap --name my-mcp -- python -m my_mcp_server
```

### In Claude Code

After configuring, Claude will see the injected tools:

```
> What tools do you have from my-server?

I have access to:
- restart_server: Restart the wrapped MCP server to pick up code changes
- server_status: Check server status (running, uptime, restart count)
- [... your server's tools ...]
```

When you make changes to your MCP server's code:

```
> I've updated the tool implementation. Please restart the server.

[Calls restart_server tool]

Server 'my-server' restarted successfully.
PID: 12345
Restart count: 1
```

## How It Works

```
Claude Code <--stdio--> rusty-restart-claude <--stdio--> Your MCP Server
                              |
                              +-- Injects restart_server and server_status tools
                              +-- Maintains connection during restart
                              +-- Replays initialize request after restart
```

1. Acts as a transparent MCP proxy between Claude Code and your server
2. Intercepts `tools/list` responses to inject additional tools
3. Handles `restart_server` calls by killing and respawning the wrapped process
4. Caches the `initialize` request and replays it after restart
5. All other messages pass through unchanged

## License

MIT
