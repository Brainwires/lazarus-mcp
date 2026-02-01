mod mcp_server;
mod pool;
mod privileges;
mod restart;
mod tui;
mod wrapper;

use anyhow::Result;
use std::env;
use std::path::PathBuf;
use tracing::Level;
use tracing_subscriber::EnvFilter;

fn print_usage() {
    eprintln!("lazarus-mcp - Universal process supervisor\n");
    eprintln!("USAGE:");
    eprintln!("  lazarus-mcp [options] <command> [args...]   Run command with supervision");
    eprintln!("  lazarus-mcp --mcp-server                    Run as MCP server (used internally)");
    eprintln!("  lazarus-mcp --dashboard [wrapper-pid]       Run TUI dashboard");
    eprintln!("  lazarus-mcp --version                       Show version information\n");
    eprintln!("OPTIONS:");
    eprintln!("  --no-inject-mcp        Don't auto-inject lazarus-mcp as an MCP server\n");
    eprintln!("EXAMPLES:");
    eprintln!("  lazarus-mcp claude");
    eprintln!("  lazarus-mcp claude --continue");
    eprintln!("  lazarus-mcp --dashboard");
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Check for --version flag
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        wrapper::print_version_info();
        return Ok(());
    }

    // Check for help
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    // Check if running as MCP server
    if args.iter().any(|arg| arg == "--mcp-server") {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env()
                    .add_directive(Level::INFO.into())
            )
            .with_writer(std::io::stderr)
            .with_target(false)
            .init();

        return mcp_server::run();
    }

    // Check if running as dashboard
    if args.iter().any(|arg| arg == "--dashboard") {
        let wrapper_pid = args
            .iter()
            .position(|a| a == "--dashboard")
            .and_then(|pos| args.get(pos + 1))
            .and_then(|pid_str| pid_str.parse::<u32>().ok())
            .or_else(find_running_wrapper);

        match wrapper_pid {
            Some(pid) => {
                eprintln!("Connecting to wrapper PID: {}", pid);
                return tui::run_dashboard(pid);
            }
            None => {
                eprintln!("Error: No running lazarus-mcp wrapper found.");
                eprintln!("Start a wrapper first with: lazarus-mcp <command>");
                eprintln!("Or specify a PID: lazarus-mcp --dashboard <pid>");
                std::process::exit(1);
            }
        }
    }

    // Wrapper mode - parse options and command
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(Level::WARN.into())
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    // Parse arguments: options come before the command, command is first non-option arg
    // Optional -- separator is supported for compatibility
    let separator_pos = args.iter().position(|a| a == "--");

    let (aegis_args, command_args) = if let Some(pos) = separator_pos {
        // Explicit -- separator: everything before is options, everything after is command
        let aegis: Vec<String> = args[1..pos].to_vec();
        let cmd: Vec<String> = args[pos + 1..].to_vec();
        (aegis, cmd)
    } else {
        // No separator: find first non-option argument as the command
        let first_cmd_pos = args[1..].iter().position(|a| !a.starts_with("--"));

        match first_cmd_pos {
            Some(pos) => {
                let actual_pos = pos + 1; // Adjust for skipping args[0]
                let aegis: Vec<String> = args[1..actual_pos].to_vec();
                let cmd: Vec<String> = args[actual_pos..].to_vec();
                (aegis, cmd)
            }
            None => {
                // No command found - show usage
                print_usage();
                eprintln!("\nError: No command specified. Use: lazarus-mcp <command>");
                std::process::exit(1);
            }
        }
    };

    // Must have a command
    if command_args.is_empty() {
        print_usage();
        eprintln!("\nError: No command specified");
        std::process::exit(1);
    }

    // Parse lazarus-mcp options
    let inject_mcp = !aegis_args.iter().any(|a| a == "--no-inject-mcp");

    // The command is the first element, rest are its arguments
    let command = PathBuf::from(&command_args[0]);
    let cmd_args: Vec<String> = command_args[1..].to_vec();

    wrapper::run_command(command, cmd_args, inject_mcp)
}

/// Find a running lazarus-mcp wrapper by scanning /tmp for state files
fn find_running_wrapper() -> Option<u32> {
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(pid_str) = filename.strip_prefix("lazarus-mcp-state-") {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        if std::fs::metadata(format!("/proc/{}", pid)).is_ok() {
                            return Some(pid);
                        }
                    }
                }
            }
        }
    }
    None
}
