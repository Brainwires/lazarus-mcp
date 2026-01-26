use anyhow::{Context, Result};
use nix::libc;
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::sys::termios::{self, SetArg};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{dup2, execvp, fork, setsid, ForkResult, Pid};
use serde_json;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

// Global to store the master PTY fd for SIGWINCH handler
static MASTER_PTY_FD: AtomicI32 = AtomicI32::new(-1);

const SIGNAL_FILE_PREFIX: &str = "/tmp/rusty-restart-claude-";

/// Get the current terminal window size
fn get_terminal_size() -> Option<Winsize> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(stdin_fd, libc::TIOCGWINSZ, &mut ws) };
    if result == 0 {
        Some(Winsize {
            ws_row: ws.ws_row,
            ws_col: ws.ws_col,
            ws_xpixel: ws.ws_xpixel,
            ws_ypixel: ws.ws_ypixel,
        })
    } else {
        None
    }
}

/// Set the window size on a PTY
fn set_pty_size(fd: i32, ws: &Winsize) {
    let winsize = libc::winsize {
        ws_row: ws.ws_row,
        ws_col: ws.ws_col,
        ws_xpixel: ws.ws_xpixel,
        ws_ypixel: ws.ws_ypixel,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) };
}

/// SIGWINCH handler - propagate terminal resize to PTY
extern "C" fn handle_sigwinch(_: libc::c_int) {
    let master_fd = MASTER_PTY_FD.load(Ordering::SeqCst);
    if master_fd >= 0 {
        if let Some(ws) = get_terminal_size() {
            set_pty_size(master_fd, &ws);
        }
    }
}

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
pub async fn run(claude_args: Vec<String>) -> Result<()> {
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

        // Add --continue on restarts if not already present
        if add_continue && !args.iter().any(|a| a == "--continue" || a == "-c") {
            args.push("--continue".to_string());
        }

        info!("Starting claude with args: {:?}", args);

        // Run claude with PTY, passing any pending prompt
        let exit_reason = run_claude_with_pty(&claude_path, &args, running.clone(), pending_prompt.take())?;

        match exit_reason {
            ExitReason::RestartRequested { reason, prompt } => {
                info!("Restart requested: {}", reason);
                add_continue = true;
                pending_prompt = prompt;
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

fn run_claude_with_pty(
    claude_path: &PathBuf,
    args: &[String],
    running: Arc<AtomicBool>,
    inject_prompt: Option<String>,
) -> Result<ExitReason> {
    // Get terminal size from real terminal
    let winsize = get_terminal_size();

    // Open a PTY pair with the terminal size
    let OpenptyResult { master, slave } = openpty(winsize.as_ref(), None)?;

    // Store master fd for SIGWINCH handler
    MASTER_PTY_FD.store(master.as_raw_fd(), Ordering::SeqCst);

    // Set up SIGWINCH handler to propagate terminal resizes
    unsafe {
        libc::signal(libc::SIGWINCH, handle_sigwinch as libc::sighandler_t);
    }

    // Save current terminal settings to restore later
    let stdin = std::io::stdin();
    let original_termios = termios::tcgetattr(&stdin).ok();

    // Put terminal in raw mode for passthrough
    if let Some(ref orig) = original_termios {
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw)?;
    }

    // Fork
    match unsafe { fork()? } {
        ForkResult::Child => {
            // Child process - run claude

            // Close master side
            drop(master);

            // Create new session
            setsid()?;

            // Set slave as controlling terminal
            unsafe {
                libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
            }

            // Redirect stdio to PTY slave
            dup2(slave.as_raw_fd(), 0)?; // stdin
            dup2(slave.as_raw_fd(), 1)?; // stdout
            dup2(slave.as_raw_fd(), 2)?; // stderr

            // Close original slave fd (we've duped it)
            drop(slave);

            // Execute claude
            let program = CString::new(claude_path.to_str().unwrap())?;
            let mut c_args: Vec<CString> = vec![program.clone()];
            for arg in args {
                c_args.push(CString::new(arg.as_str())?);
            }

            execvp(&program, &c_args)?;

            // execvp doesn't return on success
            unreachable!()
        }
        ForkResult::Parent { child } => {
            // Parent process - forward I/O

            // Close slave side
            drop(slave);

            let result = forward_io(master, child, running, inject_prompt);

            // Clear the global master fd
            MASTER_PTY_FD.store(-1, Ordering::SeqCst);

            // Restore terminal settings
            if let Some(ref orig) = original_termios {
                let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, orig);
            }

            result
        }
    }
}

fn forward_io(
    master: OwnedFd,
    child: Pid,
    running: Arc<AtomicBool>,
    inject_prompt: Option<String>,
) -> Result<ExitReason> {
    let master_fd = master.as_raw_fd();

    // Create an OwnedFd for stdin (we'll be careful not to close it)
    let stdin_fd = std::io::stdin().as_raw_fd();

    // Set non-blocking on master and stdin using libc directly
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

        let flags = libc::fcntl(stdin_fd, libc::F_GETFL);
        libc::fcntl(stdin_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut buf = [0u8; 4096];
    let mut last_signal_check = Instant::now();

    // Track if we need to inject a prompt after startup
    let mut prompt_to_inject = inject_prompt;
    let mut startup_time = Instant::now();
    let mut prompt_injected = false;

    loop {
        // Check if wrapper should stop
        if !running.load(Ordering::SeqCst) {
            // Send SIGINT to child
            let _ = signal::kill(child, Signal::SIGINT);
            return Ok(ExitReason::WrapperShutdown);
        }

        // Check for restart signal periodically
        if last_signal_check.elapsed() > Duration::from_millis(100) {
            last_signal_check = Instant::now();

            if let Some(signal_content) = check_restart_signal() {
                // Send SIGINT to claude for graceful shutdown
                let _ = signal::kill(child, Signal::SIGINT);

                // Wait for it to exit
                let start = Instant::now();
                loop {
                    match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                        Ok(WaitStatus::StillAlive) => {
                            if start.elapsed() > Duration::from_secs(3) {
                                // Try SIGTERM
                                let _ = signal::kill(child, Signal::SIGTERM);
                            }
                            if start.elapsed() > Duration::from_secs(5) {
                                // Force kill
                                let _ = signal::kill(child, Signal::SIGKILL);
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
        }

        // Check if child exited
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                return Ok(ExitReason::NormalExit(code));
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                return Ok(ExitReason::Signal(sig));
            }
            Ok(WaitStatus::StillAlive) => {}
            Err(_) => {
                return Ok(ExitReason::NormalExit(0));
            }
            _ => {}
        }

        // Use poll to wait for data (simpler than select)
        let mut poll_fds = [
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 50) };

        if ready <= 0 {
            continue;
        }

        // Inject prompt after Claude has had time to initialize
        // We wait 1.5 seconds after startup to ensure Claude is ready
        if !prompt_injected && prompt_to_inject.is_some() && startup_time.elapsed() > Duration::from_millis(1500) {
            if let Some(prompt) = prompt_to_inject.take() {
                info!("Injecting prompt after restart: {}", prompt);
                let msg = format!("{}\n", prompt);
                unsafe { libc::write(master_fd, msg.as_ptr() as *const _, msg.len()) };
                prompt_injected = true;
            }
        }

        // Read from PTY master (claude's output) and write to stdout
        if poll_fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let _ = std::io::stdout().write_all(&buf[..n as usize]);
                let _ = std::io::stdout().flush();
            } else if n == 0 {
                // EOF from PTY
                return Ok(ExitReason::NormalExit(0));
            }
        }

        // Read from stdin and write to PTY master (claude's input)
        if poll_fds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                unsafe { libc::write(master_fd, buf.as_ptr() as *const _, n as usize) };
            }
        }
    }
}
