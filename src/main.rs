mod mcp_server;
mod netmon;
mod pool;
mod privileges;
mod restart;
mod tui;
mod watchdog;
mod wrapper;

use anyhow::Result;
use std::env;
use tracing::Level;
use tracing_subscriber::EnvFilter;

fn print_usage() {
    eprintln!("aegis-mcp - Agent supervisor with hot-reload support for MCP servers\n");
    eprintln!("USAGE:");
    eprintln!("  aegis-mcp <agent> [options] [agent-args...]   Run as wrapper for the specified agent");
    eprintln!("  aegis-mcp --mcp-server                        Run as MCP server (used by agents)");
    eprintln!("  aegis-mcp --dashboard [wrapper-pid]           Run TUI dashboard (monitor running wrapper)");
    eprintln!("  aegis-mcp --version                           Show version information\n");
    eprintln!("SUPPORTED AGENTS:");
    eprintln!("  claude    Claude Code CLI");
    eprintln!("  cursor    Cursor editor");
    eprintln!("  aider     Aider CLI\n");
    eprintln!("OPTIONS:");
    eprintln!("  --keep-root          Stay root instead of dropping privileges (when run with sudo)");
    eprintln!("  --no-inject-mcp      Don't auto-inject aegis-mcp as an MCP server");
    eprintln!("  --netmon             Enable network monitoring (auto-detect mode)");
    eprintln!("  --netmon=preload     Force LD_PRELOAD mode for network monitoring");
    eprintln!("  --netmon=netns       Force network namespace mode (requires root)");
    eprintln!("  --watchdog-timeout   Watchdog timeout in seconds (default: 60)");
    eprintln!("  --no-watchdog        Disable watchdog monitoring\n");
    eprintln!("EXAMPLES:");
    eprintln!("  aegis-mcp claude --continue");
    eprintln!("  aegis-mcp claude -p \"Help me with...\"");
    eprintln!("  aegis-mcp aider --model gpt-4");
    eprintln!("  aegis-mcp claude --netmon          # Monitor network with LD_PRELOAD");
    eprintln!("  aegis-mcp --dashboard              # Open TUI dashboard (auto-detect wrapper)");
    eprintln!("  aegis-mcp --dashboard 12345        # Monitor specific wrapper PID");
    eprintln!("  sudo aegis-mcp claude              # Drops to original user before spawning");
    eprintln!("  sudo aegis-mcp claude --keep-root  # Stays root (advanced/debugging)");
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Check for --version flag
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        wrapper::print_version_info();
        return Ok(());
    }

    // Check if running as MCP server
    let is_mcp_server = args.iter().any(|arg| arg == "--mcp-server");

    // Check if running as dashboard
    let is_dashboard = args.iter().any(|arg| arg == "--dashboard");

    if is_mcp_server {
        // MCP server mode - log to stderr (stdout is for MCP protocol)
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env()
                    .add_directive(Level::INFO.into())
            )
            .with_writer(std::io::stderr)
            .with_target(false)
            .init();

        mcp_server::run()
    } else if is_dashboard {
        // Dashboard mode - find or use specified wrapper PID
        let wrapper_pid = args
            .iter()
            .position(|a| a == "--dashboard")
            .and_then(|pos| args.get(pos + 1))
            .and_then(|pid_str| pid_str.parse::<u32>().ok())
            .or_else(find_running_wrapper);

        match wrapper_pid {
            Some(pid) => {
                eprintln!("Connecting to wrapper PID: {}", pid);
                // Create a dummy watchdog for the dashboard
                let watchdog = watchdog::create_watchdog();
                tui::run_dashboard(watchdog, pid)
            }
            None => {
                eprintln!("Error: No running aegis-mcp wrapper found.");
                eprintln!("Start a wrapper first with: aegis-mcp <agent>");
                eprintln!("Or specify a PID: aegis-mcp --dashboard <pid>");
                std::process::exit(1);
            }
        }
    } else {
        // Wrapper mode - log to stderr to not interfere with terminal
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env()
                    .add_directive(Level::WARN.into())
            )
            .with_writer(std::io::stderr)
            .with_target(false)
            .init();

        // First argument (after program name) is the agent name
        if args.len() < 2 {
            print_usage();
            std::process::exit(1);
        }

        let agent = args[1].clone();

        // Check for help
        if agent == "--help" || agent == "-h" {
            print_usage();
            return Ok(());
        }

        // Parse aegis-mcp specific options and collect agent args
        let remaining_args: Vec<String> = args.into_iter().skip(2).collect();
        let keep_root = remaining_args.iter().any(|a| a == "--keep-root");
        let inject_mcp = !remaining_args.iter().any(|a| a == "--no-inject-mcp");
        let no_watchdog = remaining_args.iter().any(|a| a == "--no-watchdog");

        // Parse --netmon option
        let netmon_mode = remaining_args
            .iter()
            .find(|a| a.starts_with("--netmon"))
            .map(|a| {
                if a == "--netmon" {
                    netmon::NetmonMode::Preload // Default to preload
                } else if a == "--netmon=preload" {
                    netmon::NetmonMode::Preload
                } else if a == "--netmon=netns" {
                    netmon::NetmonMode::Namespace
                } else {
                    eprintln!("Unknown netmon mode: {}. Using preload.", a);
                    netmon::NetmonMode::Preload
                }
            });

        // Parse --watchdog-timeout option
        let watchdog_timeout = remaining_args
            .iter()
            .find(|a| a.starts_with("--watchdog-timeout="))
            .and_then(|a| a.strip_prefix("--watchdog-timeout="))
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(60);

        // Build watchdog config
        let mut watchdog_config = watchdog::WatchdogConfig::default();
        watchdog_config.enabled = !no_watchdog;
        watchdog_config.heartbeat_timeout = std::time::Duration::from_secs(watchdog_timeout);

        // Filter out aegis-mcp options, pass remaining to agent
        let agent_args: Vec<String> = remaining_args
            .into_iter()
            .filter(|a| {
                a != "--keep-root"
                    && a != "--no-inject-mcp"
                    && a != "--no-watchdog"
                    && !a.starts_with("--netmon")
                    && !a.starts_with("--watchdog-timeout")
            })
            .collect();

        wrapper::run_with_watchdog(agent, agent_args, keep_root, netmon_mode, inject_mcp, watchdog_config)
    }
}

/// Find a running aegis-mcp wrapper by scanning /tmp for state files
fn find_running_wrapper() -> Option<u32> {
    let prefix = "/tmp/aegis-mcp-state-";

    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                if filename.starts_with("aegis-mcp-state-") {
                    if let Some(pid_str) = filename.strip_prefix("aegis-mcp-state-") {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            // Verify the process is still running
                            if std::fs::metadata(format!("/proc/{}", pid)).is_ok() {
                                return Some(pid);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}
