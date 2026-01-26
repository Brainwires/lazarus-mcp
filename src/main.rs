mod mcp_server;
mod restart;

use anyhow::Result;
use tracing::Level;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to stderr (stdout is for MCP protocol)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(Level::INFO.into())
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    // Run as MCP server (no subcommands needed - it's always an MCP server)
    mcp_server::run().await
}
