use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

const SIGNAL_FILE_PREFIX: &str = "/tmp/aegis-mcp-";

#[derive(Debug, Serialize)]
pub struct RestartSignalInfo {
    pub wrapper_pid: u32,
    pub signal_file: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RestartSignal {
    pub action: String,
    pub timestamp: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub mcp_server_pid: u32,
    pub wrapper_pid: Option<u32>,
    pub wrapper_running: bool,
    pub signal_file_path: Option<String>,
    pub claude_code_pid: Option<u32>,
    pub working_directory: Option<String>,
}

/// Get the parent process PID (should be Claude Code when running as MCP server)
fn get_parent_pid() -> Option<u32> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let close_paren = stat.rfind(')')?;
    let after_comm = &stat[close_paren + 2..];
    let parts: Vec<&str> = after_comm.split_whitespace().collect();
    parts.get(1)?.parse().ok()
}

/// Get the current working directory of a process
fn get_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Get the command name of a process
fn get_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Find the wrapper PID by walking up the process tree
fn find_wrapper_pid() -> Option<u32> {
    // The process tree should be:
    // wrapper (aegis-mcp) -> claude -> MCP server (aegis-mcp --mcp-server)
    //
    // So we need to find a grandparent or great-grandparent that is aegis-mcp

    let mut current_pid = get_parent_pid()?; // Start with parent (should be claude)

    for _ in 0..5 {
        // Walk up to 5 levels
        let comm = get_comm(current_pid)?;

        // Check if this is the wrapper
        if comm.contains("aegis-mcp") {
            return Some(current_pid);
        }

        // Get parent of current
        let stat = fs::read_to_string(format!("/proc/{}/stat", current_pid)).ok()?;
        let close_paren = stat.rfind(')')?;
        let after_comm = &stat[close_paren + 2..];
        let parts: Vec<&str> = after_comm.split_whitespace().collect();
        current_pid = parts.get(1)?.parse().ok()?;

        // Stop if we hit init
        if current_pid <= 1 {
            break;
        }
    }

    None
}

/// Send a restart signal to the wrapper
pub fn send_restart_signal(reason: &str, prompt: Option<&str>) -> Result<RestartSignalInfo> {
    let wrapper_pid = find_wrapper_pid()
        .context("Could not find wrapper process. Make sure your agent was started via: aegis-mcp <agent> [args...]")?;

    let signal_file = format!("{}{}", SIGNAL_FILE_PREFIX, wrapper_pid);

    let signal = RestartSignal {
        action: "restart".to_string(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        reason: reason.to_string(),
        prompt: prompt.map(|s| s.to_string()),
    };

    let content = serde_json::to_string_pretty(&signal)?;

    info!(
        wrapper_pid = wrapper_pid,
        signal_file = %signal_file,
        "Writing restart signal"
    );

    fs::write(&signal_file, content)
        .context("Failed to write signal file")?;

    Ok(RestartSignalInfo {
        wrapper_pid,
        signal_file,
    })
}

/// Get current server status
pub fn get_status() -> ServerStatus {
    let mcp_server_pid = std::process::id();
    let claude_code_pid = get_parent_pid();
    let wrapper_pid = find_wrapper_pid();
    let working_directory = claude_code_pid.and_then(get_cwd);

    let wrapper_running = wrapper_pid
        .map(|pid| fs::metadata(format!("/proc/{}", pid)).is_ok())
        .unwrap_or(false);

    let signal_file_path = wrapper_pid.map(|pid| format!("{}{}", SIGNAL_FILE_PREFIX, pid));

    ServerStatus {
        mcp_server_pid,
        wrapper_pid,
        wrapper_running,
        signal_file_path,
        claude_code_pid,
        working_directory,
    }
}
