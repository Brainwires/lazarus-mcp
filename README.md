# rusty-restart-claude

A minimal dual-mode Rust tool that enables restarting Claude Code to reload MCP servers during development.

## The Problem

When using Claude Code to develop an MCP server, you need a way to restart Claude Code so it reconnects to the updated server. The challenge is preserving the session context.

## The Solution

This tool operates in two modes:

1. **Wrapper mode (default)**: Lightweight process supervisor that monitors Claude and can restart it on demand
2. **MCP server mode (`--mcp-server`)**: Runs as an MCP server that signals the wrapper to restart

### Architecture

```
Terminal
  └── rusty-restart-claude (wrapper)     <-- monitors for restart signals
        └── claude --continue [args...]  <-- spawned directly, can be restarted
              └── MCP servers
                    └── rusty-restart-claude --mcp-server  <-- signals wrapper
```

## Installation

```bash
# From source
git clone https://github.com/Brainwires/rusty-restart-claude.git
cd rusty-restart-claude
cargo install --path .
```

## Usage

### Step 1: Configure MCP Server

Add to your `~/.claude.json`:

```json
{
  "mcpServers": {
    "rusty-restart-claude": {
      "type": "stdio",
      "command": "rusty-restart-claude",
      "args": ["--mcp-server"],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

### Step 2: Start Claude via the Wrapper

Instead of running `claude` directly, use:

```bash
# Start Claude through the wrapper
rusty-restart-claude

# Pass any arguments to Claude
rusty-restart-claude --continue
rusty-restart-claude -p "Help me with..."
```

**Tip:** Create a shell alias for convenience:
```bash
alias claude='rusty-restart-claude'
```

**Note:** The wrapper automatically adds `--allow-dangerously-skip-permissions` if not already provided, enabling seamless MCP tool usage.

### Step 3: Use the Restart Tool

Once Claude is running through the wrapper, you can use the `restart_claude` tool:

```
> Please restart Claude to pick up my MCP server changes

[Calls restart_claude tool]

Restart signal sent! Claude will restart momentarily and resume with --continue.
```

## How It Works

1. User starts Claude via `rusty-restart-claude [args...]`
2. Wrapper spawns Claude as a direct child process
3. Claude connects to MCP servers, including `rusty-restart-claude --mcp-server`
4. When `restart_claude` tool is called:
   - MCP server writes a signal file to `/tmp/rusty-restart-claude-{wrapper-pid}`
   - Wrapper detects the signal file (polling every 100ms)
   - Wrapper sends SIGINT to Claude, waits for graceful exit
   - Wrapper restarts Claude with `--continue` to resume the session
   - Terminal is preserved because the wrapper never exits

## Tools

### restart_claude

Signals the wrapper to restart Claude Code.

**Parameters:**
- `reason` (optional, string): Reason for the restart (for logging)
- `prompt` (optional, string): A prompt to pass as a command-line argument on restart, enabling seamless continuation of work

**Example with prompt:**
```
restart_claude(reason: "MCP server updated", prompt: "Continue where we left off - the MCP servers have been reloaded.")
```

This triggers a restart and passes the prompt as a command-line argument to Claude (like `claude --continue "prompt text"`), continuing the workflow without manual intervention.

### server_status

Shows status information about the wrapper and Claude Code process.

**Response includes:**
- `mcp_server_pid`: This MCP server's process ID
- `wrapper_pid`: The wrapper's process ID (if running through wrapper)
- `wrapper_running`: Whether the wrapper is active
- `claude_code_pid`: Claude Code's process ID
- `working_directory`: Current working directory

## Features

- **Minimal overhead** - Simple process supervision without terminal interference
- **Direct spawning** - Claude runs as a regular child process, no PTY complexity
- **Auto-permissions** - Automatically adds `--allow-dangerously-skip-permissions` for seamless MCP tool usage
- **Session continuation** - Restart always uses `--continue` to resume conversation
- **Prompt passing** - Optionally pass a prompt via command-line on restart to continue work seamlessly
- **Simple signaling** - File-based IPC, no complex sockets or daemons
- **Graceful shutdown** - SIGINT (3s) → SIGTERM (2s) → SIGKILL sequence
- **Zero terminal interference** - No emulation layer that could break Claude's display

## Platform Support

Currently Linux only (uses `/proc` filesystem and PTY).

## License

MIT
