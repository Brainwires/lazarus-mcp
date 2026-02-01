use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{self, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

use crate::privileges;

// ============================================================================
// Crash Cleanup Registry
// ============================================================================

/// Global registry for cleanup on crash/exit
static CLEANUP_REGISTRY: Mutex<Option<CleanupRegistry>> = Mutex::new(None);

#[derive(Default)]
struct CleanupRegistry {
    mcp_backup_path: Option<PathBuf>,
    mcp_target_path: Option<PathBuf>,
}

/// Register for cleanup on crash
fn register_cleanup(backup_path: Option<PathBuf>, target_path: Option<PathBuf>) {
    if let Ok(mut guard) = CLEANUP_REGISTRY.lock() {
        *guard = Some(CleanupRegistry {
            mcp_backup_path: backup_path,
            mcp_target_path: target_path,
        });
    }
}

/// Perform emergency cleanup (called from panic hook or signal handler)
fn emergency_cleanup() {
    if let Ok(guard) = CLEANUP_REGISTRY.lock() {
        if let Some(ref registry) = *guard {
            // Restore .mcp.json from backup
            if let (Some(ref backup), Some(ref target)) = (&registry.mcp_backup_path, &registry.mcp_target_path) {
                if backup.exists() {
                    let _ = fs::copy(backup, target);
                    let _ = fs::remove_file(backup);
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

/// Target file for MCP config
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

/// Get the signal file path for this wrapper instance
pub fn signal_file_path() -> PathBuf {
    PathBuf::from(format!("{}{}", SIGNAL_FILE_PREFIX, process::id()))
}

/// Parsed restart signal
#[derive(Debug)]
struct ParsedRestartSignal {
    reason: String,
    prompt: Option<String>,
}

/// Backup path for .mcp.json
fn mcp_backup_path() -> PathBuf {
    PathBuf::from(".mcp.json.aegis-backup")
}

/// Restore .mcp.json from backup if a previous run crashed
fn restore_mcp_if_dirty() {
    let backup = mcp_backup_path();
    let target = Path::new(MCP_TARGET_FILE);

    if backup.exists() {
        warn!("Found .mcp.json backup from previous crash - restoring");

        // Check if backup is empty (marker meaning original didn't exist)
        let is_empty_marker = fs::metadata(&backup).map(|m| m.len() == 0).unwrap_or(false);

        if is_empty_marker {
            // Original didn't exist, delete the injected file
            let _ = fs::remove_file(target);
            info!("Removed injected .mcp.json (original didn't exist)");
        } else if let Err(e) = fs::copy(&backup, target) {
            warn!("Failed to restore .mcp.json from backup: {}", e);
        } else {
            info!("Restored .mcp.json from backup");
        }

        let _ = fs::remove_file(&backup);
    }
}

/// Inject aegis-mcp into .mcp.json (with backup for restore on exit)
fn inject_mcp_server() -> Result<(PathBuf, PathBuf)> {
    let aegis_path = std::env::current_exe()
        .context("Failed to get current executable path")?;

    let mcp_path = PathBuf::from(MCP_TARGET_FILE);
    let backup_path = mcp_backup_path();

    // Read existing config or create empty one
    let mut config: serde_json::Value = if mcp_path.exists() {
        // Backup the original first
        fs::copy(&mcp_path, &backup_path)
            .context("Failed to backup .mcp.json")?;

        let content = fs::read_to_string(&mcp_path)
            .context("Failed to read existing .mcp.json")?;
        serde_json::from_str(&content)
            .context("Failed to parse existing .mcp.json")?
    } else {
        // Create backup marker (empty file) so we know to delete .mcp.json on restore
        fs::write(&backup_path, "")
            .context("Failed to create backup marker")?;
        json!({ "mcpServers": {} })
    };

    // Ensure mcpServers object exists
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }

    // Inject aegis-mcp server
    config["mcpServers"]["aegis-mcp"] = json!({
        "command": aegis_path.to_string_lossy(),
        "args": ["--mcp-server"]
    });

    // Write modified config
    let content = serde_json::to_string_pretty(&config)?;
    fs::write(&mcp_path, &content)?;

    info!("Injected aegis-mcp into .mcp.json (backup at {})", backup_path.display());
    Ok((backup_path, mcp_path))
}

/// Remove aegis-mcp from .mcp.json (restore from backup)
fn restore_mcp_config(backup_path: &Path, target_path: &Path) {
    if backup_path.exists() {
        // Check if backup is empty (meaning original didn't exist)
        if fs::metadata(backup_path).map(|m| m.len() == 0).unwrap_or(false) {
            // Original didn't exist, delete the file we created
            let _ = fs::remove_file(target_path);
            info!("Removed injected .mcp.json (original didn't exist)");
        } else {
            // Restore from backup
            if let Err(e) = fs::copy(backup_path, target_path) {
                warn!("Failed to restore .mcp.json: {}", e);
            } else {
                info!("Restored original .mcp.json");
            }
        }
        let _ = fs::remove_file(backup_path);
    }
}

/// Display version information
pub fn print_version_info() {
    println!("aegis-mcp v{}", VERSION);
    println!("  Built: {}", BUILD_TIME);
    println!("  Git:   {}", GIT_HASH);
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

/// Run a command with supervision
pub fn run_command(
    command: PathBuf,
    cmd_args: Vec<String>,
    inject_mcp: bool,
) -> Result<()> {
    let command_name = command
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    info!("Command: {:?}", command);
    info!("Wrapper PID: {}", process::id());

    // Create shared state
    let mut shared_state = SharedState::new(&command_name);
    let _ = shared_state.save(); // Initial save

    // Drop root privileges if running as root
    if privileges::is_root() {
        info!("Running as root, will drop privileges before spawning agent");
        privileges::drop_privileges()?;
    }

    // Restore .mcp.json if a previous run crashed
    restore_mcp_if_dirty();

    // Inject aegis-mcp into .mcp.json
    let mcp_paths = if inject_mcp {
        match inject_mcp_server() {
            Ok(paths) => Some(paths),
            Err(e) => {
                warn!("Failed to inject MCP server: {}. Continuing without injection.", e);
                None
            }
        }
    } else {
        info!("MCP auto-injection disabled");
        None
    };

    // Install panic hook for crash cleanup
    install_panic_hook();

    // Register for cleanup on crash
    let (backup_path, target_path) = mcp_paths.clone().unzip();
    register_cleanup(backup_path, target_path);

    // Clean up any stale signal files
    let _ = fs::remove_file(signal_file_path());

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

    let mut pending_prompt: Option<String> = None;
    let mut final_exit_code: Option<i32> = None;

    while running.load(Ordering::SeqCst) {
        // Build args for this run
        let mut args = cmd_args.clone();

        // Add pending prompt as a command-line argument (for restart with prompt)
        if let Some(prompt) = pending_prompt.take() {
            info!("Adding prompt as command-line argument: {}", prompt);
            args.push(prompt);
        }

        info!("Starting {} with args: {:?}", command_name, args);

        // Update shared state
        shared_state.agent_status = AgentState::Starting;
        let _ = shared_state.save();

        // Spawn command
        let exit_reason = run_agent(
            &command,
            &args,
            running.clone(),
            &mut shared_state,
        )?;

        match exit_reason {
            ExitReason::RestartRequested { reason, prompt } => {
                info!("Restart requested: {}", reason);
                shared_state.restart_count += 1;
                shared_state.agent_status = AgentState::Restarting;
                let _ = shared_state.save();

                pending_prompt = prompt;

                // Clear terminal and reset before restart
                print!("\x1b[2J\x1b[H\x1b[0m");
                let _ = std::io::stdout().flush();

                // Small delay before restart
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            ExitReason::NormalExit(code) => {
                info!("{} exited with code: {}", command_name, code);
                shared_state.agent_status = AgentState::Stopped;
                let _ = shared_state.save();
                final_exit_code = Some(code);
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
    let _ = fs::remove_file(SharedState::state_file_path());

    // Restore .mcp.json from backup
    if let Some((ref backup_path, ref target_path)) = mcp_paths {
        restore_mcp_config(backup_path, target_path);
    }

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
    NormalExit(i32),
    WrapperShutdown,
}

/// Run an agent as a simple child process
fn run_agent(
    agent_path: &PathBuf,
    args: &[String],
    running: Arc<AtomicBool>,
    shared_state: &mut SharedState,
) -> Result<ExitReason> {
    // Build command
    let mut cmd = Command::new(agent_path);
    cmd.args(args);

    // Ensure ~/.local/bin is in PATH (for user-installed tools like claude)
    if let Ok(home) = std::env::var("HOME") {
        let local_bin = format!("{}/.local/bin", home);
        if PathBuf::from(&local_bin).exists() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            if !current_path.contains(&local_bin) {
                cmd.env("PATH", format!("{}:{}", local_bin, current_path));
            }
        }
    }

    // Spawn agent directly
    let mut child = cmd.spawn().context("Failed to spawn agent")?;

    let child_pid = Pid::from_raw(child.id() as i32);
    let child_pid_u32 = child.id();

    // Update shared state with agent PID
    shared_state.agent_pid = Some(child_pid_u32);
    shared_state.agent_status = AgentState::Running;
    let _ = shared_state.save();

    // Monitor the child process
    loop {
        // Check if wrapper should stop
        if !running.load(Ordering::SeqCst) {
            let _ = signal::kill(child_pid, Signal::SIGINT);
            return Ok(ExitReason::WrapperShutdown);
        }

        // Check for restart signal
        if let Some(signal_content) = check_restart_signal() {
            info!("Restart signal detected: {}", signal_content.reason);

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

        // Check if child has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(1);
                return Ok(ExitReason::NormalExit(code));
            }
            Ok(None) => {
                // Still running
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error checking child status: {}", e));
            }
        }
    }
}
