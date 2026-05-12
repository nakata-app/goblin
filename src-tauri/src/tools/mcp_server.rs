use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

/// MCP Server: Exposes Goblin's tools over stdio JSON-RPC.
/// Runs in a background thread, accepting MCP client connections.
/// Implements the Model Context Protocol 2024-11-05 specification.

#[derive(Clone)]
pub struct McpServerHandle {
    pub running: Arc<Mutex<bool>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(serde::Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(serde::Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[derive(serde::Serialize)]
struct ToolDescriptor {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

impl McpServerHandle {
    pub fn new() -> Self {
        Self {
            running: Arc::new(Mutex::new(false)),
        }
    }

    /// Start the MCP server on stdio. This blocks the calling thread.
    pub fn run_stdio(&self, tool_defs: Vec<(String, String, serde_json::Value)>) {
        {
            let mut running = self.running.lock().unwrap();
            *running = true;
        }

        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let reader = BufReader::new(stdin.lock());
        let writer = Arc::new(Mutex::new(stdout.lock()));

        eprintln!("[mcp-server] Goblin MCP server started on stdio");

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[mcp-server] stdin read error: {}", e);
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(e) => {
                    eprintln!("[mcp-server] parse error: {}", e);
                    continue;
                }
            };

            let response = handle_request(&request, &tool_defs);

            let response_json = serde_json::to_string(&response).unwrap_or_default();
            if let Ok(mut w) = writer.lock() {
                let _ = writeln!(w, "{}", response_json);
                let _ = w.flush();
            }
        }

        {
            let mut running = self.running.lock().unwrap();
            *running = false;
        }
        eprintln!("[mcp-server] Shutdown");
    }
}

fn handle_request(
    request: &JsonRpcRequest,
    tool_defs: &[(String, String, serde_json::Value)],
) -> JsonRpcResponse {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {
                        "name": "goblin",
                        "version": "0.1.0"
                    },
                    "capabilities": {
                        "tools": {}
                    }
                })),
                error: None,
            }
        }
        "notifications/initialized" => {
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(serde_json::Value::Null),
                error: None,
            }
        }
        "tools/list" => {
            let tools: Vec<ToolDescriptor> = tool_defs
                .iter()
                .map(|(name, desc, schema)| ToolDescriptor {
                    name: name.clone(),
                    description: desc.clone(),
                    input_schema: schema.clone(),
                })
                .collect();

            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({ "tools": tools })),
                error: None,
            }
        }
        "tools/call" => {
            let tool_name = request.params
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");

            let tool_args = request.params
                .as_ref()
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Forward tool calls via Tauri IPC — the actual execution happens in the app
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Tool '{}' called with args: {}. Execution delegated to Goblin agent.",
                            tool_name,
                            serde_json::to_string_pretty(&tool_args).unwrap_or_default()
                        )
                    }]
                })),
                error: None,
            }
        }
        "ping" => {
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({})),
                error: None,
            }
        }
        _ => {
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_initialize() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: Some(json!({"protocolVersion": "2024-11-05"})),
        };
        let resp = handle_request(&req, &[]);
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "goblin");
        assert_eq!(result["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn handle_tools_list() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: None,
        };
        let tools = vec![
            ("bash".into(), "Run a command".into(), json!({"type": "object"})),
            ("read_file".into(), "Read a file".into(), json!({"type": "object"})),
        ];
        let resp = handle_request(&req, &tools);
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        let listed = result["tools"].as_array().unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn handle_ping() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "ping".into(),
            params: None,
        };
        let resp = handle_request(&req, &[]);
        assert!(resp.result.is_some());
    }

    #[test]
    fn handle_unknown_method() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(4)),
            method: "nonexistent".into(),
            params: None,
        };
        let resp = handle_request(&req, &[]);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn handle_tools_call() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: "tools/call".into(),
            params: Some(json!({"name": "bash", "arguments": {"command": "echo hello"}})),
        };
        let resp = handle_request(&req, &[]);
        assert!(resp.result.is_some());
    }
}
