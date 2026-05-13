//! `mcp_install` helper — looks up a well-known MCP server, checks for
//! node/npx/pip on the host, and prints install + run instructions. The
//! original mcp_connect / mcp_list_tools / mcp_call_tool stubs that used
//! to live here were superseded by the auto-booting McpHub in
//! src/mcp/mod.rs and removed; only this discovery helper survives.

use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::Command;

pub fn mcp_install_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "mcp_install".into(),
            description: "Installs a well-known MCP server from the ecosystem. Supported servers: github, filesystem, brave-search, postgres, sqlite, memory, puppeteer, sequential-thinking.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Server name: 'github', 'filesystem', 'brave-search', 'postgres', 'sqlite', 'memory', 'puppeteer', 'sequential-thinking'"
                    }
                },
                "required": ["server"]
            }),
        },
    }
}

pub async fn handle_mcp_install(args: serde_json::Value) -> Result<String, String> {
    let server = args["server"].as_str().ok_or("server required")?;

    let packages: std::collections::HashMap<&str, &str> = [
        ("github", "@anthropic/mcp-server-github"),
        ("filesystem", "@anthropic/mcp-server-filesystem"),
        ("brave-search", "@anthropic/mcp-server-brave-search"),
        ("postgres", "@anthropic/mcp-server-postgres"),
        ("sqlite", "@anthropic/mcp-server-sqlite"),
        ("memory", "@anthropic/mcp-server-memory"),
        ("puppeteer", "@anthropic/mcp-server-puppeteer"),
        ("sequential-thinking", "@anthropic/mcp-server-sequential-thinking"),
    ].iter().cloned().collect();

    let package = packages.get(server.to_lowercase().as_str())
        .ok_or_else(|| format!(
            "Unknown MCP server: '{}'. Available: {}",
            server,
            packages.keys().cloned().collect::<Vec<_>>().join(", ")
        ))?;

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("## Install MCP Server: {}", server));
    lines.push(format!("Package: {}", package));
    lines.push(String::new());

    let has_node = Command::new("node").arg("--version").output().is_ok();
    let has_npx = Command::new("npx").arg("--version").output().is_ok();
    let has_pip = Command::new("pip3").arg("--version").output().is_ok()
        || Command::new("pip").arg("--version").output().is_ok();

    if has_node && has_npx {
        lines.push(format!("Install with: npx -y {}", package));
        lines.push(format!("Run with: npx {}", package));
    } else if has_pip {
        lines.push(format!("Install with: pip install mcp-server-{}", server));
    } else {
        lines.push("Node.js/npm not detected. Install Node.js or Python to use MCP servers.".to_string());
    }

    lines.push(String::new());

    match server.to_lowercase().as_str() {
        "github" => {
            lines.push("Requires: GITHUB_PERSONAL_ACCESS_TOKEN env var".to_string());
            lines.push("Permissions: repo, read:org".to_string());
        }
        "filesystem" => {
            lines.push("Args: --directory <path> (required)".to_string());
            lines.push("Example: npx @anthropic/mcp-server-filesystem --directory /path/to/allowed".to_string());
        }
        "brave-search" => {
            lines.push("Requires: BRAVE_API_KEY env var".to_string());
            lines.push("Get key: https://brave.com/search/api/".to_string());
        }
        _ => {
            lines.push("Check server documentation for configuration details.".to_string());
        }
    }

    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_install_def_name() {
        assert_eq!(mcp_install_def().function.name, "mcp_install");
    }

    #[tokio::test]
    async fn mcp_install_unknown_server_errors() {
        let result = handle_mcp_install(serde_json::json!({"server": "nonexistent"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mcp_install_github_mentions_token() {
        let result = handle_mcp_install(serde_json::json!({"server": "github"})).await.unwrap();
        assert!(result.contains("GITHUB_PERSONAL_ACCESS_TOKEN"));
    }
}
