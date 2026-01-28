mod mcp_server;
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
    eprintln!("  --keep-root    Stay root instead of dropping privileges (when run with sudo)\n");
    eprintln!("EXAMPLES:");
    eprintln!("  aegis-mcp claude --continue");
    eprintln!("  aegis-mcp claude -p \"Help me with...\"");
    eprintln!("  aegis-mcp aider --model gpt-4");
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

        // Filter out aegis-mcp options, pass remaining to agent
        let agent_args: Vec<String> = remaining_args
            .into_iter()
            .filter(|a| a != "--keep-root")
            .collect();

        wrapper::run(agent, agent_args, keep_root)
    }
}
