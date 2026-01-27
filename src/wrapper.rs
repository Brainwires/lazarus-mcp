use anyhow::{Context, Result};
use nix::libc;
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::sys::termios::{self, SetArg};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{dup2, execvp, fork, setsid, ForkResult, Pid};
use serde_json;
use std::collections::VecDeque;
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

// Scrollback buffer configuration
const SCROLLBACK_LINES: usize = 10000;

/// Scrollback buffer that stores terminal output line by line
struct ScrollBuffer {
    lines: VecDeque<Vec<u8>>,
    current_line: Vec<u8>,
    max_lines: usize,
    // Track if app is in alternate screen mode (no scrollback there)
    in_alternate_screen: bool,
}

impl ScrollBuffer {
    fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(max_lines),
            current_line: Vec::new(),
            max_lines,
            in_alternate_screen: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        // Check for alternate screen mode sequences
        for window in data.windows(8) {
            // Enter alternate screen: \x1b[?1049h or \x1b[?47h
            if window.starts_with(b"\x1b[?1049h") || window.starts_with(b"\x1b[?47h") {
                self.in_alternate_screen = true;
            }
            // Exit alternate screen: \x1b[?1049l or \x1b[?47l
            if window.starts_with(b"\x1b[?1049l") || window.starts_with(b"\x1b[?47l") {
                self.in_alternate_screen = false;
            }
        }

        // Don't buffer when in alternate screen mode
        if self.in_alternate_screen {
            return;
        }

        for &byte in data {
            if byte == b'\n' {
                // Finish current line and start new one
                let line = std::mem::take(&mut self.current_line);
                self.lines.push_back(line);
                if self.lines.len() > self.max_lines {
                    self.lines.pop_front();
                }
            } else if byte != b'\r' {
                // Add to current line (ignore carriage return)
                self.current_line.push(byte);
            }
        }
    }

    fn total_lines(&self) -> usize {
        self.lines.len()
    }

    fn get_lines(&self, start: usize, count: usize) -> Vec<&[u8]> {
        let mut result = Vec::new();
        for i in start..std::cmp::min(start + count, self.lines.len()) {
            if let Some(line) = self.lines.get(i) {
                result.push(line.as_slice());
            }
        }
        result
    }

    fn is_in_alternate_screen(&self) -> bool {
        self.in_alternate_screen
    }
}

/// Mouse event types we care about
#[derive(Debug, PartialEq)]
enum MouseEvent {
    ScrollUp,
    ScrollDown,
    Other,
}

/// Parse mouse events from input buffer
/// Returns (event, bytes_consumed)
fn parse_mouse_event(data: &[u8]) -> Option<(MouseEvent, usize)> {
    if data.len() < 3 {
        return None;
    }

    // SGR mouse mode: \x1b[<Cb;Cx;CyM or \x1b[<Cb;Cx;Cym
    if data.starts_with(b"\x1b[<") {
        // Find the end (M or m)
        if let Some(end_pos) = data.iter().position(|&b| b == b'M' || b == b'm') {
            let params = &data[3..end_pos];
            if let Ok(params_str) = std::str::from_utf8(params) {
                let parts: Vec<&str> = params_str.split(';').collect();
                if let Some(button_str) = parts.first() {
                    if let Ok(button) = button_str.parse::<u8>() {
                        // Button 64 = scroll up, 65 = scroll down
                        let event = match button & 0x43 {
                            64 => MouseEvent::ScrollUp,
                            65 => MouseEvent::ScrollDown,
                            _ => MouseEvent::Other,
                        };
                        return Some((event, end_pos + 1));
                    }
                }
            }
        }
        return None;
    }

    // X10 mouse mode: \x1b[M Cb Cx Cy (3 bytes after \x1b[M)
    if data.starts_with(b"\x1b[M") && data.len() >= 6 {
        let button = data[3].wrapping_sub(32);
        // Button 64 = scroll up, 65 = scroll down
        let event = match button & 0x43 {
            64 => MouseEvent::ScrollUp,
            65 => MouseEvent::ScrollDown,
            _ => MouseEvent::Other,
        };
        return Some((event, 6));
    }

    None
}

/// Scroll view state
struct ScrollView {
    active: bool,
    offset: usize, // Lines from bottom (0 = at bottom/live)
    term_rows: u16,
    term_cols: u16,
}

impl ScrollView {
    fn new() -> Self {
        Self {
            active: false,
            offset: 0,
            term_rows: 24,
            term_cols: 80,
        }
    }

    fn update_size(&mut self, rows: u16, cols: u16) {
        self.term_rows = rows;
        self.term_cols = cols;
    }

    fn scroll_up(&mut self, buffer: &ScrollBuffer, lines: usize) {
        let max_offset = buffer.total_lines().saturating_sub(self.term_rows as usize);
        self.offset = std::cmp::min(self.offset + lines, max_offset);
        self.active = true;
    }

    fn scroll_down(&mut self, lines: usize) {
        if self.offset <= lines {
            self.offset = 0;
            self.active = false;
        } else {
            self.offset -= lines;
        }
    }

    fn render(&self, buffer: &ScrollBuffer) {
        let total = buffer.total_lines();
        let visible_rows = self.term_rows as usize - 1; // Leave room for status line

        // Calculate which lines to show
        let end_line = total.saturating_sub(self.offset);
        let start_line = end_line.saturating_sub(visible_rows);

        // Save cursor, clear screen, move to top
        print!("\x1b[s\x1b[2J\x1b[H");

        // Render lines
        let lines = buffer.get_lines(start_line, visible_rows);
        for line in lines {
            // Strip any escape sequences for clean display in scroll mode
            let clean = strip_escapes(line);
            let display: String = clean.iter().map(|&b| b as char).collect();
            // Truncate to terminal width
            let truncated: String = display.chars().take(self.term_cols as usize).collect();
            println!("{}", truncated);
        }

        // Status line at bottom
        print!("\x1b[{};1H", self.term_rows);
        print!("\x1b[7m"); // Reverse video
        let status = format!(
            " SCROLL [{}/{}] - Mouse wheel or PgUp/PgDn to scroll, q/Esc to exit ",
            total.saturating_sub(self.offset),
            total
        );
        let padded: String = format!("{:width$}", status, width = self.term_cols as usize);
        print!("{}", &padded[..std::cmp::min(padded.len(), self.term_cols as usize)]);
        print!("\x1b[0m"); // Reset

        let _ = std::io::stdout().flush();
    }

    fn exit(&mut self) {
        self.active = false;
        self.offset = 0;
        // Restore cursor
        print!("\x1b[u");
        let _ = std::io::stdout().flush();
    }
}

/// Strip ANSI escape sequences from a byte slice
fn strip_escapes(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() {
            // Skip escape sequence
            if data[i + 1] == b'[' {
                // CSI sequence - find end
                i += 2;
                while i < data.len() {
                    let c = data[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        break;
                    }
                }
            } else if data[i + 1] == b']' {
                // OSC sequence - find ST or BEL
                i += 2;
                while i < data.len() {
                    if data[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if i + 1 < data.len() && data[i] == 0x1b && data[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 2;
            }
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}

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

    // Enable mouse reporting (SGR mode for better compatibility)
    print!("\x1b[?1000h\x1b[?1006h");
    let _ = std::io::stdout().flush();

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

            let result = forward_io(master, child, running, inject_prompt, winsize);

            // Clear the global master fd
            MASTER_PTY_FD.store(-1, Ordering::SeqCst);

            // Disable mouse reporting
            print!("\x1b[?1006l\x1b[?1000l");
            let _ = std::io::stdout().flush();

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
    winsize: Option<Winsize>,
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
    let startup_time = Instant::now();
    let mut prompt_injected = false;

    // Scrollback buffer and view
    let mut scroll_buffer = ScrollBuffer::new(SCROLLBACK_LINES);
    let mut scroll_view = ScrollView::new();
    if let Some(ref ws) = winsize {
        scroll_view.update_size(ws.ws_row, ws.ws_col);
    }

    // Input buffer for handling escape sequences that span reads
    let mut input_buffer: Vec<u8> = Vec::new();

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

            // Update terminal size for scroll view
            if let Some(ws) = get_terminal_size() {
                scroll_view.update_size(ws.ws_row, ws.ws_col);
            }

            if let Some(signal_content) = check_restart_signal() {
                // Exit scroll mode if active
                if scroll_view.active {
                    scroll_view.exit();
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
        // We wait 3 seconds after startup to ensure Claude's TUI is ready
        if !prompt_injected && prompt_to_inject.is_some() && startup_time.elapsed() > Duration::from_secs(3) {
            if let Some(prompt) = prompt_to_inject.take() {
                info!("Injecting prompt after restart: {}", prompt);

                // Send SIGWINCH to trigger TUI redraw before injecting
                let _ = signal::kill(child, Signal::SIGWINCH);
                std::thread::sleep(Duration::from_millis(100));

                let msg = format!("{}\n", prompt);
                unsafe { libc::write(master_fd, msg.as_ptr() as *const _, msg.len()) };
                prompt_injected = true;
            }
        }

        // Read from PTY master (claude's output) and write to stdout
        if poll_fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let data = &buf[..n as usize];

                // Add to scrollback buffer
                scroll_buffer.push(data);

                // Only show output if not in scroll view mode
                if !scroll_view.active {
                    let _ = std::io::stdout().write_all(data);
                    let _ = std::io::stdout().flush();
                }
            } else if n == 0 {
                // EOF from PTY
                return Ok(ExitReason::NormalExit(0));
            }
        }

        // Read from stdin and write to PTY master (claude's input)
        if poll_fds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                let data = &buf[..n as usize];
                input_buffer.extend_from_slice(data);

                // Process input buffer for mouse events and keys
                let mut i = 0;
                while i < input_buffer.len() {
                    // Check for mouse events
                    if let Some((event, consumed)) = parse_mouse_event(&input_buffer[i..]) {
                        match event {
                            MouseEvent::ScrollUp => {
                                if !scroll_buffer.is_in_alternate_screen() {
                                    scroll_view.scroll_up(&scroll_buffer, 3);
                                    scroll_view.render(&scroll_buffer);
                                }
                            }
                            MouseEvent::ScrollDown => {
                                if scroll_view.active {
                                    scroll_view.scroll_down(3);
                                    if scroll_view.active {
                                        scroll_view.render(&scroll_buffer);
                                    } else {
                                        // Exited scroll mode, redraw Claude's screen
                                        let _ = signal::kill(child, Signal::SIGWINCH);
                                    }
                                }
                            }
                            MouseEvent::Other => {
                                // Drop other mouse events - Claude's TUI handles mouse
                                // via its own terminal, not via stdin
                            }
                        }
                        i += consumed;
                        continue;
                    }

                    // Check for scroll mode exit keys when in scroll mode
                    if scroll_view.active {
                        let exit_scroll = match input_buffer[i] {
                            b'q' | b'Q' => true,
                            0x1b if i + 1 < input_buffer.len() => {
                                // Escape key (but not start of another sequence)
                                input_buffer.get(i + 1).map_or(true, |&b| b != b'[' && b != b'O')
                            }
                            0x1b => true, // Plain escape
                            _ => false,
                        };

                        if exit_scroll {
                            scroll_view.exit();
                            // Redraw Claude's screen
                            let _ = signal::kill(child, Signal::SIGWINCH);
                            i += 1;
                            continue;
                        }

                        // Handle Page Up/Down in scroll mode
                        if input_buffer[i..].starts_with(b"\x1b[5~") {
                            // Page Up
                            scroll_view.scroll_up(&scroll_buffer, scroll_view.term_rows as usize - 1);
                            scroll_view.render(&scroll_buffer);
                            i += 4;
                            continue;
                        }
                        if input_buffer[i..].starts_with(b"\x1b[6~") {
                            // Page Down
                            scroll_view.scroll_down(scroll_view.term_rows as usize - 1);
                            if scroll_view.active {
                                scroll_view.render(&scroll_buffer);
                            } else {
                                let _ = signal::kill(child, Signal::SIGWINCH);
                            }
                            i += 4;
                            continue;
                        }

                        // Ignore other input in scroll mode
                        i += 1;
                        continue;
                    }

                    // Not in scroll mode - forward to Claude
                    // Find the end of this input chunk (either end of buffer or start of escape)
                    let mut end = i + 1;
                    while end < input_buffer.len() && input_buffer[end] != 0x1b {
                        end += 1;
                    }
                    unsafe {
                        libc::write(
                            master_fd,
                            input_buffer[i..end].as_ptr() as *const _,
                            end - i,
                        )
                    };
                    i = end;
                }

                // Keep any incomplete escape sequence for next iteration
                input_buffer.clear();
            }
        }
    }
}
