use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{self, json};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::netmon::NetmonMode;
use crate::privileges;

const SIGNAL_FILE_PREFIX: &str = "/tmp/aegis-mcp-";

/// Environment variable for MCP overlay file path
const MCP_OVERLAY_ENV: &str = "AEGIS_MCP_OVERLAY";
/// Environment variable for target file to overlay
const MCP_TARGET_ENV: &str = "AEGIS_MCP_TARGET";
/// Default target file for MCP overlay
const MCP_TARGET_FILE: &str = ".mcp.json";

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

/// Create the MCP server configuration JSON for the overlay
/// Reads the existing .mcp.json and merges aegis-mcp into it
fn create_mcp_config() -> Result<String> {
    let aegis_path = std::env::current_exe()
        .context("Failed to get current executable path")?;

    // Read existing .mcp.json if it exists
    let mut config: serde_json::Value = if Path::new(MCP_TARGET_FILE).exists() {
        let content = fs::read_to_string(MCP_TARGET_FILE)
            .context("Failed to read existing .mcp.json")?;
        serde_json::from_str(&content)
            .context("Failed to parse existing .mcp.json")?
    } else {
        // No existing file, create empty structure
        json!({ "mcpServers": {} })
    };

    // Ensure mcpServers object exists
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }

    // Inject aegis-mcp server (schema-compliant, no extra fields)
    config["mcpServers"]["aegis-mcp"] = json!({
        "command": aegis_path.to_string_lossy(),
        "args": ["--mcp-server"]
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

/// Run the wrapper
pub fn run(agent_name: String, agent_args: Vec<String>, keep_root: bool, netmon_mode: Option<NetmonMode>, inject_mcp: bool) -> Result<()> {
    let agent = find_agent(&agent_name)?;
    info!("Found {} at: {:?}", agent.name, agent.path);
    info!("Wrapper PID: {}", process::id());

    // Handle root privileges
    if privileges::is_root() {
        if keep_root {
            warn!("Running as root with --keep-root flag. Agent will run with elevated privileges.");
        } else {
            info!("Running as root, will drop privileges before spawning agent");
            privileges::drop_privileges()?;
        }
    }

    // Create MCP overlay file for process-isolated injection
    let mcp_overlay_file = if inject_mcp {
        match create_mcp_config() {
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
        }
    } else {
        info!("MCP auto-injection disabled");
        None
    };

    // Find hooks library if MCP injection or netmon is enabled
    let hooks_library = if mcp_overlay_file.is_some() || netmon_mode.is_some() {
        match find_hooks_library() {
            Ok(path) => {
                info!("Found hooks library: {}", path.display());
                Some(path)
            }
            Err(e) => {
                warn!("{}. Hooks-based features will be disabled.", e);
                None
            }
        }
    } else {
        None
    };

    // Clean up any stale signal file
    let _ = fs::remove_file(signal_file_path());

    // Set up signal handling for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }).context("Failed to set Ctrl+C handler")?;

    let mut add_continue = false;
    let mut pending_prompt: Option<String> = None;

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

        info!("Starting {} with args: {:?}", agent.name, args);

        // Build environment variables for the agent
        let mut extra_env: HashMap<String, String> = HashMap::new();

        // Add LD_PRELOAD for hooks library (filesystem overlay for MCP injection)
        if let Some(ref lib_path) = hooks_library {
            extra_env.insert("LD_PRELOAD".to_string(), lib_path.to_string_lossy().to_string());
        }

        // Add MCP overlay environment variables
        if let Some(ref overlay_path) = mcp_overlay_file {
            extra_env.insert(MCP_OVERLAY_ENV.to_string(), overlay_path.to_string_lossy().to_string());
            extra_env.insert(MCP_TARGET_ENV.to_string(), MCP_TARGET_FILE.to_string());
        }

        // Spawn agent directly without any PTY or terminal emulation
        let exit_reason = run_agent(&agent.path, &args, &extra_env, running.clone())?;

        match exit_reason {
            ExitReason::RestartRequested { reason, prompt } => {
                info!("Restart requested: {}", reason);
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
            ExitReason::NormalExit(code) => {
                info!("{} exited with code: {}", agent.name, code);
                process::exit(code);
            }
            ExitReason::Signal(sig) => {
                info!("{} killed by signal: {:?}", agent.name, sig);
                break;
            }
            ExitReason::WrapperShutdown => {
                info!("Wrapper shutdown requested");
                break;
            }
        }
    }

    // Clean up signal file
    let _ = fs::remove_file(signal_file_path());

    // Clean up MCP overlay file
    if let Some(ref overlay_path) = mcp_overlay_file {
        let _ = fs::remove_file(overlay_path);
        info!("Cleaned up MCP overlay file");
    }

    Ok(())
}

#[derive(Debug)]
enum ExitReason {
    RestartRequested { reason: String, prompt: Option<String> },
    NormalExit(i32),
    Signal(Signal),
    WrapperShutdown,
}

/// Run an agent as a simple child process without any PTY or terminal emulation
fn run_agent(
    agent_path: &PathBuf,
    args: &[String],
    extra_env: &HashMap<String, String>,
    running: Arc<AtomicBool>,
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
                // Still running, sleep briefly
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error checking child status: {}", e));
            }
        }
    }
}

