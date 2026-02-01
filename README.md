# lazarus-mcp

A universal agent supervisor with hot-reload support, multi-agent orchestration, and TUI dashboard for AI coding agents.

## Features

- **Hot-Reload** - Restart your AI agent to pick up MCP server changes without losing terminal context
- **TUI Dashboard** - Real-time terminal dashboard showing agent status and pool
- **Multi-Agent Pool** - Spawn background agents to work on tasks autonomously
- **Safe MCP Injection** - Auto-injects into `.mcp.json` with backup/restore on exit
- **Privilege Safety** - Automatically drops root privileges before spawning agents
- **Multi-Agent Support** - Works with Claude Code, Cursor, Aider, and more
- **Version Tracking** - Embedded build timestamps and git hashes

## Quick Start

```bash
# Clone and install
git clone https://github.com/Brainwires/lazarus-mcp.git
cd lazarus-mcp
cargo install --path .

# Run Claude Code through the wrapper
lazarus-mcp claude
```

That's it! The wrapper automatically:
- Injects itself as an MCP server into `.mcp.json` (restored on exit)
- Adds permission-skipping flags appropriate for the agent
- Uses `--continue` on restarts to preserve session context

## Architecture

```
Terminal
  └── lazarus-mcp claude (wrapper)
        │
        ├── Modifies .mcp.json (backup at .mcp.json.aegis-backup)
        ├── Shared State (/tmp/lazarus-mcp-state-{pid}.json)
        │
        └── claude --dangerously-skip-permissions
              │
              └── MCP servers (from .mcp.json)
                    └── lazarus-mcp --mcp-server
                          ├── restart_claude
                          └── agent_spawn/list/status/await/stop

On Exit (normal, signal, or crash):
  └── Restores .mcp.json from backup

Second Terminal (optional)
  └── lazarus-mcp --dashboard
        └── TUI showing real-time status
```

## Supported Agents

| Agent | Continue Support | Auto-Permissions |
|-------|-----------------|------------------|
| Claude Code | `--continue` | `--dangerously-skip-permissions` |
| Cursor | - | - |
| Aider | Auto (chat history) | `--yes` |

## Usage

### Basic Usage

```bash
# Claude Code
lazarus-mcp claude
lazarus-mcp claude --continue
lazarus-mcp claude -p "Help me with..."

# Aider
lazarus-mcp aider
lazarus-mcp aider --model gpt-4

# Cursor
lazarus-mcp cursor
```

### TUI Dashboard

Monitor a running wrapper with the TUI dashboard:

```bash
# In terminal 1: Run the agent
lazarus-mcp claude

# In terminal 2: Open dashboard (auto-detects running wrapper)
lazarus-mcp --dashboard

# Or specify a wrapper PID
lazarus-mcp --dashboard 12345
```

Dashboard panels:
- **Primary Agent** - Status, PID, uptime, restarts
- **Agent Pool** - Background agents and their tasks
- **File Locks** - Currently held locks
- **Log** - Event log with timestamps

Keybindings:
- `q` / `Esc` - Quit dashboard
- `h` / `?` - Show help
- `Tab` / `Shift+Tab` - Switch panels
- `r` - Restart agent
- `j` / `k` or arrows - Scroll log

### Options

| Option | Description |
|--------|-------------|
| `--version`, `-V` | Show version info |
| `--dashboard [pid]` | Run TUI dashboard (monitor running wrapper) |
| `--no-inject-mcp` | Don't auto-inject lazarus-mcp as an MCP server |

## MCP Tools

### Hot-Reload Tools

#### restart_claude

Restart the AI coding agent to reconnect all MCP servers.

```
Parameters:
- reason (optional): Reason for the restart (for logging)
- prompt (optional): A prompt to pass as a command-line argument on restart

Example:
restart_claude(reason: "MCP server updated", prompt: "Continue where we left off")
```

#### server_status

Get status information about the wrapper, agent process, and configuration.

### Agent Pool Tools

Spawn and manage background agents that work autonomously on tasks.

#### agent_spawn

Spawn a background agent to work on a task.

```
Parameters:
- description: The task for the agent to work on
- agent_type (optional): "claude", "aider", or "cursor" (default: "claude")
- working_directory (optional): Directory for the agent to work in
- max_iterations (optional): Maximum iterations before stopping

Returns: agent_id
```

#### agent_list

List all active background agents with their status.

#### agent_status

Get detailed status of a specific agent.

```
Parameters:
- agent_id: The ID of the agent to check
```

#### agent_await

Wait for a background agent to complete and get its result.

```
Parameters:
- agent_id: The ID of the agent to wait for
- timeout_secs (optional): Maximum time to wait
```

#### agent_stop

Stop a running background agent.

```
Parameters:
- agent_id: The ID of the agent to stop
```

#### agent_pool_stats

Get statistics about the agent pool (max agents, active, running, completed, failed).

#### agent_file_locks

List all currently held file locks by agents (for coordination).

## How It Works

### Hot-Reload

1. User starts agent via `lazarus-mcp claude`
2. Wrapper backs up `.mcp.json` to `.mcp.json.aegis-backup`
3. Wrapper injects lazarus-mcp into `.mcp.json`
4. Agent spawns and loads lazarus-mcp as an MCP server
5. When `restart_claude` is called:
   - MCP server writes signal file to `/tmp/lazarus-mcp-{pid}`
   - Wrapper detects signal, sends SIGINT → SIGTERM → SIGKILL
   - Agent restarts with `--continue` flag
   - Session context is preserved
6. On exit (normal, signal, or crash), `.mcp.json` is restored from backup

**Note:** The `restart_claude` tool detects if running under the wrapper. If started without the wrapper, it returns an error message explaining how to use lazarus-mcp.

### MCP Server Injection

lazarus-mcp injects itself into `.mcp.json` with automatic backup/restore:

1. On startup, checks for `.mcp.json.aegis-backup` (previous crash recovery)
2. Backs up existing `.mcp.json` to `.mcp.json.aegis-backup`
3. Adds lazarus-mcp server entry to `.mcp.json`
4. Agent spawns and sees the injected MCP server
5. On exit (normal, Ctrl+C, or crash), restores original `.mcp.json`

Safety features:
- Backup file acts as "dirty flag" for crash recovery
- Panic hooks and signal handlers ensure cleanup
- If started without wrapper, `restart_claude` tool detects this and returns helpful error

## Building

```bash
# Build and install to ~/.cargo/bin
cargo install --path .

# Check version
lazarus-mcp --version
```

## Configuration

### Manual MCP Configuration

If you prefer to configure MCP manually instead of auto-injection:

```json
{
  "mcpServers": {
    "lazarus-mcp": {
      "command": "lazarus-mcp",
      "args": ["--mcp-server"]
    }
  }
}
```

### Shell Alias

```bash
alias claude='lazarus-mcp claude'
alias aegis-dashboard='lazarus-mcp --dashboard'
```

## Platform Support

Linux only (uses `/proc` filesystem).

## License

MIT
