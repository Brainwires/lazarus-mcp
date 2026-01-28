# aegis-mcp

A universal agent supervisor with hot-reload support, multi-agent orchestration, and network monitoring for AI coding agents.

## Features

- **Hot-Reload** - Restart your AI agent to pick up MCP server changes without losing terminal context
- **Multi-Agent Pool** - Spawn background agents to work on tasks autonomously
- **Network Monitoring** - Track all network connections made by your agent (LD_PRELOAD or network namespace)
- **Process Isolation** - MCP server auto-injection only affects the wrapped process
- **Privilege Safety** - Automatically drops root privileges before spawning agents
- **Multi-Agent Support** - Works with Claude Code, Cursor, Aider, and more

## Quick Start

```bash
# Install
git clone https://github.com/Brainwires/aegis-mcp.git
cd aegis-mcp
cargo install --path .

# Build the hooks library (for network monitoring and MCP injection)
cargo build -p aegis-hooks --release

# Run Claude Code through the wrapper
aegis-mcp claude
```

That's it! The wrapper automatically:
- Injects itself as an MCP server (process-isolated, no file modifications)
- Adds permission-skipping flags appropriate for the agent
- Uses `--continue` on restarts to preserve session context

## Architecture

```
Terminal
  └── aegis-mcp claude (wrapper)
        │
        ├── Environment: LD_PRELOAD=libaegis_hooks.so
        │                AEGIS_MCP_OVERLAY=/tmp/aegis-mcp-overlay-{pid}.json
        │                AEGIS_NETMON_LOG=/tmp/aegis-netmon-{pid}.jsonl (if --netmon)
        │
        └── claude --dangerously-skip-permissions
              │
              └── MCP servers (from overlay .mcp.json)
                    └── aegis-mcp --mcp-server
                          ├── restart_claude
                          ├── agent_spawn/list/status/await/stop
                          ├── agent_pool_stats
                          └── netmon_status/log
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
aegis-mcp claude
aegis-mcp claude --continue
aegis-mcp claude -p "Help me with..."

# Aider
aegis-mcp aider
aegis-mcp aider --model gpt-4

# Cursor
aegis-mcp cursor
```

### With Network Monitoring

```bash
# LD_PRELOAD mode (works as normal user)
aegis-mcp claude --netmon

# Network namespace mode (requires root, better isolation)
sudo aegis-mcp claude --netmon=netns
```

### Options

| Option | Description |
|--------|-------------|
| `--netmon` | Enable network monitoring (LD_PRELOAD mode) |
| `--netmon=preload` | Force LD_PRELOAD mode |
| `--netmon=netns` | Force network namespace mode (requires root) |
| `--no-inject-mcp` | Don't auto-inject aegis-mcp as an MCP server |
| `--keep-root` | Stay root instead of dropping privileges |

### Privilege Handling

When run with sudo, aegis-mcp drops privileges before spawning the agent:

```bash
sudo aegis-mcp claude              # Sets up netns, then drops to original user
sudo aegis-mcp claude --keep-root  # Stays root (for debugging only)
```

## MCP Tools (13 total)

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

### Network Monitoring Tools

Monitor all network connections made by the wrapped agent.

#### netmon_status

Get network monitoring status and statistics including:
- Total connections
- Unique addresses
- Bytes sent/received
- Top connection targets

#### netmon_log

Get recent network events from the monitoring log.

```
Parameters:
- count (optional): Number of recent events to return (default: 20)
```

#### netmon_namespace_list

List all aegis network namespaces (for `--netmon=netns` mode).

#### netmon_namespace_cleanup

Clean up stale network namespaces after crashes. Requires root.

## How It Works

### Hot-Reload

1. User starts agent via `aegis-mcp claude`
2. Wrapper spawns agent with LD_PRELOAD hooks
3. Hooks intercept `.mcp.json` reads, redirecting to overlay file
4. Agent loads aegis-mcp as an MCP server
5. When `restart_claude` is called:
   - MCP server writes signal file to `/tmp/aegis-mcp-{pid}`
   - Wrapper detects signal, sends SIGINT → SIGTERM → SIGKILL
   - Agent restarts with `--continue` flag
   - Session context is preserved

### Network Monitoring (LD_PRELOAD)

The `libaegis_hooks.so` library intercepts:
- `connect()` - Log connection attempts
- `send()`/`sendto()` - Log bytes sent
- `recv()`/`recvfrom()` - Log bytes received
- `close()` - Log connection closes

All events are logged to `/tmp/aegis-netmon-{pid}.jsonl` in JSONL format.

### Network Monitoring (Namespace)

When using `--netmon=netns`:
1. Creates isolated network namespace `aegis-{pid}`
2. Sets up veth pair for connectivity
3. Configures NAT for internet access
4. Agent runs inside the namespace
5. All traffic is isolated and can be captured

### Process-Isolated MCP Injection

Instead of modifying `.mcp.json` on disk (which would affect all processes), aegis-mcp uses LD_PRELOAD filesystem interception:

1. Creates temp overlay file `/tmp/aegis-mcp-overlay-{pid}.json`
2. Sets `LD_PRELOAD=libaegis_hooks.so`
3. Hooks intercept `open()`/`openat()` calls
4. Reads of `.mcp.json` are redirected to the overlay
5. Only the wrapped process sees the injected MCP server

Benefits:
- No file modifications
- Multiple aegis-mcp instances work independently
- Automatic cleanup on exit

## Building

```bash
# Build everything
cargo build --release
cargo build -p aegis-hooks --release

# Install binaries
cp target/release/aegis-mcp ~/.local/bin/
cp target/release/libaegis_hooks.so ~/.local/lib/
```

The hooks library should be placed either:
- Next to the `aegis-mcp` binary
- In `./target/release/` (for development)
- In `/usr/local/lib/` or `/usr/lib/`

## Configuration

### Manual MCP Configuration

If you prefer to configure MCP manually instead of auto-injection:

```json
{
  "mcpServers": {
    "aegis-mcp": {
      "command": "aegis-mcp",
      "args": ["--mcp-server"]
    }
  }
}
```

### Shell Alias

```bash
alias claude='aegis-mcp claude'
alias claude-monitor='aegis-mcp claude --netmon'
```

## Platform Support

Linux only (uses `/proc` filesystem, LD_PRELOAD, and network namespaces).

## License

MIT
