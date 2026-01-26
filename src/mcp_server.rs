use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use tracing::{debug, error, info};

use crate::restart;

/// MCP Server implementation
pub async fn run() -> Result<()> {
    info!("Starting rusty-restart-claude MCP server");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "Failed to read stdin");
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        debug!("Received: {}", line);

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "Failed to parse JSON-RPC");
                continue;
            }
        };

        let response = handle_request(&request).await;

        if let Some(resp) = response {
            let resp_str = serde_json::to_string(&resp)?;
            debug!("Sending: {}", resp_str);
            writeln!(stdout, "{}", resp_str)?;
            stdout.flush()?;
        }
    }

    info!("MCP server shutting down");
    Ok(())
}

async fn handle_request(request: &Value) -> Option<Value> {
    let method = request.get("method")?.as_str()?;
    let id = request.get("id").cloned();

    let result = match method {
        "initialize" => handle_initialize(),
        "initialized" => return None, // Notification, no response
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(request.get("params")).await,
        "ping" => json!({}),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {}", method)
                }
            }));
        }
    };

    Some(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "rusty-restart-claude",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "restart_claude",
                "description": "Restart Claude Code to reconnect all MCP servers. Use this after making changes to an MCP server's code. A detached process will restart Claude Code, preserving the working directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "delay_ms": {
                            "type": "integer",
                            "description": "Delay in milliseconds before restarting (default: 500)",
                            "default": 500
                        }
                    }
                }
            },
            {
                "name": "server_status",
                "description": "Get status information about this MCP server and Claude Code process.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

async fn handle_tools_call(params: Option<&Value>) -> Value {
    let params = match params {
        Some(p) => p,
        None => {
            return json!({
                "content": [{
                    "type": "text",
                    "text": "Missing params"
                }],
                "isError": true
            });
        }
    };

    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = params.get("arguments");

    match tool_name {
        "restart_claude" => handle_restart_claude(arguments).await,
        "server_status" => handle_server_status().await,
        _ => json!({
            "content": [{
                "type": "text",
                "text": format!("Unknown tool: {}", tool_name)
            }],
            "isError": true
        }),
    }
}

async fn handle_restart_claude(arguments: Option<&Value>) -> Value {
    let delay_ms = arguments
        .and_then(|a| a.get("delay_ms"))
        .and_then(|d| d.as_u64())
        .unwrap_or(500) as u32;

    info!(delay_ms = delay_ms, "Triggering Claude Code restart");

    match restart::trigger_restart(delay_ms) {
        Ok(info) => json!({
            "content": [{
                "type": "text",
                "text": format!(
                    "Restart initiated!\n\nClaude Code (PID {}) will restart in {}ms.\nWorking directory: {}\n\nThis session will end. A new Claude Code session will start automatically.",
                    info.claude_pid,
                    delay_ms,
                    info.working_dir
                )
            }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Failed to trigger restart: {}", e)
            }],
            "isError": true
        }),
    }
}

async fn handle_server_status() -> Value {
    let status = restart::get_status();

    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&status).unwrap_or_else(|_| format!("{:?}", status))
        }],
        "isError": false
    })
}
