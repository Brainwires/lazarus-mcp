# rusty-restart-claude

A dual-mode Rust tool that enables restarting Claude Code to reload MCP servers during development.

## The Problem

When using Claude Code to develop an MCP server, you need a way to restart Claude Code so it reconnects to the updated server. The challenge is preserving the terminal and session.

## The Solution

This tool operates in two modes:

1. **Wrapper mode (default)**: Wraps Claude Code, maintaining terminal ownership so it can restart Claude without losing the terminal
2. **MCP server mode (`--mcp-server`)**: Runs as an MCP server that signals the wrapper to restart

### Architecture

```
Terminal
  └── rusty-restart-claude (wrapper)     <-- owns terminal, stays alive
        └── claude --continue [args...]  <-- child process, can be restarted
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

### Step 3: Use the Restart Tool

Once Claude is running through the wrapper, you can use the `restart_claude` tool:

```
> Please restart Claude to pick up my MCP server changes

[Calls restart_claude tool]

Restart signal sent! Claude will restart momentarily and resume with --continue.
```

## How It Works

1. User starts Claude via `rusty-restart-claude [args...]`
2. Wrapper spawns Claude as a child process with full PTY passthrough
3. Terminal size is inherited and resize events (SIGWINCH) are propagated
4. Claude connects to MCP servers, including `rusty-restart-claude --mcp-server`
5. When `restart_claude` tool is called:
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
- `prompt` (optional, string): A prompt to automatically send after restart, enabling seamless continuation of work

**Example with prompt:**
```
restart_claude(reason: "MCP server updated", prompt: "Continue where we left off - the MCP servers have been reloaded.")
```

This triggers a restart and automatically sends the prompt to Claude after it initializes, continuing the workflow without manual intervention.

### server_status

Shows status information about the wrapper and Claude Code process.

**Response includes:**
- `mcp_server_pid`: This MCP server's process ID
- `wrapper_pid`: The wrapper's process ID (if running through wrapper)
- `wrapper_running`: Whether the wrapper is active
- `claude_code_pid`: Claude Code's process ID
- `working_directory`: Current working directory

## Features

- **Terminal preserved** - Wrapper owns terminal, Claude is just a child
- **Full PTY passthrough** - Complete terminal emulation with proper size handling
- **Terminal resize support** - SIGWINCH propagation keeps Claude's display correct
- **Scrollback buffer** - 10,000 lines of history with mouse wheel scrolling
- **Session continuation** - Restart always uses `--continue` to resume conversation
- **Prompt injection** - Optionally auto-send a prompt after restart to continue work seamlessly
- **Simple signaling** - File-based IPC, no complex sockets or daemons
- **Graceful shutdown** - SIGINT (3s) → SIGTERM (2s) → SIGKILL sequence
- **Raw mode passthrough** - All keyboard input forwarded correctly

## Scrollback

The wrapper includes a built-in scrollback buffer using the `vt100` crate for proper terminal emulation:

- **Page Up**: Enter scroll mode and scroll up through history
- **Page Down**: Scroll down (exits scroll mode when reaching bottom)
- **Arrow Up/Down**: Scroll line by line when in scroll mode
- **q or Esc**: Exit scroll mode and return to live view

The scrollback uses vt100 terminal emulation to properly parse and render terminal output, preserving colors and formatting.

## Platform Support

Currently Linux only (uses `/proc` filesystem and PTY).

## License

MIT
