mod mcp_server;
mod netmon;
mod pool;
mod privileges;
mod restart;
mod wrapper;

use anyhow::Result;
use std::env;
use tracing::Level;
use tracing_subscriber::EnvFilter;

fn print_usage() {
    eprintln!("aegis-mcp - Agent supervisor with hot-reload support for MCP servers\n");
    eprintln!("USAGE:");
    eprintln!("  aegis-mcp <agent> [options] [agent-args...]   Run as wrapper for the specified agent");
    eprintln!("  aegis-mcp --mcp-server                        Run as MCP server (used by agents)\n");
    eprintln!("SUPPORTED AGENTS:");
    eprintln!("  claude    Claude Code CLI");
    eprintln!("  cursor    Cursor editor");
    eprintln!("  aider     Aider CLI\n");
    eprintln!("OPTIONS:");
    eprintln!("  --keep-root          Stay root instead of dropping privileges (when run with sudo)");
    eprintln!("  --no-inject-mcp      Don't auto-inject aegis-mcp as an MCP server");
    eprintln!("  --netmon             Enable network monitoring (auto-detect mode)");
    eprintln!("  --netmon=preload     Force LD_PRELOAD mode for network monitoring");
    eprintln!("  --netmon=netns       Force network namespace mode (requires root)\n");
    eprintln!("EXAMPLES:");
    eprintln!("  aegis-mcp claude --continue");
    eprintln!("  aegis-mcp claude -p \"Help me with...\"");
    eprintln!("  aegis-mcp aider --model gpt-4");
    eprintln!("  aegis-mcp claude --netmon          # Monitor network with LD_PRELOAD");
    eprintln!("  sudo aegis-mcp claude              # Drops to original user before spawning");
    eprintln!("  sudo aegis-mcp claude --keep-root  # Stays root (advanced/debugging)");
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Check if running as MCP server
    let is_mcp_server = args.iter().any(|arg| arg == "--mcp-server");

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

        // Filter out aegis-mcp options, pass remaining to agent
        let agent_args: Vec<String> = remaining_args
            .into_iter()
            .filter(|a| a != "--keep-root" && a != "--no-inject-mcp" && !a.starts_with("--netmon"))
            .collect();

        wrapper::run(agent, agent_args, keep_root, netmon_mode, inject_mcp)
    }
}
