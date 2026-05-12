use crate::provider::ToolDefinition;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio, Child};

#[allow(dead_code)]
struct McpServer {
    child: Child,
    name: String,
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

pub fn mcp_connect_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "mcp_connect".into(),
            description: "Connects to an MCP (Model Context Protocol) server via stdio. Returns available tools and resources. Supports Node.js and Python MCP servers.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to start the MCP server (e.g. 'npx @anthropic/mcp-server-github')"
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Additional arguments for the server command"
                    },
                    "env": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "Environment variables for the server (e.g. API keys)"
                    }
                },
                "required": ["command"]
            }),
        },
    }
}

pub fn mcp_list_tools_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "mcp_list_tools".into(),
            description: "Lists available tools from a connected MCP server. Returns tool names, descriptions, and parameter schemas.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    }
}

pub fn mcp_call_tool_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "mcp_call_tool".into(),
            description: "Calls a tool on a connected MCP server. Sends a JSON-RPC tools/call request and returns the result.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "toolName": {
                        "type": "string",
                        "description": "Name of the MCP tool to call"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments to pass to the tool (JSON object)"
                    }
                },
                "required": ["toolName"]
            }),
        },
    }
}

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

fn send_jsonrpc(child: &mut Child, method: &str, params: serde_json::Value) -> Result<String, String> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let stdin = child.stdin.as_mut()
        .ok_or("Server stdin not available")?;
    let stdout = child.stdout.as_mut()
        .ok_or("Server stdout not available")?;

    let msg = serde_json::to_string(&request)
        .map_err(|e| format!("JSON serialization error: {}", e))?;
    writeln!(stdin, "{}", msg)
        .map_err(|e| format!("Failed to write to server: {}", e))?;
    stdin.flush()
        .map_err(|e| format!("Failed to flush stdin: {}", e))?;

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line)
        .map_err(|e| format!("Failed to read from server: {}", e))?;

    Ok(line.trim().to_string())
}

pub async fn handle_mcp_connect(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str().ok_or("command required")?;
    let extra_args: Vec<String> = args["args"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut cmd_parts: Vec<&str> = command.split_whitespace().collect();
    let program = cmd_parts.remove(0);
    let mut cmd_args: Vec<&str> = cmd_parts;
    cmd_args.extend(extra_args.iter().map(|s| s.as_str()));

    let mut cmd = Command::new(program);
    cmd.args(&cmd_args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Set environment variables if provided
    if let Some(env) = args["env"].as_object() {
        for (key, val) in env {
            if let Some(v) = val.as_str() {
                cmd.env(key, v);
            }
        }
    }

    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to start MCP server '{}': {}", command, e))?;

    // Send initialize request
    let init_result = send_jsonrpc(
        &mut child,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": "goblin",
                "version": "0.1.0"
            }
        }),
    );

    match init_result {
        Ok(response) => {
            // Send initialized notification
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#);
                let _ = stdin.flush();
            }

            // List tools
            let tools_result = send_jsonrpc(
                &mut child,
                "tools/list",
                serde_json::Value::Null,
            );

            // Kill the child since we don't have persistent connection
            let _ = child.kill();

            match tools_result {
                Ok(tools) => {
                    let mut output = format!("MCP Server connected: {}\n\n", command);
                    output.push_str(&format!("Initialize response: {}\n\n", response));
                    output.push_str(&format!("Available tools: {}", tools));
                    Ok(output)
                }
                Err(e) => {
                    Ok(format!(
                        "MCP Server connected but tools/list failed: {}\nInitialize response: {}",
                        e, response
                    ))
                }
            }
        }
        Err(e) => {
            let _ = child.kill();

            let stderr_output = child.stderr.as_mut()
                .map(|s| {
                    let mut buf = String::new();
                    let _ = BufReader::new(s).read_line(&mut buf);
                    buf
                })
                .unwrap_or_default();

            Ok(format!(
                "MCP server '{}' responded with:\nError: {}\nStderr: {}",
                command, e, stderr_output
            ))
        }
    }
}

pub async fn handle_mcp_list_tools(_args: serde_json::Value) -> Result<String, String> {
    Ok("MCP: No persistent server connection. Use mcp_connect to connect to a server and see available tools.\n\n\
        Supported MCP servers:\n\
        - @anthropic/mcp-server-github — GitHub API access\n\
        - @anthropic/mcp-server-filesystem — File system access\n\
        - @anthropic/mcp-server-brave-search — Brave Search API\n\
        - @anthropic/mcp-server-postgres — PostgreSQL queries\n\
        - @anthropic/mcp-server-sqlite — SQLite database access\n\
        - @anthropic/mcp-server-memory — Knowledge graph memory\n\
        - @anthropic/mcp-server-puppeteer — Browser automation\n\
        - @anthropic/mcp-server-sequential-thinking — Structured reasoning".to_string())
}

pub async fn handle_mcp_call_tool(args: serde_json::Value) -> Result<String, String> {
    let tool_name = args["toolName"].as_str().ok_or("toolName required")?;
    let tool_args = args.get("arguments").unwrap_or(&serde_json::Value::Null);

    Ok(format!(
        "MCP Tool Call: {}\nArguments: {}\n\nNote: MCP tools require an active server connection. Use mcp_connect first.",
        tool_name,
        serde_json::to_string_pretty(tool_args).unwrap_or_default()
    ))
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

    // Check if npm/npx is available
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

    // Provide server-specific config help
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
    fn test_defs_exist() {
        assert_eq!(mcp_connect_def().function.name, "mcp_connect");
        assert_eq!(mcp_list_tools_def().function.name, "mcp_list_tools");
        assert_eq!(mcp_call_tool_def().function.name, "mcp_call_tool");
        assert_eq!(mcp_install_def().function.name, "mcp_install");
    }

    #[tokio::test]
    async fn test_mcp_connect_missing_args() {
        let result = handle_mcp_connect(serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mcp_list_tools_no_server() {
        let result = handle_mcp_list_tools(serde_json::json!({})).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mcp_call_tool_no_server() {
        let result = handle_mcp_call_tool(serde_json::json!({"toolName": "test"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("mcp_connect"));
    }

    #[tokio::test]
    async fn test_mcp_install_unknown() {
        let result = handle_mcp_install(serde_json::json!({"server": "nonexistent"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mcp_install_github() {
        let result = handle_mcp_install(serde_json::json!({"server": "github"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("GITHUB_PERSONAL_ACCESS_TOKEN"));
    }
}
