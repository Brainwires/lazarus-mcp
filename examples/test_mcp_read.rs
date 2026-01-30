//! Test executable for MCP injection
//!
//! This simulates what a coding agent does when reading .mcp.json
//! Run with: cargo run --example test_mcp_read

use std::fs;
use std::path::Path;

fn main() {
    println!("=== MCP Config Reader Test ===\n");

    // Check environment
    println!("Environment:");
    println!("  LD_PRELOAD: {:?}", std::env::var("LD_PRELOAD").ok());
    println!("  AEGIS_MCP_OVERLAY: {:?}", std::env::var("AEGIS_MCP_OVERLAY").ok());
    println!("  AEGIS_MCP_TARGET: {:?}", std::env::var("AEGIS_MCP_TARGET").ok());
    println!();

    // Try to read .mcp.json from current directory
    let mcp_path = Path::new(".mcp.json");
    println!("Reading: {}", mcp_path.display());

    match fs::read_to_string(mcp_path) {
        Ok(content) => {
            println!("Content ({} bytes):", content.len());
            println!("{}", content);

            // Parse and list servers
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(servers) = json.get("mcpServers").and_then(|s| s.as_object()) {
                    println!("\nMCP Servers found:");
                    for (name, config) in servers {
                        let cmd = config.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                        println!("  - {} (command: {})", name, cmd);
                    }
                }
            }
        }
        Err(e) => {
            println!("Error reading .mcp.json: {}", e);
        }
    }

    println!("\n=== Done ===");
}
