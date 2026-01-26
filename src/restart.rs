use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use tracing::{debug, info};

#[derive(Debug, Serialize)]
pub struct RestartInfo {
    pub claude_pid: u32,
    pub working_dir: String,
}

#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub server_pid: u32,
    pub claude_code_pid: Option<u32>,
    pub claude_code_exe: Option<String>,
    pub working_directory: Option<String>,
}

/// Get the parent process (Claude Code) PID
fn get_parent_pid() -> Option<u32> {
    // Read /proc/self/stat to get parent PID
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Format: pid (comm) state ppid ...
    // Find the closing paren, then split
    let close_paren = stat.rfind(')')?;
    let after_comm = &stat[close_paren + 2..];
    let parts: Vec<&str> = after_comm.split_whitespace().collect();
    // parts[0] = state, parts[1] = ppid
    parts.get(1)?.parse().ok()
}

/// Get the executable path of a process
fn get_exe_path(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{}/exe", pid))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Get the current working directory of a process
fn get_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Get the command line of a process
fn get_cmdline(pid: u32) -> Option<Vec<String>> {
    fs::read_to_string(format!("/proc/{}/cmdline", pid))
        .ok()
        .map(|s| {
            s.split('\0')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
}

/// Get current server status
pub fn get_status() -> ServerStatus {
    let server_pid = std::process::id();
    let claude_code_pid = get_parent_pid();
    let claude_code_exe = claude_code_pid.and_then(get_exe_path);
    let working_directory = claude_code_pid.and_then(get_cwd);

    ServerStatus {
        server_pid,
        claude_code_pid,
        claude_code_exe,
        working_directory,
    }
}

/// Trigger a restart of Claude Code
///
/// This forks a detached daemon process that will:
/// 1. Wait for the specified delay
/// 2. Kill the Claude Code process
/// 3. Restart Claude Code with the same working directory
/// 4. Exit
pub fn trigger_restart(delay_ms: u32) -> Result<RestartInfo> {
    let parent_pid = get_parent_pid()
        .context("Failed to get parent (Claude Code) PID")?;

    let working_dir = get_cwd(parent_pid)
        .context("Failed to get Claude Code working directory")?;

    let exe_path = get_exe_path(parent_pid)
        .context("Failed to get Claude Code executable path")?;

    let cmdline = get_cmdline(parent_pid)
        .context("Failed to get Claude Code command line")?;

    info!(
        parent_pid = parent_pid,
        working_dir = %working_dir,
        exe = %exe_path,
        cmdline = ?cmdline,
        "Preparing to restart Claude Code"
    );

    // Fork a detached daemon process
    match unsafe { libc::fork() } {
        -1 => {
            return Err(anyhow::anyhow!("Fork failed"));
        }
        0 => {
            // Child process - become a daemon

            // Create new session (detach from parent)
            unsafe { libc::setsid() };

            // Fork again to ensure we're not a session leader
            match unsafe { libc::fork() } {
                -1 => std::process::exit(1),
                0 => {
                    // Grandchild - this is our daemon

                    // Close stdin/stdout/stderr
                    unsafe {
                        libc::close(0);
                        libc::close(1);
                        libc::close(2);
                    }

                    // Wait for the delay
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms as u64));

                    // Kill Claude Code
                    unsafe {
                        libc::kill(parent_pid as i32, libc::SIGTERM);
                    }

                    // Wait a bit for it to die
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Check if it's still running, force kill if needed
                    let still_running = fs::metadata(format!("/proc/{}", parent_pid)).is_ok();
                    if still_running {
                        unsafe {
                            libc::kill(parent_pid as i32, libc::SIGKILL);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }

                    // Restart Claude Code
                    let mut cmd = Command::new(&exe_path);
                    cmd.current_dir(&working_dir);

                    // Add original args (skip the exe itself)
                    if cmdline.len() > 1 {
                        cmd.args(&cmdline[1..]);
                    }

                    // Execute (replaces this process)
                    let err = cmd.exec();

                    // If we get here, exec failed
                    eprintln!("Failed to restart Claude Code: {}", err);
                    std::process::exit(1);
                }
                _ => {
                    // First child - exit immediately
                    std::process::exit(0);
                }
            }
        }
        child_pid => {
            // Parent process - wait for first child to exit
            debug!(child_pid = child_pid, "Forked restart daemon");
            unsafe {
                let mut status: i32 = 0;
                libc::waitpid(child_pid, &mut status, 0);
            }
        }
    }

    Ok(RestartInfo {
        claude_pid: parent_pid,
        working_dir,
    })
}
