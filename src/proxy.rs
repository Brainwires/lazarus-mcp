use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, warn};

use crate::process::ProcessManager;
use crate::tools::{handle_injected_tool, get_injected_tools, RESTART_SERVER_TOOL, SERVER_STATUS_TOOL};

/// MCP Proxy that sits between Claude Code and the wrapped server
pub struct McpProxy {
    process_manager: Arc<ProcessManager>,
    /// Cached initialize request for replay after restart
    cached_initialize: Arc<Mutex<Option<String>>>,
    /// Channel to receive stdout from child
    child_stdout_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    /// Channel to send stdin to child
    child_stdin_tx: mpsc::Sender<String>,
}

impl McpProxy {
    pub fn new(
        process_manager: Arc<ProcessManager>,
        child_stdout_rx: mpsc::Receiver<String>,
        child_stdin_tx: mpsc::Sender<String>,
    ) -> Self {
        Self {
            process_manager,
            cached_initialize: Arc::new(Mutex::new(None)),
            child_stdout_rx: Arc::new(Mutex::new(child_stdout_rx)),
            child_stdin_tx,
        }
    }

    /// Run the proxy - reads from our stdin, forwards to child, reads child stdout, writes to our stdout
    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_reader = BufReader::new(stdin).lines();

        // Spawn a task to forward child stdout to our stdout (with tool injection)
        let child_stdout_rx = Arc::clone(&self.child_stdout_rx);
        let stdout_handle = tokio::spawn(async move {
            let mut rx = child_stdout_rx.lock().await;
            let mut stdout = tokio::io::stdout();
            while let Some(line) = rx.recv().await {
                // Try to parse and potentially modify the response
                let output_line = match serde_json::from_str::<Value>(&line) {
                    Ok(mut msg) => {
                        // Check if this is a tools/list response and inject our tools
                        if let Some(result) = msg.get_mut("result") {
                            if let Some(tools) = result.get_mut("tools") {
                                if let Some(tools_array) = tools.as_array_mut() {
                                    // Inject our tools
                                    for tool in get_injected_tools() {
                                        tools_array.push(tool);
                                    }
                                    debug!("Injected tools into tools/list response");
                                }
                            }
                        }
                        serde_json::to_string(&msg).unwrap_or(line)
                    }
                    Err(_) => line,
                };

                if let Err(e) = stdout.write_all(output_line.as_bytes()).await {
                    error!(error = %e, "Failed to write to stdout");
                    break;
                }
                if let Err(e) = stdout.write_all(b"\n").await {
                    error!(error = %e, "Failed to write newline to stdout");
                    break;
                }
                if let Err(e) = stdout.flush().await {
                    error!(error = %e, "Failed to flush stdout");
                    break;
                }
            }
        });

        // Main loop: read from our stdin, process, and forward to child
        while let Ok(Some(line)) = stdin_reader.next_line().await {
            debug!("Received from Claude Code: {}", line);

            // Parse the JSON-RPC message
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "Failed to parse JSON-RPC message");
                    continue;
                }
            };

            // Check if this is an initialize request - cache it for replay
            if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                if method == "initialize" {
                    debug!("Caching initialize request for replay");
                    *self.cached_initialize.lock().await = Some(line.clone());
                }

                // Check if this is a tools/call for one of our injected tools
                if method == "tools/call" {
                    if let Some(params) = msg.get("params") {
                        if let Some(tool_name) = params.get("name").and_then(|n| n.as_str()) {
                            if tool_name == RESTART_SERVER_TOOL || tool_name == SERVER_STATUS_TOOL {
                                // Handle our injected tool
                                let response = handle_injected_tool(
                                    tool_name,
                                    params.get("arguments"),
                                    &self.process_manager,
                                    self.cached_initialize.clone(),
                                    &self.child_stdin_tx,
                                ).await;

                                // Build JSON-RPC response
                                let rpc_response = json!({
                                    "jsonrpc": "2.0",
                                    "id": msg.get("id"),
                                    "result": response
                                });

                                let response_str = serde_json::to_string(&rpc_response)?;
                                debug!("Sending injected tool response: {}", response_str);

                                stdout.write_all(response_str.as_bytes()).await?;
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                                continue; // Don't forward to child
                            }
                        }
                    }
                }
            }

            // Forward to child
            if let Err(e) = self.child_stdin_tx.send(line).await {
                error!(error = %e, "Failed to send to child stdin");
                break;
            }
        }

        stdout_handle.abort();
        Ok(())
    }
}
