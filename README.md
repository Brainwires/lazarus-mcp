# aegis-mcp

A universal agent supervisor with hot-reload support, multi-agent orchestration, watchdog monitoring, and TUI dashboard for AI coding agents.

## Features

- **Hot-Reload** - Restart your AI agent to pick up MCP server changes without losing terminal context
- **Watchdog Monitoring** - Detect and recover from hung/unresponsive agents automatically
- **TUI Dashboard** - Real-time terminal dashboard showing agent health, network, and pool status
- **Multi-Agent Pool** - Spawn background agents to work on tasks autonomously
- **Network Monitoring** - Track all network connections made by your agent (LD_PRELOAD or network namespace)
- **Process Isolation** - MCP server auto-injection only affects the wrapped process
- **Privilege Safety** - Automatically drops root privileges before spawning agents
- **Multi-Agent Support** - Works with Claude Code, Cursor, Aider, and more
- **Version Tracking** - Embedded build timestamps and git hashes to detect stale builds

## Quick Start

```bash
# Clone and install
git clone https://github.com/Brainwires/aegis-mcp.git
cd aegis-mcp
make install

# Run Claude Code through the wrapper
aegis-mcp claude
```

That's it! The wrapper automatically:
- Injects itself as an MCP server (process-isolated, no file modifications)
- Adds permission-skipping flags appropriate for the agent
- Uses `--continue` on restarts to preserve session context
- Monitors agent health and auto-restarts on lockup

## Architecture

```
Terminal
  └── aegis-mcp claude (wrapper)
        │
        ├── Watchdog (monitors health, detects lockups)
        ├── Shared State (/tmp/aegis-mcp-state-{pid})
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
                          ├── watchdog_status/configure/disable/ping
                          ├── agent_spawn/list/status/await/stop
                          ├── agent_pool_stats
                          └── netmon_status/log

Second Terminal (optional)
  └── aegis-mcp --dashboard
        └── TUI showing real-time status from shared state
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

### TUI Dashboard

Monitor a running wrapper with the TUI dashboard:

```bash
# In terminal 1: Run the agent
aegis-mcp claude

# In terminal 2: Open dashboard (auto-detects running wrapper)
aegis-mcp --dashboard

# Or specify a wrapper PID
aegis-mcp --dashboard 12345
```

Dashboard panels:
- **Primary Agent** - Status, PID, uptime, restarts, health state
- **System** - Memory and CPU usage bars
- **Agent Pool** - Background agents and their tasks
- **Network Activity** - Connections, traffic, top targets
- **File Locks** - Currently held locks
- **Log** - Event log with timestamps

Keybindings:
- `q` / `Esc` - Quit dashboard
- `h` / `?` - Show help
- `Tab` / `Shift+Tab` - Switch panels
- `r` - Restart agent
- `j` / `k` or arrows - Scroll log

### With Network Monitoring

```bash
# LD_PRELOAD mode (works as normal user)
aegis-mcp claude --netmon

# Network namespace mode (requires root, better isolation)
sudo aegis-mcp claude --netmon=netns
```

### Watchdog Configuration

```bash
# Custom timeout (default: 60 seconds)
aegis-mcp claude --watchdog-timeout=120

# Disable watchdog
aegis-mcp claude --no-watchdog
```

### Options

| Option | Description |
|--------|-------------|
| `--version`, `-V` | Show version info for binary and hooks library |
| `--dashboard [pid]` | Run TUI dashboard (monitor running wrapper) |
| `--watchdog-timeout=<secs>` | Heartbeat timeout in seconds (default: 60) |
| `--no-watchdog` | Disable watchdog monitoring |
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

## MCP Tools (17 total)

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

### Watchdog Tools

Monitor and control the watchdog system that detects hung processes.

#### watchdog_status

Get current health status including:
- Process state (Active, Idle, Unresponsive, HighResource)
- Uptime and last activity time
- Memory and CPU usage
- Pending actions

#### watchdog_configure

Configure watchdog settings at runtime.

```
Parameters:
- enabled (optional): Enable or disable watchdog
- heartbeat_timeout_secs (optional): Seconds without activity before unresponsive
- lockup_action (optional): "warn", "restart", "restart_with_backoff", "kill", "notify_and_wait"
- max_memory_mb (optional): Maximum memory usage before triggering action
```

#### watchdog_disable

Temporarily disable watchdog for long-running operations.

```
Parameters:
- duration_secs (optional): Seconds to disable (default: 300)
```

#### watchdog_ping

Send a manual heartbeat to reset the activity timer.

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

### Watchdog Monitoring

The watchdog tracks agent activity through multiple signals:
- stdout/stderr output
- MCP tool calls
- Manual pings from the agent

When no activity is detected for the configured timeout (default 60s):
1. Process is marked as Unresponsive
2. After 3 consecutive checks, configured action is taken
3. Default action is to restart the agent automatically

The watchdog can be controlled via MCP tools or temporarily disabled for long operations.

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
# Build and install (recommended)
make install

# Or install system-wide
sudo make install PREFIX=/usr/local

# Manual build
cargo build --workspace --release

# Check versions
aegis-mcp --version
```

### Makefile Targets

| Target | Description |
|--------|-------------|
| `make build` | Build debug binaries |
| `make release` | Build release binaries |
| `make install` | Build and install to `~/.local` |
| `make uninstall` | Remove installed files |
| `make clean` | Remove build artifacts |

The hooks library is installed to `~/.local/lib/` by default. It can also be placed:
- Next to the `aegis-mcp` binary
- In `./target/release/` (for development)
- In `/usr/local/lib/` or `/usr/lib/`

### Version Tracking

Both the main binary and hooks library embed build timestamps and git hashes at compile time. This helps detect stale builds when the two components get out of sync.

```bash
$ aegis-mcp --version
aegis-mcp v0.3.0
  Built: 2026-01-30 09:16:49 UTC
  Git:   6b83cd8

Hooks library: /path/to/libaegis_hooks.so
  Version: 0.1.0 (built 2026-01-30 09:16:45 UTC, git 6b83cd8)
  Built:   2026-01-30 09:16:45 UTC
```

If the hooks library is stale, you'll see a warning:
```
WARNING: Hooks library version mismatch! Binary: 0.3.0 (abc1234), Library: 0.1.0 (old5678).
Consider rebuilding with: cargo build -p aegis-hooks
```

**Tip:** Always use `cargo build --workspace` to build both components together and keep them in sync.

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
alias aegis-dashboard='aegis-mcp --dashboard'
```

## Platform Support

Linux only (uses `/proc` filesystem, LD_PRELOAD, and network namespaces).

## License

MIT
