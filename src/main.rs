mod process;
mod proxy;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, Level};
use tracing_subscriber::EnvFilter;

use process::ProcessManager;
use proxy::McpProxy;

#[derive(Parser)]
#[command(name = "rusty-restart-claude")]
#[command(author = "Brainwires")]
#[command(version)]
#[command(about = "MCP proxy for hot-reloading MCP servers during development")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Wrap an MCP server with hot-reload capability
    Wrap {
        /// Name of the server (for logging and tool descriptions)
        #[arg(short, long)]
        name: String,

        /// The command and arguments to run the wrapped MCP server
        /// Use -- to separate from rusty-restart-claude arguments
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

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

    let cli = Cli::parse();

    match cli.command {
        Commands::Wrap { name, command } => {
            run_wrap(name, command).await?;
        }
    }

    Ok(())
}

async fn run_wrap(name: String, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        anyhow::bail!("No command specified. Use -- followed by the command to run.");
    }

    let (cmd, args) = command.split_first().unwrap();
    let args: Vec<String> = args.to_vec();

    info!(
        name = %name,
        command = %cmd,
        args = ?args,
        "Starting rusty-restart-claude proxy"
    );

    // Create channels for communication
    let (child_stdout_tx, child_stdout_rx) = mpsc::channel::<String>(100);

    // Create the process manager
    let (process_manager, child_stdin_tx) = ProcessManager::new(
        name.clone(),
        cmd.clone(),
        args,
        child_stdout_tx,
    );

    let process_manager = Arc::new(process_manager);

    // Spawn the wrapped server
    process_manager.spawn().await
        .context("Failed to spawn wrapped MCP server")?;

    // Create and run the proxy
    let proxy = McpProxy::new(
        Arc::clone(&process_manager),
        child_stdout_rx,
        child_stdin_tx,
    );

    // Run until stdin closes or error
    if let Err(e) = proxy.run().await {
        error!(error = %e, "Proxy error");
    }

    // Clean up
    info!("Shutting down");
    process_manager.kill().await?;

    Ok(())
}
