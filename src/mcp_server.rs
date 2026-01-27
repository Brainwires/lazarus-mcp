use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use tracing::{debug, error, info};

use crate::restart;

/// MCP Server implementation
pub fn run() -> Result<()> {
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

        let response = handle_request(&request);

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

fn handle_request(request: &Value) -> Option<Value> {
    let method = request.get("method")?.as_str()?;
    let id = request.get("id").cloned();

    let result = match method {
        "initialize" => handle_initialize(),
        "initialized" => return None, // Notification, no response
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(request.get("params")),
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
                "description": "Restart Claude Code to reconnect all MCP servers. Use this after making changes to an MCP server's code. Requires Claude to be started via the rusty-restart-claude wrapper. The session will automatically continue with --continue. Optionally include a prompt to auto-send after restart.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reason": {
                            "type": "string",
                            "description": "Optional reason for the restart (for logging)"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Optional prompt to automatically send after restart (e.g., 'Continue where we left off - MCP servers reloaded')"
                        }
                    }
                }
            },
            {
                "name": "server_status",
                "description": "Get status information about the wrapper, Claude Code process, and whether restart is supported.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

fn handle_tools_call(params: Option<&Value>) -> Value {
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
        "restart_claude" => handle_restart_claude(arguments),
        "server_status" => handle_server_status(),
        _ => json!({
            "content": [{
                "type": "text",
                "text": format!("Unknown tool: {}", tool_name)
            }],
            "isError": true
        }),
    }
}

fn handle_restart_claude(arguments: Option<&Value>) -> Value {
    let reason = arguments
        .and_then(|a| a.get("reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("MCP server restart requested")
        .to_string();

    let prompt = arguments
        .and_then(|a| a.get("prompt"))
        .and_then(|p| p.as_str());

    info!(reason = %reason, prompt = ?prompt, "Triggering Claude Code restart via signal file");

    match restart::send_restart_signal(&reason, prompt) {
        Ok(info) => {
            let prompt_msg = if prompt.is_some() {
                "\nA prompt will be auto-sent after restart."
            } else {
                ""
            };
            json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Restart signal sent!\n\nWrapper PID: {}\nReason: {}{}\\n\nClaude will restart momentarily and resume with --continue.",
                        info.wrapper_pid,
                        reason,
                        prompt_msg
                    )
                }],
                "isError": false
            })
        },
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Failed to trigger restart: {}\n\nMake sure you started Claude via the rusty-restart-claude wrapper:\n  rusty-restart-claude [claude-args...]", e)
            }],
            "isError": true
        }),
    }
}

fn handle_server_status() -> Value {
    let status = restart::get_status();

    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&status).unwrap_or_else(|_| format!("{:?}", status))
        }],
        "isError": false
    })
}
