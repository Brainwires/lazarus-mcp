use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

const SIGNAL_FILE_PREFIX: &str = "/tmp/rusty-restart-claude-";


/// Get the signal file path for this wrapper instance
pub fn signal_file_path() -> PathBuf {
    PathBuf::from(format!("{}{}", SIGNAL_FILE_PREFIX, process::id()))
}

/// Find the claude executable
fn find_claude() -> Result<PathBuf> {
    // Try common locations
    let candidates = [
        // Check if 'claude' is in PATH
        which::which("claude").ok(),
        // Common install locations
        Some(PathBuf::from("/usr/local/bin/claude")),
        Some(PathBuf::from("/usr/bin/claude")),
        dirs::home_dir().map(|h| h.join(".local/bin/claude")),
        dirs::home_dir().map(|h| h.join(".local/share/claude/claude")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() && candidate.is_file() {
            return Ok(candidate);
        }
    }

    // Fallback: try to find in ~/.local/share/claude/versions/
    if let Some(home) = dirs::home_dir() {
        let versions_dir = home.join(".local/share/claude/versions");
        if versions_dir.exists() {
            // Find the latest version directory
            if let Ok(entries) = fs::read_dir(&versions_dir) {
                let mut versions: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .collect();
                versions.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

                if let Some(latest) = versions.first() {
                    let claude_path = latest.path().join("claude");
                    if claude_path.exists() {
                        return Ok(claude_path);
                    }
                }
            }
        }
    }

    anyhow::bail!("Could not find claude executable. Make sure Claude Code is installed.")
}

/// Parsed restart signal
#[derive(Debug)]
struct ParsedRestartSignal {
    reason: String,
    prompt: Option<String>,
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
pub fn run(claude_args: Vec<String>) -> Result<()> {
    let claude_path = find_claude()?;
    info!("Found claude at: {:?}", claude_path);
    info!("Wrapper PID: {}", process::id());

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
        let mut args = claude_args.clone();

        // Always add --allow-dangerously-skip-permissions if not already present
        if !args.iter().any(|a| a == "--allow-dangerously-skip-permissions") {
            args.push("--allow-dangerously-skip-permissions".to_string());
            info!("Auto-adding --allow-dangerously-skip-permissions flag");
        }

        // Add --continue on restarts if not already present
        if add_continue && !args.iter().any(|a| a == "--continue" || a == "-c") {
            args.push("--continue".to_string());
        }

        // Add pending prompt as a command-line argument
        if let Some(prompt) = pending_prompt.take() {
            info!("Adding prompt as command-line argument: {}", prompt);
            args.push(prompt);
        }

        info!("Starting claude with args: {:?}", args);

        // Spawn Claude directly without any PTY or terminal emulation
        let exit_reason = run_claude(&claude_path, &args, running.clone())?;

        match exit_reason {
            ExitReason::RestartRequested { reason, prompt } => {
                info!("Restart requested: {}", reason);
                add_continue = true;
                pending_prompt = prompt;

                // Clear terminal and reset before restart
                // This helps ensure Claude's TUI renders properly
                print!("\x1b[2J\x1b[H\x1b[0m");
                let _ = std::io::stdout().flush();

                // Small delay before restart
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            ExitReason::NormalExit(code) => {
                info!("Claude exited with code: {}", code);
                process::exit(code);
            }
            ExitReason::Signal(sig) => {
                info!("Claude killed by signal: {:?}", sig);
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

    Ok(())
}

#[derive(Debug)]
enum ExitReason {
    RestartRequested { reason: String, prompt: Option<String> },
    NormalExit(i32),
    Signal(Signal),
    WrapperShutdown,
}

/// Run Claude as a simple child process without any PTY or terminal emulation
fn run_claude(
    claude_path: &PathBuf,
    args: &[String],
    running: Arc<AtomicBool>,
) -> Result<ExitReason> {
    // Spawn Claude directly - no PTY, no terminal emulation
    let mut child = Command::new(claude_path)
        .args(args)
        .spawn()
        .context("Failed to spawn Claude")?;

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

            // Send SIGINT to Claude for graceful shutdown
            let _ = signal::kill(child_pid, Signal::SIGINT);

            // Wait for it to exit (with timeout escalation)
            let start = std::time::Instant::now();
            loop {
                match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                    Ok(WaitStatus::StillAlive) => {
                        if start.elapsed() > Duration::from_secs(3) {
                            info!("Claude not responding to SIGINT, sending SIGTERM");
                            let _ = signal::kill(child_pid, Signal::SIGTERM);
                        }
                        if start.elapsed() > Duration::from_secs(5) {
                            info!("Claude not responding to SIGTERM, sending SIGKILL");
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

