# aegis-mcp Expansion Plan

## Overview

Extend aegis-mcp from a simple agent restart wrapper to a full **agent runtime/supervisor** with:
1. **Multi-Agent Orchestration** ✅ COMPLETED
2. **Network Monitoring** ✅ COMPLETED
3. **Combined LD_PRELOAD Hooks** ✅ COMPLETED (filesystem overlay for MCP injection)

### Design Principles

- **Self-contained**: All functionality is built into aegis-mcp itself. No external dependencies.
- **Privilege Safety**: Drop privileges before executing coding agents by default.
- **Process Isolation**: Use LD_PRELOAD for per-process modifications (no shared file mutations).

---

## Current Status

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | Privilege handling (`--keep-root`) | ✅ DONE |
| 1a | Agent pool core (`src/pool/`) | ✅ DONE |
| 1b | Agent MCP tools | ✅ DONE |
| 2a | LD_PRELOAD network monitoring | ✅ DONE |
| 2b | `--netmon` integration | ✅ DONE |
| 2c | Network namespace support | ✅ DONE |
| 3 | Combined hooks library + FS overlay | ✅ DONE |

**13 MCP tools registered**, **22 tests passing**

### All Phases Complete! 🎉

---

## Privilege Handling

### Default Behavior (Root → Drop)

When aegis-mcp is started as root:
1. Set up any root-required resources (network namespace, etc.)
2. **Drop to the original user** before spawning the coding agent
3. Agent runs with normal user privileges

### CLI Flags

```
aegis-mcp claude                    # Normal user, no privilege changes
sudo aegis-mcp claude               # Drop root before spawning claude (default)
sudo aegis-mcp claude --keep-root   # Stay root (for debugging, advanced use)
```

### Implementation (`src/privileges.rs`)

```rust
use nix::unistd::{setuid, setgid, Uid, Gid};
use std::env;

/// Drop root privileges to the original user
pub fn drop_privileges() -> Result<()> {
    if !Uid::effective().is_root() {
        return Ok(()); // Not root, nothing to do
    }

    // Get original user from SUDO_UID/SUDO_GID
    let uid = env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Uid::from_raw)
        .ok_or_else(|| anyhow!("Running as root but SUDO_UID not set"))?;

    let gid = env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Gid::from_raw)
        .ok_or_else(|| anyhow!("Running as root but SUDO_GID not set"))?;

    // Drop privileges (must do gid first, then uid)
    setgid(gid).context("Failed to drop group privileges")?;
    setuid(uid).context("Failed to drop user privileges")?;

    info!("Dropped privileges to uid={}, gid={}", uid, gid);
    Ok(())
}
```

### Wrapper Integration

```rust
// src/wrapper.rs - in run() function

pub fn run(agent_name: String, agent_args: Vec<String>, keep_root: bool) -> Result<()> {
    let agent = find_agent(&agent_name)?;

    // Set up root-required resources BEFORE dropping privileges
    if Uid::effective().is_root() {
        if let Some(netmon) = &netmon_config {
            if netmon.mode == NetmonMode::Namespace {
                // Create network namespace while still root
                setup_network_namespace()?;
            }
        }
    }

    // Drop privileges unless --keep-root specified
    if !keep_root {
        privileges::drop_privileges()?;
    }

    // Now spawn agent with (hopefully) normal user privileges
    // ...
}
```

---

## Phase 1: Multi-Agent Orchestration (Built-in Pool)

### Architecture

```
aegis-mcp (orchestrator)
├── Wrapper Mode: supervises primary agent (claude, aider, cursor)
├── MCP Server Mode: exposes orchestration tools
│   ├── restart_agent - existing
│   ├── server_status - existing
│   ├── agent_spawn - NEW: spawn background agent
│   ├── agent_list - NEW: list all agents
│   ├── agent_status - NEW: get agent status
│   ├── agent_await - NEW: wait for completion
│   ├── agent_stop - NEW: stop agent
│   └── agent_pool_stats - NEW: pool statistics
└── Agent Pool: manages concurrent background agents
    ├── AgentHandle: tracks running agents
    ├── TaskQueue: pending tasks
    └── FileLockManager: coordinate file access
```

### Key Files to Create/Modify

| File | Purpose |
|------|---------|
| `src/pool/mod.rs` | Agent pool management |
| `src/pool/agent.rs` | Individual agent lifecycle |
| `src/pool/task.rs` | Task definition and queue |
| `src/pool/locks.rs` | File lock coordination |
| `src/pool/communication.rs` | Inter-agent messaging |
| `src/privileges.rs` | Root privilege dropping |
| `src/mcp_server.rs` | Add agent_* MCP tools |
| `src/main.rs` | Initialize pool, add --max-agents and --keep-root flags |
| `Cargo.toml` | Add tokio, uuid dependencies |

### Implementation Steps

1. **Create Agent Pool Core** (`src/pool/mod.rs`)
   ```rust
   pub struct AgentPool {
       max_agents: usize,
       agents: HashMap<String, AgentHandle>,
       file_locks: FileLockManager,
       task_queue: TaskQueue,
   }

   impl AgentPool {
       pub async fn spawn(&self, task: Task, agent_type: &str) -> Result<String>;
       pub async fn status(&self, id: &str) -> Option<AgentStatus>;
       pub async fn await_completion(&self, id: &str) -> Result<TaskResult>;
       pub async fn stop(&self, id: &str) -> Result<()>;
       pub async fn list(&self) -> Vec<(String, AgentStatus)>;
   }
   ```

2. **Create Agent Handle** (`src/pool/agent.rs`)
   - Spawns agent subprocess (claude/aider/cursor)
   - Monitors stdout/stderr
   - Tracks iterations and status
   - Handles graceful shutdown

3. **Add File Lock Manager** (`src/pool/locks.rs`)
   - Prevents concurrent file edits
   - Read/write lock types
   - Agent-scoped locks

4. **Extend MCP Server** (`src/mcp_server.rs`)
   - Add 6 new tools for agent management
   - Pool is initialized on first spawn

5. **Update Wrapper** (`src/wrapper.rs`)
   - Pass pool reference to MCP server mode
   - Share state between wrapper and MCP server

### MCP Tool Definitions

```json
{
  "agent_spawn": {
    "description": "Spawn a background agent to work on a task autonomously",
    "inputSchema": {
      "type": "object",
      "properties": {
        "description": { "type": "string", "description": "Task for the agent" },
        "agent_type": { "type": "string", "enum": ["claude", "aider", "cursor"] },
        "working_directory": { "type": "string" },
        "max_iterations": { "type": "integer", "default": 50 }
      },
      "required": ["description"]
    }
  },
  "agent_list": {
    "description": "List all active background agents",
    "inputSchema": { "type": "object", "properties": {} }
  },
  "agent_status": {
    "description": "Get status of a specific agent",
    "inputSchema": {
      "type": "object",
      "properties": { "agent_id": { "type": "string" } },
      "required": ["agent_id"]
    }
  },
  "agent_await": {
    "description": "Wait for an agent to complete and get result",
    "inputSchema": {
      "type": "object",
      "properties": {
        "agent_id": { "type": "string" },
        "timeout_secs": { "type": "integer" }
      },
      "required": ["agent_id"]
    }
  },
  "agent_stop": {
    "description": "Stop a running agent",
    "inputSchema": {
      "type": "object",
      "properties": { "agent_id": { "type": "string" } },
      "required": ["agent_id"]
    }
  },
  "agent_pool_stats": {
    "description": "Get agent pool statistics",
    "inputSchema": { "type": "object", "properties": {} }
  }
}
```

---

## Phase 2: Network Monitoring

### Architecture

```
aegis-mcp
├── Non-root Mode (LD_PRELOAD):
│   └── libnetmon.so intercepts connect/send/recv
│       └── Logs to /tmp/aegis-mcp-netmon-{pid}.jsonl
└── Root Mode (Network Namespace):
    └── Agent runs in isolated netns
        ├── veth pair to host namespace
        ├── iptables NFLOG for packet capture
        └── Traffic routed through aegis-mcp proxy
```

### Key Files to Create

| File | Purpose |
|------|---------|
| `netmon/Cargo.toml` | Subcrate for LD_PRELOAD library |
| `netmon/src/lib.rs` | Shared library with libc interception |
| `src/netmon/mod.rs` | Network monitoring coordinator |
| `src/netmon/preload.rs` | LD_PRELOAD setup and log parsing |
| `src/netmon/netns.rs` | Network namespace setup (root) |
| `src/netmon/proxy.rs` | Transparent proxy for netns mode |

### LD_PRELOAD Library (`netmon/`)

```rust
// netmon/src/lib.rs - cdylib that intercepts network calls

#[no_mangle]
pub extern "C" fn connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    log_connection(addr);  // Log to file
    real_connect(fd, addr, len)  // Call real libc::connect via dlsym
}

#[no_mangle]
pub extern "C" fn send(fd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t {
    log_send(fd, len);
    real_send(fd, buf, len, flags)
}
```

**Dependencies for netmon crate:**
```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
libc = "0.2"
once_cell = "1"
```

**Log Format (JSONL):**
```json
{"ts":1706000000,"event":"connect","addr":"api.anthropic.com:443","fd":5}
{"ts":1706000001,"event":"send","fd":5,"bytes":2048}
{"ts":1706000002,"event":"recv","fd":5,"bytes":4096}
```

### Network Namespace Mode (Root)

**Setup Steps:**
1. Create network namespace: `ip netns add aegis-{pid}`
2. Create veth pair: `ip link add veth-aegis type veth peer name veth-agent`
3. Move peer to namespace: `ip link set veth-agent netns aegis-{pid}`
4. Configure addresses and routing
5. Set up NAT/masquerade on host
6. Add NFLOG rules for packet capture

**Implementation (`src/netmon/netns.rs`):**
```rust
pub struct NetworkNamespace {
    name: String,
    veth_host: String,
    veth_agent: String,
}

impl NetworkNamespace {
    pub fn create(pid: u32) -> Result<Self>;
    pub fn enter_namespace(&self) -> Result<()>;
    pub fn setup_routing(&self) -> Result<()>;
    pub fn start_capture(&self) -> Result<PacketReceiver>;
    pub fn cleanup(&self) -> Result<()>;
}
```

**Dependencies:**
- `nix` (already have) - namespace operations
- `rtnetlink` - network interface setup
- `netlink-packet-route` - routing

### Integration with Wrapper

```rust
// src/wrapper.rs additions

fn run_agent_with_netmon(agent: &AgentConfig, args: &[String], mode: NetmonMode) {
    match mode {
        NetmonMode::Preload => {
            // Set LD_PRELOAD and spawn
            env::set_var("LD_PRELOAD", "/path/to/libnetmon.so");
            env::set_var("AEGIS_NETMON_LOG", format!("/tmp/aegis-mcp-netmon-{}.jsonl", pid));
        }
        NetmonMode::Namespace => {
            // Create namespace, enter it, then spawn
            let ns = NetworkNamespace::create(pid)?;
            ns.enter_namespace()?;
            ns.setup_routing()?;
        }
    }
    // ... spawn agent
}
```

### CLI Flags

```
# Network monitoring modes
aegis-mcp claude --netmon          # Auto-detect mode (preload if non-root, netns if root)
aegis-mcp claude --netmon=preload  # Force LD_PRELOAD mode
aegis-mcp claude --netmon=netns    # Force network namespace (requires root)

# Root privilege handling
sudo aegis-mcp claude              # Drops root before spawning agent (default)
sudo aegis-mcp claude --keep-root  # Stay root (for debugging, --netmon=netns without dropping)

# Combined example
sudo aegis-mcp claude --netmon=netns --keep-root  # Network namespace, agent runs as root
```

### Implementation Steps

1. **Create netmon subcrate:**
   - Set up `netmon/Cargo.toml` with cdylib target
   - Implement connect/send/recv interception
   - Add JSONL logging to file

2. **Integrate LD_PRELOAD in wrapper:**
   - Add `--netmon` flag parsing
   - Set LD_PRELOAD when spawning agent
   - Add log file reader for status

3. **Implement network namespace (root mode):**
   - Add namespace creation/cleanup
   - Implement veth pair setup
   - Add packet capture via NFLOG

4. **Add MCP tools for network monitoring:**
   - `netmon_status` - current monitoring mode/stats
   - `netmon_connections` - list active connections
   - `netmon_log` - recent network activity

---

## Dependencies to Add

```toml
# Cargo.toml
[dependencies]
tokio = { version = "1", features = ["full"] }  # async runtime for pool
uuid = { version = "1", features = ["v4"] }     # agent IDs

# For network namespace (optional, feature-gated)
rtnetlink = { version = "0.14", optional = true }
netlink-packet-route = { version = "0.19", optional = true }

[features]
default = []
netns = ["rtnetlink", "netlink-packet-route"]

[workspace]
members = ["netmon"]
```

```toml
# netmon/Cargo.toml
[package]
name = "aegis-netmon"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
libc = "0.2"
once_cell = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

---

## Verification Plan

### Multi-Agent Orchestration

```bash
# Start aegis-mcp with Claude
aegis-mcp claude

# In Claude session, test agent tools:
> Use agent_spawn to create a background agent that researches the codebase
> Use agent_list to see running agents
> Use agent_status with the agent_id
> Use agent_await to wait for completion
```

**Expected behavior:**
- `agent_spawn` returns agent ID immediately
- `agent_list` shows agent with status "working"
- `agent_await` blocks until completion, returns result
- Multiple agents can run concurrently up to pool limit

### Network Monitoring (LD_PRELOAD)

```bash
# Build the netmon library
cargo build -p aegis-netmon --release

# Test manually
LD_PRELOAD=./target/release/libnetmon.so AEGIS_NETMON_LOG=/tmp/test.jsonl curl https://example.com
cat /tmp/test.jsonl  # Should show connect/send/recv entries

# Test with aegis-mcp
aegis-mcp claude --netmon
# After some API calls, check the log file
```

### Network Monitoring (Namespace - Root)

```bash
# Must be root
sudo aegis-mcp claude --netmon=netns

# Verify namespace exists
ip netns list | grep aegis

# Check traffic capture is working
# (agent's network traffic should be logged)
```

---

## Implementation Order

1. ~~**Phase 0: Privilege Handling**~~ ✅ DONE
2. ~~**Phase 1a: Agent Pool Core**~~ ✅ DONE
3. ~~**Phase 1b: MCP Tools**~~ ✅ DONE
4. ~~**Phase 2a: LD_PRELOAD Library**~~ ✅ DONE
5. ~~**Phase 2b: Integrate Preload**~~ ✅ DONE
6. ~~**Phase 2c: Network Namespace**~~ ✅ DONE
7. ~~**Phase 3: Combined Hooks + FS Overlay**~~ ✅ DONE

---

## Phase 3: Combined Hooks Library (COMPLETED)

### Problem: Current MCP Injection Lacks Process Isolation

Current `mcp_inject.rs` modifies the actual `.mcp.json` file on disk:
- ❌ Affects ALL processes in the directory (not just the wrapped agent)
- ❌ Requires cleanup coordination between multiple aegis-mcp instances
- ❌ Race conditions when multiple instances modify the same file

### Solution: LD_PRELOAD Filesystem Overlay

Intercept `open()`/`openat()` calls to redirect `.mcp.json` reads to a per-process temp file:
- ✅ Only affects the wrapped child process
- ✅ No modification to actual `.mcp.json`
- ✅ Automatic cleanup (temp file per-process)
- ✅ Multiple aegis-mcp instances work independently

### Rename: `netmon/` → `hooks/`

Consolidate into a single LD_PRELOAD library:

```
hooks/                          # Renamed from netmon/
├── Cargo.toml                  # name = "aegis-hooks"
└── src/
    └── lib.rs                  # Combined: network + filesystem hooks
        ├── Network monitoring  # (existing)
        └── Filesystem overlay  # (new)

Output: libaegis_hooks.so       # Renamed from libaegis_netmon.so
```

### Environment Variables

```bash
# Network monitoring (existing)
AEGIS_NETMON_LOG=/tmp/aegis-netmon-{pid}.jsonl

# Filesystem overlay (new)
AEGIS_MCP_OVERLAY=/tmp/aegis-mcp-{pid}.mcp.json
AEGIS_MCP_TARGET=.mcp.json      # Which file to overlay (relative to cwd)
```

### Filesystem Hooks to Add

```rust
// Type definitions
type OpenFn = unsafe extern "C" fn(*const c_char, c_int, ...) -> c_int;
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, ...) -> c_int;

static REAL_OPEN: Lazy<Option<OpenFn>> = Lazy::new(|| unsafe { get_real_fn("open") });
static REAL_OPENAT: Lazy<Option<OpenatFn>> = Lazy::new(|| unsafe { get_real_fn("openat") });

/// Check if path matches our overlay target
fn should_overlay(path: &str) -> bool {
    if let Ok(target) = std::env::var("AEGIS_MCP_TARGET") {
        path.ends_with(&target) || path == target
    } else {
        false
    }
}

/// Get the overlay file path
fn get_overlay_path() -> Option<String> {
    std::env::var("AEGIS_MCP_OVERLAY").ok()
}

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    let path_str = CStr::from_ptr(path).to_string_lossy();

    // Check if this is our overlay target
    if should_overlay(&path_str) {
        if let Some(overlay) = get_overlay_path() {
            let overlay_cstr = CString::new(overlay).unwrap();
            return match *REAL_OPEN {
                Some(f) => f(overlay_cstr.as_ptr(), flags, mode),
                None => { *libc::__errno_location() = libc::ENOSYS; -1 }
            };
        }
    }

    // Normal open
    match *REAL_OPEN {
        Some(f) => f(path, flags, mode),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}
```

### Files to Modify

| File | Changes |
|------|---------|
| `hooks/Cargo.toml` | Rename from `netmon/`, update package name |
| `hooks/src/lib.rs` | Add `open`/`openat` interception |
| `src/netmon/mod.rs` | Update library path references |
| `src/wrapper.rs` | Set `AEGIS_MCP_OVERLAY` env var, create temp file |
| `src/mcp_inject.rs` | **DELETE** - no longer needed |
| `src/main.rs` | Remove `mcp_inject` module |

### Wrapper Integration

```rust
// src/wrapper.rs - updated run() function

pub fn run(..., inject_mcp: bool) -> Result<()> {
    // Create temp .mcp.json for this process
    let mcp_overlay_path = if inject_mcp {
        let path = format!("/tmp/aegis-mcp-{}.mcp.json", process::id());
        let config = create_mcp_config()?;  // JSON with aegis-mcp server
        fs::write(&path, config)?;
        Some(path)
    } else {
        None
    };

    // ... later when spawning agent ...

    // Always use LD_PRELOAD (for network + filesystem hooks)
    let hooks_lib = find_hooks_library()?;
    cmd.env("LD_PRELOAD", hooks_lib);

    if let Some(overlay) = &mcp_overlay_path {
        cmd.env("AEGIS_MCP_OVERLAY", overlay);
        cmd.env("AEGIS_MCP_TARGET", ".mcp.json");
    }

    if netmon_enabled {
        cmd.env("AEGIS_NETMON_LOG", netmon_log_path);
    }
}
```

### Cleanup

The temp file is automatically cleaned up:
- When aegis-mcp wrapper exits (normal exit or crash)
- Each instance uses its own PID-namespaced temp file
- No coordination needed between instances

---

## Verification Plan

### Test Filesystem Overlay

```bash
# Build hooks library
cargo build -p aegis-hooks --release

# Create test overlay file
echo '{"mcpServers":{"test":{}}}' > /tmp/overlay.mcp.json

# Test with cat (should read overlay, not real file)
echo '{"original":"data"}' > .mcp.json
LD_PRELOAD=./target/release/libaegis_hooks.so \
  AEGIS_MCP_OVERLAY=/tmp/overlay.mcp.json \
  AEGIS_MCP_TARGET=.mcp.json \
  cat .mcp.json
# Expected: {"mcpServers":{"test":{}}}

# Verify real file unchanged
cat .mcp.json
# Expected: {"original":"data"}
```

### Test Process Isolation

```bash
# Terminal 1: Start aegis-mcp wrapped claude
aegis-mcp claude

# Terminal 2: Start unwrapped claude in same directory
claude

# Terminal 2's claude should NOT see aegis-mcp MCP server
# Terminal 1's claude SHOULD see aegis-mcp MCP server
```

---

## Out of Scope (Future Enhancements)

- Real-time network monitoring dashboard (TUI)
- eBPF-based monitoring (alternative to LD_PRELOAD)
- Inter-agent communication protocol
- Cost tracking from API traffic analysis
- Agent task dependencies/DAG execution
- Warm agent pool (pre-spawned agents)
