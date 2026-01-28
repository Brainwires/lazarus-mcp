//! MCP Server Implementation
//!
//! Implements the MCP (Model Context Protocol) server that exposes aegis-mcp's capabilities.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::netmon;
use crate::pool::{AgentPool, AgentStatus, Task, TaskPriority};
use crate::restart;

/// Lazy-initialized agent pool
static POOL: std::sync::OnceLock<Arc<RwLock<AgentPool>>> = std::sync::OnceLock::new();

/// Get or create the agent pool
fn get_pool() -> Arc<RwLock<AgentPool>> {
    POOL.get_or_init(|| {
        info!("Initializing agent pool");
        Arc::new(RwLock::new(AgentPool::new(5)))
    })
    .clone()
}

/// MCP Server implementation
pub fn run() -> Result<()> {
    info!("Starting aegis-mcp MCP server");

    // Create tokio runtime for async operations
    let rt = Runtime::new()?;

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

        let response = rt.block_on(handle_request(&request));

        if let Some(resp) = response {
            let resp_str = serde_json::to_string(&resp)?;
            debug!("Sending: {}", resp_str);
            writeln!(stdout, "{}", resp_str)?;
            stdout.flush()?;
        }
    }

    // Cleanup
    info!("MCP server shutting down");
    rt.block_on(async {
        let pool = get_pool();
        pool.read().await.shutdown().await;
    });

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
            "name": "aegis-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            // Existing restart tools
            {
                "name": "restart_claude",
                "description": "Restart the AI coding agent to reconnect all MCP servers. Use this after making changes to an MCP server's code. Requires the agent to be started via the aegis-mcp wrapper (e.g., 'aegis-mcp claude'). The session will automatically continue if the agent supports it. Optionally include a prompt to auto-send after restart.",
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
            },
            // Agent pool tools
            {
                "name": "agent_spawn",
                "description": "Spawn a background agent to work on a task autonomously. The agent will execute the task in the background and report results. Returns the agent ID immediately.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "Description of the task for the agent to execute"
                        },
                        "agent_type": {
                            "type": "string",
                            "enum": ["claude", "aider", "cursor"],
                            "description": "Type of agent to spawn (default: claude)"
                        },
                        "working_directory": {
                            "type": "string",
                            "description": "Working directory for the agent"
                        },
                        "max_iterations": {
                            "type": "integer",
                            "description": "Maximum iterations before the agent gives up (default: 50)"
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["low", "normal", "high", "urgent"],
                            "description": "Task priority (default: normal)"
                        }
                    },
                    "required": ["description"]
                }
            },
            {
                "name": "agent_list",
                "description": "List all active background agents with their current status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "agent_status",
                "description": "Get detailed status of a specific background agent.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to check"
                        }
                    },
                    "required": ["agent_id"]
                }
            },
            {
                "name": "agent_await",
                "description": "Wait for a background agent to complete and get its result. Blocks until the agent finishes (completes or fails).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to wait for"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Optional timeout in seconds"
                        }
                    },
                    "required": ["agent_id"]
                }
            },
            {
                "name": "agent_stop",
                "description": "Stop a running background agent.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to stop"
                        }
                    },
                    "required": ["agent_id"]
                }
            },
            {
                "name": "agent_pool_stats",
                "description": "Get statistics about the agent pool (active, running, completed agents).",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "agent_file_locks",
                "description": "List all currently held file locks by agents.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            // Network monitoring tools
            {
                "name": "netmon_status",
                "description": "Get network monitoring status and statistics. Shows connection counts, bytes transferred, and top targets. Requires aegis-mcp to be started with --netmon flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "netmon_log",
                "description": "Get recent network events from the monitoring log. Requires aegis-mcp to be started with --netmon flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "count": {
                            "type": "integer",
                            "description": "Number of recent events to return (default: 20)"
                        }
                    }
                }
            },
            {
                "name": "netmon_namespace_list",
                "description": "List all aegis network namespaces. Network namespaces are used when --netmon=netns is specified (requires root).",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "netmon_namespace_cleanup",
                "description": "Clean up stale aegis network namespaces. Useful for recovery after crashes. Requires root privileges.",
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
        // Existing tools
        "restart_claude" => handle_restart_claude(arguments),
        "server_status" => handle_server_status(),
        // Agent pool tools
        "agent_spawn" => handle_agent_spawn(arguments).await,
        "agent_list" => handle_agent_list().await,
        "agent_status" => handle_agent_status(arguments).await,
        "agent_await" => handle_agent_await(arguments).await,
        "agent_stop" => handle_agent_stop(arguments).await,
        "agent_pool_stats" => handle_agent_pool_stats().await,
        "agent_file_locks" => handle_agent_file_locks().await,
        // Network monitoring tools
        "netmon_status" => handle_netmon_status(),
        "netmon_log" => handle_netmon_log(arguments),
        "netmon_namespace_list" => handle_netmon_namespace_list(),
        "netmon_namespace_cleanup" => handle_netmon_namespace_cleanup(),
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
                        "Restart signal sent!\n\nWrapper PID: {}\nReason: {}{}\n\nClaude will restart momentarily and resume with --continue.",
                        info.wrapper_pid,
                        reason,
                        prompt_msg
                    )
                }],
                "isError": false
            })
        }
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Failed to trigger restart: {}\n\nMake sure you started your agent via the aegis-mcp wrapper:\n  aegis-mcp <agent> [args...]\n\nExample: aegis-mcp claude --continue", e)
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

// Agent pool tool handlers

async fn handle_agent_spawn(arguments: Option<&Value>) -> Value {
    let description = match arguments.and_then(|a| a.get("description")).and_then(|d| d.as_str()) {
        Some(d) => d.to_string(),
        None => {
            return json!({
                "content": [{
                    "type": "text",
                    "text": "Missing required parameter: description"
                }],
                "isError": true
            });
        }
    };

    let agent_type = arguments
        .and_then(|a| a.get("agent_type"))
        .and_then(|t| t.as_str())
        .unwrap_or("claude")
        .to_string();

    let working_directory = arguments
        .and_then(|a| a.get("working_directory"))
        .and_then(|d| d.as_str())
        .map(std::path::PathBuf::from);

    let max_iterations = arguments
        .and_then(|a| a.get("max_iterations"))
        .and_then(|m| m.as_u64())
        .unwrap_or(50) as u32;

    let priority = match arguments
        .and_then(|a| a.get("priority"))
        .and_then(|p| p.as_str())
        .unwrap_or("normal")
    {
        "low" => TaskPriority::Low,
        "high" => TaskPriority::High,
        "urgent" => TaskPriority::Urgent,
        _ => TaskPriority::Normal,
    };

    let mut task = Task::new(&description)
        .with_agent_type(&agent_type)
        .with_max_iterations(max_iterations)
        .with_priority(priority);

    if let Some(dir) = working_directory {
        task = task.with_working_directory(dir);
    }

    let pool = get_pool();
    let pool = pool.read().await;

    match pool.spawn(task).await {
        Ok(agent_id) => json!({
            "content": [{
                "type": "text",
                "text": format!(
                    "Spawned background agent: {}\n\nTask: {}\nAgent type: {}\nMax iterations: {}",
                    agent_id, description, agent_type, max_iterations
                )
            }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Failed to spawn agent: {}", e)
            }],
            "isError": true
        }),
    }
}

async fn handle_agent_list() -> Value {
    let pool = get_pool();
    let pool = pool.read().await;
    let agents = pool.list().await;

    if agents.is_empty() {
        return json!({
            "content": [{
                "type": "text",
                "text": "No active background agents"
            }],
            "isError": false
        });
    }

    let mut output = format!("{} active agent(s):\n\n", agents.len());
    for (id, status) in agents {
        let icon = match &status {
            AgentStatus::Starting => "🔄",
            AgentStatus::Running { .. } => "▶️",
            AgentStatus::Completed { .. } => "✅",
            AgentStatus::Failed { .. } => "❌",
            AgentStatus::Stopped => "⏹️",
        };
        output.push_str(&format!("{} {} - {}\n", icon, id, status));
    }

    json!({
        "content": [{
            "type": "text",
            "text": output
        }],
        "isError": false
    })
}

async fn handle_agent_status(arguments: Option<&Value>) -> Value {
    let agent_id = match arguments.and_then(|a| a.get("agent_id")).and_then(|i| i.as_str()) {
        Some(id) => id,
        None => {
            return json!({
                "content": [{
                    "type": "text",
                    "text": "Missing required parameter: agent_id"
                }],
                "isError": true
            });
        }
    };

    let pool = get_pool();
    let pool = pool.read().await;

    match pool.status(agent_id).await {
        Some(status) => json!({
            "content": [{
                "type": "text",
                "text": format!("Agent {}: {}", agent_id, status)
            }],
            "isError": false
        }),
        None => json!({
            "content": [{
                "type": "text",
                "text": format!("Agent {} not found", agent_id)
            }],
            "isError": true
        }),
    }
}

async fn handle_agent_await(arguments: Option<&Value>) -> Value {
    let agent_id = match arguments.and_then(|a| a.get("agent_id")).and_then(|i| i.as_str()) {
        Some(id) => id.to_string(),
        None => {
            return json!({
                "content": [{
                    "type": "text",
                    "text": "Missing required parameter: agent_id"
                }],
                "isError": true
            });
        }
    };

    let timeout_secs = arguments
        .and_then(|a| a.get("timeout_secs"))
        .and_then(|t| t.as_u64())
        .map(|t| std::time::Duration::from_secs(t));

    let pool = get_pool();
    let pool = pool.read().await;

    let result = if let Some(timeout) = timeout_secs {
        pool.await_completion_timeout(&agent_id, timeout).await
    } else {
        pool.await_completion(&agent_id).await
    };

    match result {
        Ok(task_result) => {
            let status = if task_result.success { "succeeded" } else { "failed" };
            let error_msg = task_result.error.map(|e| format!("\nError: {}", e)).unwrap_or_default();
            json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Agent {} {} after {} iterations.\n\nSummary: {}{}",
                        agent_id, status, task_result.iterations, task_result.summary, error_msg
                    )
                }],
                "isError": !task_result.success
            })
        }
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Error waiting for agent: {}", e)
            }],
            "isError": true
        }),
    }
}

async fn handle_agent_stop(arguments: Option<&Value>) -> Value {
    let agent_id = match arguments.and_then(|a| a.get("agent_id")).and_then(|i| i.as_str()) {
        Some(id) => id,
        None => {
            return json!({
                "content": [{
                    "type": "text",
                    "text": "Missing required parameter: agent_id"
                }],
                "isError": true
            });
        }
    };

    let pool = get_pool();
    let pool = pool.read().await;

    match pool.stop(agent_id).await {
        Ok(()) => json!({
            "content": [{
                "type": "text",
                "text": format!("Stopped agent {}", agent_id)
            }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Failed to stop agent: {}", e)
            }],
            "isError": true
        }),
    }
}

async fn handle_agent_pool_stats() -> Value {
    let pool = get_pool();
    let pool = pool.read().await;
    let stats = pool.stats().await;

    json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Agent Pool Statistics:\n\
                 Max agents: {}\n\
                 Total agents: {}\n\
                 Running: {}\n\
                 Completed: {}\n\
                 Failed: {}",
                stats.max_agents,
                stats.total_agents,
                stats.running,
                stats.completed,
                stats.failed
            )
        }],
        "isError": false
    })
}

async fn handle_agent_file_locks() -> Value {
    let pool = get_pool();
    let pool = pool.read().await;
    let lock_manager = pool.lock_manager();
    let locks = lock_manager.list_locks().await;

    if locks.is_empty() {
        return json!({
            "content": [{
                "type": "text",
                "text": "No file locks currently held"
            }],
            "isError": false
        });
    }

    let mut output = format!("{} file lock(s):\n\n", locks.len());
    for (path, info) in locks {
        let lock_type = match info.lock_type {
            crate::pool::LockType::Read => "read",
            crate::pool::LockType::Write => "write",
        };
        output.push_str(&format!(
            "- {} ({}) by {}\n",
            path.display(),
            lock_type,
            info.agent_id
        ));
    }

    json!({
        "content": [{
            "type": "text",
            "text": output
        }],
        "isError": false
    })
}

// Network monitoring tool handlers

fn handle_netmon_status() -> Value {
    // Look for the netmon log file in the standard location
    let log_path = std::path::PathBuf::from(format!(
        "/tmp/aegis-netmon-{}.jsonl",
        find_wrapper_pid().unwrap_or(std::process::id())
    ));

    if !log_path.exists() {
        return json!({
            "content": [{
                "type": "text",
                "text": "Network monitoring not active.\n\nTo enable, start aegis-mcp with the --netmon flag:\n  aegis-mcp claude --netmon"
            }],
            "isError": false
        });
    }

    match netmon::format_summary(&log_path) {
        Ok(summary) => json!({
            "content": [{
                "type": "text",
                "text": summary
            }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Error reading netmon log: {}", e)
            }],
            "isError": true
        }),
    }
}

fn handle_netmon_log(arguments: Option<&Value>) -> Value {
    let count = arguments
        .and_then(|a| a.get("count"))
        .and_then(|c| c.as_u64())
        .unwrap_or(20) as usize;

    // Look for the netmon log file in the standard location
    let log_path = std::path::PathBuf::from(format!(
        "/tmp/aegis-netmon-{}.jsonl",
        find_wrapper_pid().unwrap_or(std::process::id())
    ));

    if !log_path.exists() {
        return json!({
            "content": [{
                "type": "text",
                "text": "Network monitoring not active.\n\nTo enable, start aegis-mcp with the --netmon flag:\n  aegis-mcp claude --netmon"
            }],
            "isError": false
        });
    }

    match netmon::recent_events(&log_path, count) {
        Ok(events) => {
            if events.is_empty() {
                return json!({
                    "content": [{
                        "type": "text",
                        "text": "No network events recorded yet."
                    }],
                    "isError": false
                });
            }

            let mut output = format!("Recent {} network events:\n\n", events.len());
            for event in events {
                output.push_str(&format!("{}\n", serde_json::to_string(&event).unwrap_or_default()));
            }

            json!({
                "content": [{
                    "type": "text",
                    "text": output
                }],
                "isError": false
            })
        }
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Error reading netmon log: {}", e)
            }],
            "isError": true
        }),
    }
}

fn handle_netmon_namespace_list() -> Value {
    match netmon::netns::list_namespaces() {
        Ok(namespaces) => {
            if namespaces.is_empty() {
                json!({
                    "content": [{
                        "type": "text",
                        "text": "No aegis network namespaces found.\n\nNetwork namespaces are created when using --netmon=netns mode (requires root)."
                    }],
                    "isError": false
                })
            } else {
                let mut output = format!("{} aegis network namespace(s):\n\n", namespaces.len());
                for ns in namespaces {
                    output.push_str(&format!("- {}\n", ns));
                }
                json!({
                    "content": [{
                        "type": "text",
                        "text": output
                    }],
                    "isError": false
                })
            }
        }
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Error listing namespaces: {}", e)
            }],
            "isError": true
        }),
    }
}

fn handle_netmon_namespace_cleanup() -> Value {
    match netmon::netns::cleanup_all() {
        Ok(count) => {
            if count == 0 {
                json!({
                    "content": [{
                        "type": "text",
                        "text": "No stale network namespaces to clean up."
                    }],
                    "isError": false
                })
            } else {
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Cleaned up {} stale network namespace(s).", count)
                    }],
                    "isError": false
                })
            }
        }
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": format!("Error cleaning up namespaces: {}. Make sure you have root privileges.", e)
            }],
            "isError": true
        }),
    }
}

/// Find the wrapper PID by walking up the process tree
fn find_wrapper_pid() -> Option<u32> {
    // Try to find the wrapper by checking parent processes
    let mut current_pid = std::process::id();

    for _ in 0..5 {
        // Get parent PID
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", current_pid)).ok()?;
        let close_paren = stat.rfind(')')?;
        let after_comm = &stat[close_paren + 2..];
        let parts: Vec<&str> = after_comm.split_whitespace().collect();
        let parent_pid: u32 = parts.get(1)?.parse().ok()?;

        if parent_pid <= 1 {
            break;
        }

        // Check if parent is aegis-mcp
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", parent_pid))
            .ok()
            .map(|s| s.trim().to_string())?;

        if comm.contains("aegis-mcp") {
            return Some(parent_pid);
        }

        current_pid = parent_pid;
    }

    None
}
