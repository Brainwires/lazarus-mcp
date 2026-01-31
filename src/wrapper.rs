use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{self, json};
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

// Signal handling

use crate::netmon::NetmonMode;
use crate::privileges;
use crate::watchdog::{self, HealthStatus, LockupAction, ProcessState, SharedWatchdog, WatchdogConfig};

// ============================================================================
// Crash Cleanup Registry
// ============================================================================

/// Global registry of files to clean up on crash/exit
static CLEANUP_REGISTRY: Mutex<Option<CleanupRegistry>> = Mutex::new(None);

#[derive(Default)]
struct CleanupRegistry {
    overlay_path: Option<PathBuf>,
    stub_created: bool,
    marker_path: Option<PathBuf>,
}

/// Register files for cleanup on crash
fn register_cleanup(overlay: Option<PathBuf>, stub_created: bool) {
    if let Ok(mut guard) = CLEANUP_REGISTRY.lock() {
        *guard = Some(CleanupRegistry {
            overlay_path: overlay,
            stub_created,
            marker_path: Some(mcp_marker_path()),
        });
    }
}

/// Perform emergency cleanup (called from panic hook or signal handler)
fn emergency_cleanup() {
    if let Ok(guard) = CLEANUP_REGISTRY.lock() {
        if let Some(ref registry) = *guard {
            // Remove overlay file
            if let Some(ref path) = registry.overlay_path {
                let _ = fs::remove_file(path);
            }
            // Remove marker file
            if let Some(ref path) = registry.marker_path {
                let _ = fs::remove_file(path);
            }
            // Remove stub if we created it and no other instances
            if registry.stub_created && !other_aegis_instances_exist() {
                let mcp_path = Path::new(MCP_TARGET_FILE);
                if let Ok(content) = fs::read_to_string(mcp_path) {
                    let trimmed = content.trim();
                    if trimmed == "{\"mcpServers\":{}}" || trimmed == "{ \"mcpServers\": {} }" {
                        let _ = fs::remove_file(mcp_path);
                    }
                }
            }
        }
    }
}

/// Install panic hook for cleanup on crash
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        eprintln!("[aegis-mcp] Panic detected, cleaning up...");
        emergency_cleanup();
        default_hook(panic_info);
    }));
}

// ============================================================================
// Version Information
// ============================================================================

/// Main binary version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build timestamp
pub const BUILD_TIME: &str = env!("AEGIS_BUILD_TIME");

/// Git commit hash
pub const GIT_HASH: &str = env!("AEGIS_GIT_HASH");

const SIGNAL_FILE_PREFIX: &str = "/tmp/aegis-mcp-";
const WATCHDOG_PING_PREFIX: &str = "/tmp/aegis-watchdog-ping-";
const WATCHDOG_CONFIG_PREFIX: &str = "/tmp/aegis-watchdog-config-";

/// Environment variable for MCP overlay file path
const MCP_OVERLAY_ENV: &str = "AEGIS_MCP_OVERLAY";
/// Environment variable for target file to overlay
const MCP_TARGET_ENV: &str = "AEGIS_MCP_TARGET";
/// Default target file for MCP overlay
const MCP_TARGET_FILE: &str = ".mcp.json";

/// Shared state file for TUI/MCP communication
const SHARED_STATE_FILE: &str = "/tmp/aegis-mcp-state-";

/// Shared state accessible by TUI and MCP server
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SharedState {
    /// Wrapper PID
    pub wrapper_pid: u32,
    /// Agent PID (if running)
    pub agent_pid: Option<u32>,
    /// Agent name
    pub agent_name: String,
    /// Agent status
    pub agent_status: AgentState,
    /// Number of restarts
    pub restart_count: u32,
    /// Watchdog health status
    pub health: Option<HealthStatus>,
    /// Uptime in seconds
    pub uptime_secs: u64,
    /// Start timestamp (unix epoch)
    pub started_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Running,
    Restarting,
    Stopped,
    Failed,
}

impl SharedState {
    pub fn new(agent_name: &str) -> Self {
        Self {
            wrapper_pid: process::id(),
            agent_pid: None,
            agent_name: agent_name.to_string(),
            agent_status: AgentState::Starting,
            restart_count: 0,
            health: None,
            uptime_secs: 0,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// Get the shared state file path
    pub fn state_file_path() -> PathBuf {
        PathBuf::from(format!("{}{}", SHARED_STATE_FILE, process::id()))
    }

    /// Write state to file for other processes to read
    pub fn save(&self) -> Result<()> {
        let path = Self::state_file_path();
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Load state from file
    pub fn load(wrapper_pid: u32) -> Result<Self> {
        let path = PathBuf::from(format!("{}{}", SHARED_STATE_FILE, wrapper_pid));
        let content = fs::read_to_string(&path)?;
        let state: Self = serde_json::from_str(&content)?;
        Ok(state)
    }
}

/// Agent-specific configuration
struct AgentConfig {
    /// Name of the agent
    name: String,
    /// Path to the executable
    path: PathBuf,
    /// Flag to continue/resume session (if supported)
    continue_flag: Option<&'static str>,
    /// Flag to skip permission prompts (if supported)
    skip_permissions_flag: Option<&'static str>,
}

/// Get the signal file path for this wrapper instance
pub fn signal_file_path() -> PathBuf {
    PathBuf::from(format!("{}{}", SIGNAL_FILE_PREFIX, process::id()))
}

/// Find an agent executable by name
fn find_agent(agent_name: &str) -> Result<AgentConfig> {
    match agent_name.to_lowercase().as_str() {
        "claude" => find_claude(),
        "cursor" => find_cursor(),
        "aider" => find_aider(),
        _ => anyhow::bail!(
            "Unknown agent '{}'. Supported agents: claude, cursor, aider",
            agent_name
        ),
    }
}

/// Find the Claude Code executable
fn find_claude() -> Result<AgentConfig> {
    let candidates = [
        which::which("claude").ok(),
        Some(PathBuf::from("/usr/local/bin/claude")),
        Some(PathBuf::from("/usr/bin/claude")),
        dirs::home_dir().map(|h| h.join(".local/bin/claude")),
        dirs::home_dir().map(|h| h.join(".local/share/claude/claude")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() && candidate.is_file() {
            return Ok(AgentConfig {
                name: "claude".to_string(),
                path: candidate,
                continue_flag: Some("--continue"),
                skip_permissions_flag: Some("--dangerously-skip-permissions"),
            });
        }
    }

    // Fallback: try to find in ~/.local/share/claude/versions/
    if let Some(home) = dirs::home_dir() {
        let versions_dir = home.join(".local/share/claude/versions");
        if versions_dir.exists() {
            if let Ok(entries) = fs::read_dir(&versions_dir) {
                let mut versions: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .collect();
                versions.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

                if let Some(latest) = versions.first() {
                    let claude_path = latest.path().join("claude");
                    if claude_path.exists() {
                        return Ok(AgentConfig {
                            name: "claude".to_string(),
                            path: claude_path,
                            continue_flag: Some("--continue"),
                            skip_permissions_flag: Some("--dangerously-skip-permissions"),
                        });
                    }
                }
            }
        }
    }

    anyhow::bail!("Could not find claude executable. Make sure Claude Code is installed.")
}

/// Find the Cursor editor executable
fn find_cursor() -> Result<AgentConfig> {
    let candidates = [
        which::which("cursor").ok(),
        Some(PathBuf::from("/usr/local/bin/cursor")),
        Some(PathBuf::from("/usr/bin/cursor")),
        dirs::home_dir().map(|h| h.join(".local/bin/cursor")),
        // AppImage location
        dirs::home_dir().map(|h| h.join("Applications/cursor.AppImage")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() && candidate.is_file() {
            return Ok(AgentConfig {
                name: "cursor".to_string(),
                path: candidate,
                continue_flag: None, // Cursor doesn't have a continue flag
                skip_permissions_flag: None,
            });
        }
    }

    anyhow::bail!("Could not find cursor executable. Make sure Cursor is installed.")
}

/// Find the Aider CLI executable
fn find_aider() -> Result<AgentConfig> {
    let candidates = [
        which::which("aider").ok(),
        Some(PathBuf::from("/usr/local/bin/aider")),
        Some(PathBuf::from("/usr/bin/aider")),
        dirs::home_dir().map(|h| h.join(".local/bin/aider")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() && candidate.is_file() {
            return Ok(AgentConfig {
                name: "aider".to_string(),
                path: candidate,
                continue_flag: None, // Aider auto-continues via chat history
                skip_permissions_flag: Some("--yes"), // Auto-confirm prompts
            });
        }
    }

    anyhow::bail!("Could not find aider executable. Install with: pip install aider-chat")
}

/// Parsed restart signal
#[derive(Debug)]
struct ParsedRestartSignal {
    reason: String,
    prompt: Option<String>,
}

/// Get the MCP overlay file path for this process
fn mcp_overlay_path() -> PathBuf {
    PathBuf::from(format!("/tmp/aegis-mcp-overlay-{}.json", process::id()))
}

/// Get the marker file path for this process (tracks our instance)
fn mcp_marker_path() -> PathBuf {
    PathBuf::from(format!(".mcp.json.aegis.{}", process::id()))
}

/// Check if any other aegis-mcp instances are using the .mcp.json in this directory
fn other_aegis_instances_exist() -> bool {
    let current_marker = mcp_marker_path();
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".mcp.json.aegis.") && entry.path() != current_marker {
                // Check if the PID is still running
                if let Some(pid_str) = name_str.strip_prefix(".mcp.json.aegis.") {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        if Path::new(&format!("/proc/{}", pid)).exists() {
                            return true;
                        } else {
                            // Stale marker, clean it up
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    }
    false
}

/// Create a stub .mcp.json file on disk so Claude sees it exists
/// The actual content will be intercepted by LD_PRELOAD and replaced with overlay
fn create_mcp_stub_file() -> Result<bool> {
    let mcp_path = Path::new(MCP_TARGET_FILE);

    // Create marker file first to track our instance
    let marker_path = mcp_marker_path();
    fs::write(&marker_path, format!("{}", process::id()))?;

    if mcp_path.exists() {
        // File already exists (either from another aegis instance or user-created)
        info!("Using existing {}", MCP_TARGET_FILE);
        return Ok(false); // false = we didn't create it
    }

    // Create minimal valid MCP config - hooks replace content on read
    fs::write(mcp_path, "{\"mcpServers\":{}}\n")?;
    info!("Created stub {}", MCP_TARGET_FILE);

    Ok(true) // true = we created it
}

/// Clean up stub .mcp.json if no other instances are using it
/// Note: we_created_it is unused now - any last instance cleans up orphaned stubs
fn cleanup_mcp_stub_file(_we_created_it: bool) {
    let marker_path = mcp_marker_path();

    // Remove our marker first
    let _ = fs::remove_file(&marker_path);

    // Check if any other instances exist (this also cleans up stale markers from crashed processes)
    // If we're the last instance, clean up any orphaned stub file
    if !other_aegis_instances_exist() {
        let mcp_path = Path::new(MCP_TARGET_FILE);
        if mcp_path.exists() {
            // Only remove if it's a minimal stub (no user modifications)
            if let Ok(content) = fs::read_to_string(mcp_path) {
                let trimmed = content.trim();
                if trimmed == "{\"mcpServers\":{}}" || trimmed == "{ \"mcpServers\": {} }" {
                    let _ = fs::remove_file(mcp_path);
                    info!("Cleaned up stub {}", MCP_TARGET_FILE);
                }
            }
        }
    }
}

/// Create the MCP server configuration JSON for aegis-mcp
/// For Claude, this is passed via --mcp-config and merged with other configs
/// For other agents, this is used with LD_PRELOAD overlay
fn create_mcp_config() -> Result<String> {
    let aegis_path = std::env::current_exe()
        .context("Failed to get current executable path")?;

    // Create config with just aegis-mcp - Claude will merge with project config
    let config = json!({
        "mcpServers": {
            "aegis-mcp": {
                "command": aegis_path.to_string_lossy(),
                "args": ["--mcp-server"]
            }
        }
    });

    Ok(serde_json::to_string_pretty(&config)?)
}

/// Find the hooks library (libaegis_hooks.so)
fn find_hooks_library() -> Result<PathBuf> {
    let candidates = [
        // Next to the aegis-mcp binary
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("libaegis_hooks.so"))),
        // In the same directory as the binary
        Some(PathBuf::from("./libaegis_hooks.so")),
        // System lib directories
        Some(PathBuf::from("/usr/local/lib/libaegis_hooks.so")),
        Some(PathBuf::from("/usr/lib/libaegis_hooks.so")),
        // Development location (relative to cwd)
        Some(PathBuf::from("./target/release/libaegis_hooks.so")),
        Some(PathBuf::from("./target/debug/libaegis_hooks.so")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }

    anyhow::bail!(
        "Could not find libaegis_hooks.so. Build it with: cargo build -p aegis-hooks --release"
    )
}

/// Information about the hooks library version
#[derive(Debug, Clone)]
pub struct HooksLibraryInfo {
    pub path: PathBuf,
    pub version: String,
    pub build_time: String,
    pub is_compatible: bool,
    pub warning: Option<String>,
}

/// Verify the hooks library version matches the main binary
/// Uses dlopen/dlsym to load version info from the shared library
fn verify_hooks_library(lib_path: &Path) -> Result<HooksLibraryInfo> {
    use std::ffi::CString;

    let path_cstr = CString::new(lib_path.to_string_lossy().as_bytes())
        .context("Invalid library path")?;

    unsafe {
        // Load the library
        let handle = libc::dlopen(path_cstr.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let error = CStr::from_ptr(libc::dlerror());
            anyhow::bail!("Failed to load hooks library: {}", error.to_string_lossy());
        }

        // Get version function
        let version_fn_name = CString::new("aegis_hooks_version").unwrap();
        let version_fn = libc::dlsym(handle, version_fn_name.as_ptr());

        // Get build time function
        let build_time_fn_name = CString::new("aegis_hooks_build_time").unwrap();
        let build_time_fn = libc::dlsym(handle, build_time_fn_name.as_ptr());

        let (version, build_time) = if !version_fn.is_null() && !build_time_fn.is_null() {
            // Call the version function
            let version_fn: extern "C" fn() -> *const libc::c_char =
                std::mem::transmute(version_fn);
            let version_ptr = version_fn();
            let version = if !version_ptr.is_null() {
                CStr::from_ptr(version_ptr).to_string_lossy().to_string()
            } else {
                "unknown".to_string()
            };

            // Call the build time function
            let build_time_fn: extern "C" fn() -> *const libc::c_char =
                std::mem::transmute(build_time_fn);
            let build_time_ptr = build_time_fn();
            let build_time = if !build_time_ptr.is_null() {
                CStr::from_ptr(build_time_ptr).to_string_lossy().to_string()
            } else {
                "unknown".to_string()
            };

            (version, build_time)
        } else {
            // Old library without version info
            ("pre-versioning".to_string(), "unknown".to_string())
        };

        // Close the library
        libc::dlclose(handle);

        // Check compatibility
        let is_compatible = version.starts_with(VERSION) || version.contains(GIT_HASH);
        let warning = if !is_compatible {
            Some(format!(
                "Hooks library version mismatch! Binary: {} ({}), Library: {}. Consider rebuilding with: cargo build -p aegis-hooks",
                VERSION, GIT_HASH, version
            ))
        } else {
            None
        };

        Ok(HooksLibraryInfo {
            path: lib_path.to_path_buf(),
            version,
            build_time,
            is_compatible,
            warning,
        })
    }
}

/// Display version information for both binary and hooks library
pub fn print_version_info() {
    println!("aegis-mcp v{}", VERSION);
    println!("  Built: {}", BUILD_TIME);
    println!("  Git:   {}", GIT_HASH);

    match find_hooks_library() {
        Ok(lib_path) => {
            match verify_hooks_library(&lib_path) {
                Ok(info) => {
                    println!("\nHooks library: {}", info.path.display());
                    println!("  Version: {}", info.version);
                    println!("  Built:   {}", info.build_time);
                    if let Some(warning) = info.warning {
                        println!("\n  WARNING: {}", warning);
                    }
                }
                Err(e) => {
                    println!("\nHooks library: {} (failed to verify: {})", lib_path.display(), e);
                }
            }
        }
        Err(e) => {
            println!("\nHooks library: not found ({})", e);
        }
    }
}

/// Check if there's a restart signal and parse it
fn check_restart_signal() -> Option<ParsedRestartSignal> {
    let path = signal_file_path();
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            // Delete the signal file
            let _ = fs::remove_file(&path);

            // Try to parse as JSON to extract prompt
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                let reason = parsed.get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("restart requested")
                    .to_string();
                let prompt = parsed.get("prompt")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string());
                return Some(ParsedRestartSignal { reason, prompt });
            }

            // Fallback: treat content as reason
            return Some(ParsedRestartSignal {
                reason: content,
                prompt: None,
            });
        }
    }
    None
}

/// Get the watchdog ping signal file path
fn watchdog_ping_path() -> PathBuf {
    PathBuf::from(format!("{}{}", WATCHDOG_PING_PREFIX, process::id()))
}

/// Get the watchdog config signal file path
fn watchdog_config_path() -> PathBuf {
    PathBuf::from(format!("{}{}", WATCHDOG_CONFIG_PREFIX, process::id()))
}

/// Check for and handle watchdog ping signal
fn check_watchdog_ping(watchdog: &SharedWatchdog) {
    let path = watchdog_ping_path();
    if path.exists() {
        let _ = fs::remove_file(&path);
        watchdog.record_ping();
        info!("Watchdog ping received");
    }
}

/// Check for and handle watchdog config signal
fn check_watchdog_config(watchdog: &SharedWatchdog) {
    let path = watchdog_config_path();
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            let _ = fs::remove_file(&path);

            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                // Handle disable_for_secs
                if let Some(disable_secs) = config.get("disable_for_secs").and_then(|d| d.as_u64()) {
                    watchdog.disable_for(Duration::from_secs(disable_secs));
                    info!("Watchdog disabled for {} seconds", disable_secs);
                    return;
                }

                // Handle configuration updates
                let mut current_config = watchdog.get_config();

                if let Some(enabled) = config.get("enabled").and_then(|e| e.as_bool()) {
                    current_config.enabled = enabled;
                }
                if let Some(timeout) = config.get("heartbeat_timeout").and_then(|t| t.as_u64()) {
                    current_config.heartbeat_timeout = Duration::from_secs(timeout);
                }
                if let Some(action) = config.get("lockup_action").and_then(|a| a.as_str()) {
                    current_config.lockup_action = match action {
                        "warn" => LockupAction::Warn,
                        "restart" => LockupAction::Restart,
                        "restart_with_backoff" => LockupAction::RestartWithBackoff,
                        "kill" => LockupAction::Kill,
                        "notify_and_wait" => LockupAction::NotifyAndWait,
                        _ => current_config.lockup_action,
                    };
                }
                if let Some(max_mem) = config.get("max_memory_mb").and_then(|m| m.as_u64()) {
                    current_config.max_memory_mb = Some(max_mem);
                }

                watchdog.configure(current_config);
                info!("Watchdog configuration updated");
            }
        }
    }
}

/// Run the wrapper
pub fn run(agent_name: String, agent_args: Vec<String>, keep_root: bool, netmon_mode: Option<NetmonMode>, inject_mcp: bool) -> Result<()> {
    run_with_watchdog(agent_name, agent_args, keep_root, netmon_mode, inject_mcp, WatchdogConfig::default())
}

/// Run the wrapper with custom watchdog configuration
pub fn run_with_watchdog(
    agent_name: String,
    agent_args: Vec<String>,
    keep_root: bool,
    netmon_mode: Option<NetmonMode>,
    inject_mcp: bool,
    watchdog_config: WatchdogConfig,
) -> Result<()> {
    let agent = find_agent(&agent_name)?;
    info!("Found {} at: {:?}", agent.name, agent.path);
    info!("Wrapper PID: {}", process::id());

    // Create watchdog
    let watchdog = watchdog::create_watchdog_with_config(watchdog_config);
    let watchdog_enabled = watchdog.get_config().enabled;
    info!("Watchdog enabled: {}", watchdog_enabled);

    // Create shared state
    let mut shared_state = SharedState::new(&agent.name);
    let _ = shared_state.save(); // Initial save

    // Handle root privileges
    if privileges::is_root() {
        if keep_root {
            warn!("Running as root with --keep-root flag. Agent will run with elevated privileges.");
        } else {
            info!("Running as root, will drop privileges before spawning agent");
            privileges::drop_privileges()?;
        }
    }

    // Create MCP overlay file and stub for process-isolated injection
    let (mcp_overlay_file, mcp_stub_created) = if inject_mcp {
        // First create the overlay file in /tmp with injected config
        let overlay = match create_mcp_config() {
            Ok(config) => {
                let overlay_path = mcp_overlay_path();
                match fs::write(&overlay_path, &config) {
                    Ok(()) => {
                        info!("Created MCP overlay at: {}", overlay_path.display());
                        Some(overlay_path)
                    }
                    Err(e) => {
                        warn!("Failed to write MCP overlay: {}. Continuing without injection.", e);
                        None
                    }
                }
            }
            Err(e) => {
                warn!("Failed to create MCP config: {}. Continuing without injection.", e);
                None
            }
        };

        // Then create stub .mcp.json so Claude sees it exists
        let stub_created = if overlay.is_some() {
            match create_mcp_stub_file() {
                Ok(created) => created,
                Err(e) => {
                    warn!("Failed to create stub .mcp.json: {}. Injection may not work.", e);
                    false
                }
            }
        } else {
            false
        };

        (overlay, stub_created)
    } else {
        info!("MCP auto-injection disabled");
        (None, false)
    };

    // Find and verify hooks library if MCP injection or netmon is enabled
    let hooks_library = if mcp_overlay_file.is_some() || netmon_mode.is_some() {
        match find_hooks_library() {
            Ok(path) => {
                // Verify library version
                match verify_hooks_library(&path) {
                    Ok(info) => {
                        info!("Found hooks library: {} ({})", path.display(), info.version);
                        if let Some(warning) = &info.warning {
                            warn!("{}", warning);
                        }
                        Some(path)
                    }
                    Err(e) => {
                        warn!("Found hooks library but failed to verify: {}. Using anyway.", e);
                        Some(path)
                    }
                }
            }
            Err(e) => {
                warn!("{}. Hooks-based features will be disabled.", e);
                None
            }
        }
    } else {
        None
    };

    // Install panic hook for crash cleanup
    install_panic_hook();

    // Register files for cleanup on crash
    register_cleanup(mcp_overlay_file.clone(), mcp_stub_created);

    // Clean up any stale signal files
    let _ = fs::remove_file(signal_file_path());
    let _ = fs::remove_file(watchdog_ping_path());
    let _ = fs::remove_file(watchdog_config_path());

    // Set up signal handling for graceful shutdown (SIGINT and SIGTERM)
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Handle Ctrl+C (SIGINT)
    ctrlc::set_handler(move || {
        // Trigger emergency cleanup on signal
        emergency_cleanup();
        r.store(false, Ordering::SeqCst);
    }).context("Failed to set Ctrl+C handler")?;

    // Also handle SIGTERM for graceful shutdown
    let r2 = running.clone();
    if let Err(e) = unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
            emergency_cleanup();
            r2.store(false, Ordering::SeqCst);
        })
    } {
        warn!("Failed to register SIGTERM handler: {}", e);
    }

    let mut add_continue = false;
    let mut pending_prompt: Option<String> = None;
    let mut final_exit_code: Option<i32> = None;

    while running.load(Ordering::SeqCst) {
        // Build args for this run
        let mut args = agent_args.clone();

        // Add skip-permissions flag if agent supports it and not already present
        if let Some(skip_flag) = agent.skip_permissions_flag {
            if !args.iter().any(|a| a == skip_flag) {
                args.push(skip_flag.to_string());
                info!("Auto-adding {} flag", skip_flag);
            }
        }

        // Add continue flag on restarts if agent supports it and not already present
        if add_continue {
            if let Some(continue_flag) = agent.continue_flag {
                if !args.iter().any(|a| a == continue_flag || a == "-c") {
                    args.push(continue_flag.to_string());
                }
            }
        }

        // Add pending prompt as a command-line argument
        if let Some(prompt) = pending_prompt.take() {
            info!("Adding prompt as command-line argument: {}", prompt);
            args.push(prompt);
        }

        // For Claude, use --mcp-config to inject aegis-mcp server (more reliable than LD_PRELOAD)
        if agent.name == "claude" {
            if let Some(ref overlay_path) = mcp_overlay_file {
                args.push("--mcp-config".to_string());
                args.push(overlay_path.to_string_lossy().to_string());
                info!("Using --mcp-config for MCP injection: {}", overlay_path.display());
            }
        }

        info!("Starting {} with args: {:?}", agent.name, args);

        // Build environment variables for the agent
        let mut extra_env: HashMap<String, String> = HashMap::new();

        // Add LD_PRELOAD for hooks library (for network monitoring, NOT for MCP injection on Claude)
        // Claude uses --mcp-config instead which is more reliable
        if let Some(ref lib_path) = hooks_library {
            if netmon_mode.is_some() {
                extra_env.insert("LD_PRELOAD".to_string(), lib_path.to_string_lossy().to_string());
            }
        }

        // Add MCP overlay environment variables (for non-Claude agents that use LD_PRELOAD)
        if agent.name != "claude" {
            if let Some(ref overlay_path) = mcp_overlay_file {
                if let Some(ref lib_path) = hooks_library {
                    extra_env.insert("LD_PRELOAD".to_string(), lib_path.to_string_lossy().to_string());
                }
                extra_env.insert(MCP_OVERLAY_ENV.to_string(), overlay_path.to_string_lossy().to_string());
                extra_env.insert(MCP_TARGET_ENV.to_string(), MCP_TARGET_FILE.to_string());
            }
        }

        // Update shared state
        shared_state.agent_status = AgentState::Starting;
        let _ = shared_state.save();

        // Spawn agent with watchdog monitoring
        let exit_reason = run_agent(&agent.path, &args, &extra_env, running.clone(), watchdog.clone(), &mut shared_state)?;

        match exit_reason {
            ExitReason::RestartRequested { reason, prompt } => {
                info!("Restart requested: {}", reason);
                shared_state.restart_count += 1;
                shared_state.agent_status = AgentState::Restarting;
                let _ = shared_state.save();

                add_continue = true;
                pending_prompt = prompt;

                // Clear terminal and reset before restart
                // This helps ensure the agent's TUI renders properly
                print!("\x1b[2J\x1b[H\x1b[0m");
                let _ = std::io::stdout().flush();

                // Small delay before restart
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            ExitReason::WatchdogTriggered { action } => {
                warn!("Watchdog triggered with action: {:?}", action);
                shared_state.restart_count += 1;
                shared_state.agent_status = AgentState::Restarting;
                let _ = shared_state.save();

                match action {
                    LockupAction::Restart | LockupAction::RestartWithBackoff => {
                        add_continue = true;
                        // Clear terminal before restart
                        print!("\x1b[2J\x1b[H\x1b[0m");
                        let _ = std::io::stdout().flush();
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    LockupAction::Kill => {
                        info!("Watchdog killed unresponsive agent");
                        shared_state.agent_status = AgentState::Failed;
                        let _ = shared_state.save();
                        break;
                    }
                    LockupAction::Warn | LockupAction::NotifyAndWait => {
                        // These shouldn't trigger exit, but handle gracefully
                        add_continue = true;
                        continue;
                    }
                }
            }
            ExitReason::NormalExit(code) => {
                info!("{} exited with code: {}", agent.name, code);
                shared_state.agent_status = AgentState::Stopped;
                let _ = shared_state.save();
                final_exit_code = Some(code);
                break;
            }
            ExitReason::Signal(sig) => {
                info!("{} killed by signal: {:?}", agent.name, sig);
                shared_state.agent_status = AgentState::Stopped;
                let _ = shared_state.save();
                break;
            }
            ExitReason::WrapperShutdown => {
                info!("Wrapper shutdown requested");
                shared_state.agent_status = AgentState::Stopped;
                let _ = shared_state.save();
                break;
            }
        }
    }

    // Clean up signal files
    let _ = fs::remove_file(signal_file_path());
    let _ = fs::remove_file(watchdog_ping_path());
    let _ = fs::remove_file(watchdog_config_path());
    let _ = fs::remove_file(SharedState::state_file_path());

    // Clean up MCP overlay file
    if let Some(ref overlay_path) = mcp_overlay_file {
        let _ = fs::remove_file(overlay_path);
        info!("Cleaned up MCP overlay file");
    }

    // Clean up stub .mcp.json if we created it and no other instances need it
    cleanup_mcp_stub_file(mcp_stub_created);

    info!("Wrapper cleanup complete");

    // Exit with the agent's exit code if it exited normally
    if let Some(code) = final_exit_code {
        process::exit(code);
    }

    Ok(())
}

#[derive(Debug)]
enum ExitReason {
    RestartRequested { reason: String, prompt: Option<String> },
    WatchdogTriggered { action: LockupAction },
    NormalExit(i32),
    Signal(Signal),
    WrapperShutdown,
}

/// Run an agent as a simple child process with watchdog monitoring
fn run_agent(
    agent_path: &PathBuf,
    args: &[String],
    extra_env: &HashMap<String, String>,
    running: Arc<AtomicBool>,
    watchdog: SharedWatchdog,
    shared_state: &mut SharedState,
) -> Result<ExitReason> {
    // Build command with environment variables
    let mut cmd = Command::new(agent_path);
    cmd.args(args);

    // Add extra environment variables (e.g., LD_PRELOAD for MCP injection)
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    // Spawn agent directly - no PTY, no terminal emulation
    let mut child = cmd.spawn().context("Failed to spawn agent")?;

    let child_pid = Pid::from_raw(child.id() as i32);
    let child_pid_u32 = child.id();

    // Start watchdog monitoring
    watchdog.start_monitoring(child_pid_u32);
    info!("Watchdog started monitoring PID {}", child_pid_u32);

    // Update shared state with agent PID
    shared_state.agent_pid = Some(child_pid_u32);
    shared_state.agent_status = AgentState::Running;
    let _ = shared_state.save();

    // Track last health check time
    let check_interval = watchdog.get_config().check_interval;
    let mut last_health_check = std::time::Instant::now();

    // Monitor the child process
    loop {
        // Check if wrapper should stop
        if !running.load(Ordering::SeqCst) {
            watchdog.stop_monitoring();
            let _ = signal::kill(child_pid, Signal::SIGINT);
            return Ok(ExitReason::WrapperShutdown);
        }

        // Check for restart signal
        if let Some(signal_content) = check_restart_signal() {
            info!("Restart signal detected: {}", signal_content.reason);
            watchdog.stop_monitoring();

            // Send SIGINT to agent for graceful shutdown
            let _ = signal::kill(child_pid, Signal::SIGINT);

            // Wait for it to exit (with timeout escalation)
            let start = std::time::Instant::now();
            loop {
                match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                    Ok(WaitStatus::StillAlive) => {
                        if start.elapsed() > Duration::from_secs(3) {
                            info!("Agent not responding to SIGINT, sending SIGTERM");
                            let _ = signal::kill(child_pid, Signal::SIGTERM);
                        }
                        if start.elapsed() > Duration::from_secs(5) {
                            info!("Agent not responding to SIGTERM, sending SIGKILL");
                            let _ = signal::kill(child_pid, Signal::SIGKILL);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    _ => break,
                }
            }

            return Ok(ExitReason::RestartRequested {
                reason: signal_content.reason,
                prompt: signal_content.prompt,
            });
        }

        // Check for watchdog signals from MCP server
        check_watchdog_ping(&watchdog);
        check_watchdog_config(&watchdog);

        // Perform watchdog health check periodically
        if last_health_check.elapsed() >= check_interval {
            last_health_check = std::time::Instant::now();

            if let Some(health) = watchdog.check_health() {
                // Update shared state with health info
                shared_state.health = Some(health.clone());
                shared_state.uptime_secs = health.uptime_secs;
                let _ = shared_state.save();

                // Check if action is needed
                if let Some(action) = health.action_pending {
                    match action {
                        LockupAction::Warn => {
                            warn!(
                                "Watchdog warning: Process {} unresponsive for {}s",
                                child_pid_u32, health.last_activity_secs
                            );
                        }
                        LockupAction::NotifyAndWait => {
                            warn!(
                                "Watchdog: Process {} unresponsive, waiting for user action",
                                child_pid_u32
                            );
                            // In TUI mode, this would prompt the user
                        }
                        LockupAction::Restart | LockupAction::RestartWithBackoff | LockupAction::Kill => {
                            warn!(
                                "Watchdog triggering {:?} for unresponsive process {}",
                                action, child_pid_u32
                            );
                            watchdog.stop_monitoring();

                            // Kill the process
                            let _ = signal::kill(child_pid, Signal::SIGINT);
                            let start = std::time::Instant::now();
                            loop {
                                match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                                    Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                                    Ok(WaitStatus::StillAlive) => {
                                        if start.elapsed() > Duration::from_secs(2) {
                                            let _ = signal::kill(child_pid, Signal::SIGKILL);
                                            break;
                                        }
                                        std::thread::sleep(Duration::from_millis(50));
                                    }
                                    _ => break,
                                }
                            }

                            return Ok(ExitReason::WatchdogTriggered { action });
                        }
                    }
                }
            }
        }

        // Check if child has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                watchdog.stop_monitoring();
                let code = status.code().unwrap_or(1);
                return Ok(ExitReason::NormalExit(code));
            }
            Ok(None) => {
                // Still running, record activity and sleep briefly
                // The actual activity tracking happens via the watchdog
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error checking child status: {}", e));
            }
        }
    }
}

