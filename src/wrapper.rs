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

// Scrollback buffer size (lines)
const SCROLLBACK_LINES: usize = 10000;

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

fn run_claude_with_pty(
    claude_path: &PathBuf,
    args: &[String],
    running: Arc<AtomicBool>,
    inject_prompt: Option<String>,
) -> Result<ExitReason> {
    // Get terminal size from real terminal
    let winsize = get_terminal_size();
    let (rows, cols) = winsize
        .as_ref()
        .map(|ws| (ws.ws_row, ws.ws_col))
        .unwrap_or((24, 80));

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

            let result = forward_io(master, child, running, inject_prompt, rows, cols);

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

/// Scroll mode state
struct ScrollMode {
    active: bool,
    parser: vt100::Parser,
    offset: usize,
    rows: u16,
    cols: u16,
}

impl ScrollMode {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: false,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            offset: 0,
            rows,
            cols,
        }
    }

    fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn enter(&mut self) {
        if !self.active {
            self.active = true;
            self.offset = 0;
            // Save cursor and switch to alternate screen for our scroll view
            print!("\x1b[?1049h\x1b[H");
            let _ = std::io::stdout().flush();
            self.render();
        }
    }

    fn exit(&mut self) {
        if self.active {
            self.active = false;
            self.offset = 0;
            self.parser.screen_mut().set_scrollback(0);
            // Return from alternate screen
            print!("\x1b[?1049l");
            let _ = std::io::stdout().flush();
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        // Get total scrollback available by checking how far we can scroll
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let actual_max = self.parser.screen().scrollback();

        self.offset = std::cmp::min(self.offset + lines, actual_max);
        self.parser.screen_mut().set_scrollback(self.offset);
        self.render();
    }

    fn scroll_down(&mut self, lines: usize) {
        if self.offset <= lines {
            self.offset = 0;
        } else {
            self.offset -= lines;
        }
        self.parser.screen_mut().set_scrollback(self.offset);

        if self.offset == 0 {
            self.exit();
        } else {
            self.render();
        }
    }

    fn render(&self) {
        // Clear screen and move to top
        print!("\x1b[2J\x1b[H");

        // Get formatted content with scrollback applied
        let screen = self.parser.screen();

        // Render each row
        for row in 0..self.rows.saturating_sub(1) {
            let row_content = screen.rows_formatted(0, self.cols).nth(row as usize);
            if let Some(content) = row_content {
                print!("{}\r\n", String::from_utf8_lossy(&content));
            } else {
                print!("\r\n");
            }
        }

        // Status line at bottom
        print!("\x1b[{};1H", self.rows);
        print!("\x1b[7m"); // Reverse video
        let status = format!(
            " SCROLL [{}] | PgUp/PgDn/Arrows to scroll | q/Esc to exit ",
            self.offset
        );
        print!("{:width$}", status, width = self.cols as usize);
        print!("\x1b[0m");

        let _ = std::io::stdout().flush();
    }
}

fn forward_io(
    master: OwnedFd,
    child: Pid,
    running: Arc<AtomicBool>,
    inject_prompt: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<ExitReason> {
    let master_fd = master.as_raw_fd();
    let stdin_fd = std::io::stdin().as_raw_fd();

    // Set non-blocking on master and stdin
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
    let startup_time = Instant::now();
    let mut prompt_injected = false;

    // Scroll mode with vt100 parser
    let mut scroll = ScrollMode::new(rows, cols);

    // Input buffer for escape sequence parsing
    let mut input_buf: Vec<u8> = Vec::new();

    loop {
        // Check if wrapper should stop
        if !running.load(Ordering::SeqCst) {
            let _ = signal::kill(child, Signal::SIGINT);
            return Ok(ExitReason::WrapperShutdown);
        }

        // Check for restart signal periodically
        if last_signal_check.elapsed() > Duration::from_millis(100) {
            last_signal_check = Instant::now();

            // Update terminal size
            if let Some(ws) = get_terminal_size() {
                if ws.ws_row != scroll.rows || ws.ws_col != scroll.cols {
                    scroll.resize(ws.ws_row, ws.ws_col);
                }
            }

            if let Some(signal_content) = check_restart_signal() {
                // Exit scroll mode if active
                if scroll.active {
                    scroll.exit();
                }

                // Send SIGINT to claude for graceful shutdown
                let _ = signal::kill(child, Signal::SIGINT);

                // Wait for it to exit
                let start = Instant::now();
                loop {
                    match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                        Ok(WaitStatus::StillAlive) => {
                            if start.elapsed() > Duration::from_secs(3) {
                                let _ = signal::kill(child, Signal::SIGTERM);
                            }
                            if start.elapsed() > Duration::from_secs(5) {
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

        // Poll for data
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
        if !prompt_injected && prompt_to_inject.is_some() && startup_time.elapsed() > Duration::from_secs(3) {
            if let Some(prompt) = prompt_to_inject.take() {
                info!("Injecting prompt after restart: {}", prompt);
                let _ = signal::kill(child, Signal::SIGWINCH);
                std::thread::sleep(Duration::from_millis(100));
                // Use \r (carriage return) - that's what Enter sends in raw terminal mode
                let msg = format!("{}\r", prompt);
                unsafe { libc::write(master_fd, msg.as_ptr() as *const _, msg.len()) };
                prompt_injected = true;
            }
        }

        // Read from PTY master (claude's output)
        if poll_fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let data = &buf[..n as usize];

                // Always feed to vt100 parser for scrollback
                scroll.process(data);

                // Pass through to stdout if not in scroll mode
                if !scroll.active {
                    let _ = std::io::stdout().write_all(data);
                    let _ = std::io::stdout().flush();
                }
            } else if n == 0 {
                return Ok(ExitReason::NormalExit(0));
            }
        }

        // Read from stdin
        if poll_fds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let data = &buf[..n as usize];
                input_buf.extend_from_slice(data);

                // Process input
                let mut i = 0;
                while i < input_buf.len() {
                    if scroll.active {
                        // Handle scroll mode input
                        match &input_buf[i..] {
                            // q or Esc to exit
                            [b'q', ..] | [b'Q', ..] => {
                                scroll.exit();
                                // Trigger redraw
                                let _ = signal::kill(child, Signal::SIGWINCH);
                                i += 1;
                            }
                            [0x1b, ..] if input_buf.len() == i + 1 => {
                                // Lone escape - exit scroll mode
                                scroll.exit();
                                let _ = signal::kill(child, Signal::SIGWINCH);
                                i += 1;
                            }
                            // Page Up
                            [0x1b, b'[', b'5', b'~', ..] => {
                                scroll.scroll_up(scroll.rows as usize - 1);
                                i += 4;
                            }
                            // Page Down
                            [0x1b, b'[', b'6', b'~', ..] => {
                                scroll.scroll_down(scroll.rows as usize - 1);
                                i += 4;
                            }
                            // Arrow Up
                            [0x1b, b'[', b'A', ..] => {
                                scroll.scroll_up(1);
                                i += 3;
                            }
                            // Arrow Down
                            [0x1b, b'[', b'B', ..] => {
                                scroll.scroll_down(1);
                                i += 3;
                            }
                            // Escape sequence we don't recognize - might be incomplete
                            [0x1b, ..] if input_buf.len() < i + 4 => {
                                // Keep in buffer for next read
                                break;
                            }
                            // Any other escape sequence - skip it
                            [0x1b, b'[', ..] => {
                                // Find end of CSI sequence
                                let mut j = i + 2;
                                while j < input_buf.len() {
                                    if (0x40..=0x7e).contains(&input_buf[j]) {
                                        j += 1;
                                        break;
                                    }
                                    j += 1;
                                }
                                i = j;
                            }
                            // Ignore other keys in scroll mode
                            _ => {
                                i += 1;
                            }
                        }
                    } else {
                        // Normal mode - check for scroll mode trigger (Page Up)
                        match &input_buf[i..] {
                            // Page Up enters scroll mode
                            [0x1b, b'[', b'5', b'~', ..] => {
                                scroll.enter();
                                scroll.scroll_up(scroll.rows as usize - 1);
                                i += 4;
                            }
                            // Everything else goes to Claude
                            _ => {
                                // Find how much to forward
                                let mut end = i + 1;
                                while end < input_buf.len() {
                                    // Stop at escape sequences that might be Page Up
                                    if input_buf[end] == 0x1b {
                                        break;
                                    }
                                    end += 1;
                                }
                                unsafe {
                                    libc::write(
                                        master_fd,
                                        input_buf[i..end].as_ptr() as *const _,
                                        end - i,
                                    )
                                };
                                i = end;
                            }
                        }
                    }
                }

                // Keep unprocessed bytes
                if i < input_buf.len() {
                    input_buf = input_buf[i..].to_vec();
                } else {
                    input_buf.clear();
                }
            }
        }
    }
}
