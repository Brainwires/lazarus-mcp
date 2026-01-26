# rusty-restart-claude

A standalone Rust MCP server that can restart Claude Code to reload all MCP servers during development.

## Problem

When using Claude Code to develop an MCP server, you need a way to restart Claude Code so it reconnects to the updated server. There's no built-in way to programmatically trigger this.

## Solution

This MCP server provides two tools:

- **`restart_claude`** - Restarts Claude Code entirely, which reconnects to all MCP servers with fresh code
- **`server_status`** - Shows status info about Claude Code and this server

When `restart_claude` is called, it:
1. Forks a detached daemon process (survives Claude Code's death)
2. The daemon waits briefly, then kills Claude Code
3. The daemon restarts Claude Code in the same working directory
4. The daemon exits

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
# Binary at target/release/rusty-restart-claude
```

## Configuration

Add to your `~/.claude.json`:

```json
{
  "mcpServers": {
    "rusty-restart-claude": {
      "type": "stdio",
      "command": "/path/to/rusty-restart-claude",
      "args": [],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

## Usage

After configuring, Claude will have access to two tools:

```
> What tools do you have from rusty-restart-claude?

I have access to:
- restart_claude: Restart Claude Code to reconnect all MCP servers
- server_status: Get status information about this MCP server and Claude Code
```

When you make changes to an MCP server's code:

```
> I've updated the brainwires MCP server code. Please restart Claude Code to pick up the changes.

[Calls restart_claude tool]

Restart initiated!

Claude Code (PID 12345) will restart in 500ms.
Working directory: /home/user/dev/my-project

This session will end. A new Claude Code session will start automatically.
```

## Tools

### restart_claude

Restarts Claude Code to reconnect all MCP servers.

**Parameters:**
- `delay_ms` (optional, integer): Delay before restarting in milliseconds. Default: 500

### server_status

Returns status information:
- `server_pid`: This MCP server's process ID
- `claude_code_pid`: Claude Code's process ID
- `claude_code_exe`: Path to Claude Code executable
- `working_directory`: Claude Code's current working directory

## How It Works

```
┌─────────────────────────────────────────────────────────────┐
│                      Claude Code                             │
│  ┌──────────────────┐  ┌──────────────────┐                 │
│  │ brainwires MCP   │  │ rusty-restart    │  ... other MCPs │
│  │ (your server)    │  │ (this server)    │                 │
│  └──────────────────┘  └────────┬─────────┘                 │
└─────────────────────────────────┼───────────────────────────┘
                                  │
                     calls restart_claude
                                  │
                                  ▼
                    ┌─────────────────────────┐
                    │   Fork detached daemon   │
                    │   (survives parent)      │
                    └─────────────┬───────────┘
                                  │
                                  ▼
                    ┌─────────────────────────┐
                    │   1. Wait delay_ms      │
                    │   2. Kill Claude Code   │
                    │   3. Restart Claude     │
                    │   4. Daemon exits       │
                    └─────────────────────────┘
```

## Platform Support

Currently Linux only (uses `/proc` filesystem for process information).

## License

MIT
