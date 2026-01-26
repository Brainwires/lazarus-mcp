use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

use crate::process::ProcessManager;

/// Tool name constants
pub const RESTART_SERVER_TOOL: &str = "restart_server";
pub const SERVER_STATUS_TOOL: &str = "server_status";

/// Get the tool definitions to inject into tools/list responses
pub fn get_injected_tools() -> Vec<Value> {
    vec![
        json!({
            "name": RESTART_SERVER_TOOL,
            "description": "Restart the wrapped MCP server to pick up code changes. Use after editing the server's source code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Optional reason for restart (for logging)"
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": SERVER_STATUS_TOOL,
            "description": "Check the status of the wrapped MCP server (running, uptime, restart count).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
    ]
}

/// Handle a call to one of our injected tools
pub async fn handle_injected_tool(
    tool_name: &str,
    arguments: Option<&Value>,
    process_manager: &Arc<ProcessManager>,
    cached_initialize: Arc<Mutex<Option<String>>>,
    child_stdin_tx: &mpsc::Sender<String>,
) -> Value {
    match tool_name {
        RESTART_SERVER_TOOL => {
            handle_restart_server(arguments, process_manager, cached_initialize, child_stdin_tx).await
        }
        SERVER_STATUS_TOOL => {
            handle_server_status(process_manager).await
        }
        _ => {
            json!({
                "content": [{
                    "type": "text",
                    "text": format!("Unknown tool: {}", tool_name)
                }],
                "isError": true
            })
        }
    }
}

/// Handle the restart_server tool
async fn handle_restart_server(
    arguments: Option<&Value>,
    process_manager: &Arc<ProcessManager>,
    cached_initialize: Arc<Mutex<Option<String>>>,
    child_stdin_tx: &mpsc::Sender<String>,
) -> Value {
    let reason = arguments
        .and_then(|args| args.get("reason"))
        .and_then(|r| r.as_str());

    info!(reason = reason, "Handling restart_server tool call");

    // Perform the restart
    match process_manager.restart(reason).await {
        Ok(()) => {
            // Replay the initialize request
            if let Some(init_request) = cached_initialize.lock().await.clone() {
                info!("Replaying initialize request after restart");
                if let Err(e) = child_stdin_tx.send(init_request).await {
                    error!(error = %e, "Failed to replay initialize request");
                    return json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Server restarted but failed to replay initialize: {}", e)
                        }],
                        "isError": true
                    });
                }

                // Give the server a moment to process initialize
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            let status = process_manager.status().await;
            json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Server '{}' restarted successfully.\nPID: {}\nRestart count: {}",
                        status.server_name,
                        status.pid.map(|p| p.to_string()).unwrap_or_else(|| "unknown".to_string()),
                        status.restart_count
                    )
                }],
                "isError": false
            })
        }
        Err(e) => {
            error!(error = %e, "Failed to restart server");
            json!({
                "content": [{
                    "type": "text",
                    "text": format!("Failed to restart server: {}", e)
                }],
                "isError": true
            })
        }
    }
}

/// Handle the server_status tool
async fn handle_server_status(process_manager: &Arc<ProcessManager>) -> Value {
    let status = process_manager.status().await;

    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&status).unwrap_or_else(|_| format!("{:?}", status))
        }],
        "isError": false
    })
}
