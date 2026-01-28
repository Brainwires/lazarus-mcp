# aegis-mcp

A universal agent supervisor with hot-reload support for MCP servers. Wrap any AI coding agent and restart it on demand to reload MCP server changes during development.

## The Problem

When developing MCP servers, you need a way to restart your AI coding agent so it reconnects to the updated server. The challenge is preserving the session context across restarts.

## The Solution

This tool operates in two modes:

1. **Wrapper mode (default)**: Lightweight process supervisor that monitors any AI agent and can restart it on demand
2. **MCP server mode (`--mcp-server`)**: Runs as an MCP server that signals the wrapper to restart

### Supported Agents

| Agent | Continue Support | Auto-Permissions |
|-------|-----------------|------------------|
| Claude Code | `--continue` | `--dangerously-skip-permissions` |
| Cursor | - | - |
| Aider | Auto (chat history) | `--yes` |

### Architecture

```
Terminal
  └── aegis-mcp <agent> (wrapper)     <-- monitors for restart signals
        └── <agent> [args...]         <-- spawned directly, can be restarted
              └── MCP servers
                    └── aegis-mcp --mcp-server  <-- signals wrapper
```

## Installation

```bash
# From source
git clone https://github.com/Brainwires/aegis-mcp.git
cd aegis-mcp
cargo install --path .
```

## Usage

### Step 1: Configure MCP Server

Add to your agent's MCP configuration (e.g., `~/.claude.json` for Claude):

```json
{
  "mcpServers": {
    "aegis-mcp": {
      "type": "stdio",
      "command": "aegis-mcp",
      "args": ["--mcp-server"],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

### Step 2: Start Your Agent via the Wrapper

Instead of running your agent directly, use aegis-mcp:

```bash
# Claude Code
aegis-mcp claude
aegis-mcp claude --continue
aegis-mcp claude -p "Help me with..."

# Aider
aegis-mcp aider
aegis-mcp aider --model gpt-4

# Cursor
aegis-mcp cursor
```

**Tip:** Create a shell alias for convenience:
```bash
alias claude='aegis-mcp claude'
```

**Note:** The wrapper automatically adds permission-skipping flags if the agent supports them.

### Step 3: Use the Restart Tool

Once your agent is running through the wrapper, you can use the `restart_claude` tool:

```
> Please restart to pick up my MCP server changes

[Calls restart_claude tool]

Restart signal sent! Agent will restart momentarily.
```

## How It Works

1. User starts agent via `aegis-mcp <agent> [args...]`
2. Wrapper spawns the agent as a direct child process
3. Agent connects to MCP servers, including `aegis-mcp --mcp-server`
4. When `restart_claude` tool is called:
   - MCP server writes a signal file to `/tmp/aegis-mcp-{wrapper-pid}`
   - Wrapper detects the signal file (polling every 100ms)
   - Wrapper sends SIGINT to agent, waits for graceful exit
   - Wrapper restarts agent with continue flag (if supported)
   - Terminal is preserved because the wrapper never exits

## Tools

### restart_claude

Signals the wrapper to restart the AI coding agent.

**Parameters:**
- `reason` (optional, string): Reason for the restart (for logging)
- `prompt` (optional, string): A prompt to pass as a command-line argument on restart

**Example with prompt:**
```
restart_claude(reason: "MCP server updated", prompt: "Continue where we left off - the MCP servers have been reloaded.")
```

### server_status

Shows status information about the wrapper and agent process.

**Response includes:**
- `mcp_server_pid`: This MCP server's process ID
- `wrapper_pid`: The wrapper's process ID (if running through wrapper)
- `wrapper_running`: Whether the wrapper is active
- `claude_code_pid`: The agent's process ID
- `working_directory`: Current working directory

## Features

- **Multi-agent support** - Works with Claude, Cursor, Aider, and more
- **Minimal overhead** - Simple process supervision without terminal interference
- **Direct spawning** - Agent runs as a regular child process, no PTY complexity
- **Auto-permissions** - Automatically adds permission flags per agent
- **Session continuation** - Restart uses continue flags when supported
- **Prompt passing** - Optionally pass a prompt on restart
- **Simple signaling** - File-based IPC, no complex sockets or daemons
- **Graceful shutdown** - SIGINT (3s) → SIGTERM (2s) → SIGKILL sequence
- **Zero terminal interference** - No emulation layer that could break the agent's display

## Platform Support

Currently Linux only (uses `/proc` filesystem).

## License

MIT
