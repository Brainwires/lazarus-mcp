mod mcp_server;
mod restart;
mod wrapper;

use anyhow::Result;
use std::env;
use tracing::Level;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
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

        mcp_server::run().await
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

        // Pass all args (except program name) to claude
        let claude_args: Vec<String> = args.into_iter().skip(1).collect();
        wrapper::run(claude_args).await
    }
}
