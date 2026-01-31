//! Network Monitoring Module
//!
//! Coordinates network monitoring via LD_PRELOAD or network namespaces.

pub mod netns;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process;

/// Environment variable for the netmon log path
pub const NETMON_LOG_ENV: &str = "AEGIS_NETMON_LOG";

/// Network monitoring mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetmonMode {
    /// Use LD_PRELOAD to intercept network calls (works as non-root)
    Preload,
    /// Use network namespace for full isolation (requires root)
    Namespace,
}

impl std::fmt::Display for NetmonMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetmonMode::Preload => write!(f, "preload"),
            NetmonMode::Namespace => write!(f, "namespace"),
        }
    }
}

/// Network monitoring configuration
#[derive(Debug, Clone)]
pub struct NetmonConfig {
    /// The monitoring mode
    pub mode: NetmonMode,
    /// Path to the netmon library (for preload mode)
    pub library_path: PathBuf,
    /// Path to the log file
    pub log_path: PathBuf,
}

impl NetmonConfig {
    /// Create a new netmon configuration
    pub fn new(mode: NetmonMode) -> Result<Self> {
        let library_path = find_netmon_library()?;
        let log_path = PathBuf::from(format!(
            "/tmp/aegis-netmon-{}.jsonl",
            process::id()
        ));

        Ok(Self {
            mode,
            library_path,
            log_path,
        })
    }

    /// Auto-detect the best mode based on privileges
    pub fn auto() -> Result<Self> {
        let mode = if nix::unistd::Uid::effective().is_root() {
            // Root can use namespace mode for better isolation
            // But preload is simpler and works well, so default to preload
            NetmonMode::Preload
        } else {
            NetmonMode::Preload
        };
        Self::new(mode)
    }

    /// Get the environment variables to set for the child process
    pub fn env_vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        match self.mode {
            NetmonMode::Preload => {
                vars.insert(
                    "LD_PRELOAD".to_string(),
                    self.library_path.to_string_lossy().to_string(),
                );
                vars.insert(
                    NETMON_LOG_ENV.to_string(),
                    self.log_path.to_string_lossy().to_string(),
                );
            }
            NetmonMode::Namespace => {
                // Namespace mode doesn't need LD_PRELOAD
                // Traffic is captured via the network namespace
                vars.insert(
                    NETMON_LOG_ENV.to_string(),
                    self.log_path.to_string_lossy().to_string(),
                );
            }
        }

        vars
    }
}

/// Find the hooks library (libaegis_hooks.so)
fn find_netmon_library() -> Result<PathBuf> {
    // Try common locations
    let candidates = [
        // Next to the aegis-mcp binary
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("libaegis_hooks.so"))),
        // In ~/.local/lib (common user install location)
        dirs::home_dir().map(|h| h.join(".local/lib/libaegis_hooks.so")),
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

    Err(anyhow!(
        "Could not find libaegis_hooks.so. Build it with: cargo build -p aegis-hooks --release"
    ))
}

/// Network event from the log file
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum NetEvent {
    #[serde(rename = "connect")]
    Connect {
        ts: u64,
        fd: i32,
        addr: String,
        port: u16,
        family: String,
        result: i32,
    },
    #[serde(rename = "send")]
    Send {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "recv")]
    Recv {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "sendto")]
    SendTo {
        ts: u64,
        fd: i32,
        bytes: usize,
        addr: Option<String>,
        port: Option<u16>,
        result: isize,
    },
    #[serde(rename = "recvfrom")]
    RecvFrom {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "close")]
    Close { ts: u64, fd: i32, result: i32 },
}

/// Statistics from network monitoring
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetmonStats {
    /// Total number of connections
    pub connections: usize,
    /// Unique addresses connected to
    pub unique_addresses: usize,
    /// Total bytes sent
    pub bytes_sent: usize,
    /// Total bytes received
    pub bytes_received: usize,
    /// Connection targets (addr:port -> count)
    pub targets: HashMap<String, usize>,
}

/// Read and parse the netmon log file
pub fn read_log(log_path: &PathBuf) -> Result<Vec<NetEvent>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(log_path).context("Failed to open netmon log")?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        if let Ok(line) = line {
            if let Ok(event) = serde_json::from_str::<NetEvent>(&line) {
                events.push(event);
            }
        }
    }

    Ok(events)
}

/// Calculate statistics from network events
pub fn calculate_stats(events: &[NetEvent]) -> NetmonStats {
    let mut stats = NetmonStats::default();
    let mut addresses = std::collections::HashSet::new();

    for event in events {
        match event {
            NetEvent::Connect { addr, port, .. } => {
                stats.connections += 1;
                addresses.insert(addr.clone());
                let target = format!("{}:{}", addr, port);
                *stats.targets.entry(target).or_insert(0) += 1;
            }
            NetEvent::Send { result, .. } | NetEvent::SendTo { result, .. } => {
                if *result > 0 {
                    stats.bytes_sent += *result as usize;
                }
            }
            NetEvent::Recv { result, .. } | NetEvent::RecvFrom { result, .. } => {
                if *result > 0 {
                    stats.bytes_received += *result as usize;
                }
            }
            _ => {}
        }
    }

    stats.unique_addresses = addresses.len();
    stats
}

/// Get recent network events (last N)
pub fn recent_events(log_path: &PathBuf, count: usize) -> Result<Vec<NetEvent>> {
    let events = read_log(log_path)?;
    let start = events.len().saturating_sub(count);
    Ok(events[start..].to_vec())
}

/// Format a summary of network activity
pub fn format_summary(log_path: &PathBuf) -> Result<String> {
    let events = read_log(log_path)?;
    let stats = calculate_stats(&events);

    let mut output = String::new();
    output.push_str(&format!("Network Monitoring Summary\n"));
    output.push_str(&format!("==========================\n\n"));
    output.push_str(&format!("Total connections: {}\n", stats.connections));
    output.push_str(&format!("Unique addresses: {}\n", stats.unique_addresses));
    output.push_str(&format!("Bytes sent: {}\n", format_bytes(stats.bytes_sent)));
    output.push_str(&format!(
        "Bytes received: {}\n",
        format_bytes(stats.bytes_received)
    ));

    if !stats.targets.is_empty() {
        output.push_str(&format!("\nTop connection targets:\n"));
        let mut targets: Vec<_> = stats.targets.iter().collect();
        targets.sort_by(|a, b| b.1.cmp(a.1));
        for (target, count) in targets.iter().take(10) {
            output.push_str(&format!("  {} ({} connections)\n", target, count));
        }
    }

    Ok(output)
}

/// Format bytes in human-readable form
fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn test_netmon_mode_display() {
        assert_eq!(NetmonMode::Preload.to_string(), "preload");
        assert_eq!(NetmonMode::Namespace.to_string(), "namespace");
    }
}
