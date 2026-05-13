use crate::tools::ToolRegistry;
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

    /// Start the MCP server on stdio. Blocks the calling thread.
    ///
    /// `registry` is the live Goblin ToolRegistry; tools/call requests
    /// dispatch into it directly. `tool_defs` is the metadata triple
    /// (name, description, schema) advertised on tools/list — kept
    /// separate so we can describe tools without holding the registry
    /// lock while we describe them.
    pub fn run_stdio(
        &self,
        registry: Arc<ToolRegistry>,
        tool_defs: Vec<(String, String, serde_json::Value)>,
    ) {
        {
            let mut running = self.running.lock().unwrap();
            *running = true;
        }

        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let reader = BufReader::new(stdin.lock());
        let writer = Arc::new(Mutex::new(stdout.lock()));

        // Tools execute async, but MCP stdio is a synchronous line
        // loop. We carry a tokio runtime here and block_on each call.
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[mcp-server] failed to build tokio runtime: {}", e);
                return;
            }
        };

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

            let response = handle_request(&request, &tool_defs, registry.as_ref(), &rt);

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
    registry: &ToolRegistry,
    rt: &tokio::runtime::Runtime,
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
                .unwrap_or("")
                .to_string();

            let tool_args = request.params
                .as_ref()
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Execute the real tool via the live ToolRegistry. tools
            // are async; block_on this synchronous request handler.
            let result = rt.block_on(registry.execute(&tool_name, tool_args));
            match result {
                Ok(output) => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(json!({
                        "content": [{ "type": "text", "text": output }]
                    })),
                    error: None,
                },
                Err(e) => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(json!({
                        "content": [{ "type": "text", "text": format!("Tool error: {}", e) }],
                        "isError": true
                    })),
                    error: None,
                },
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

    fn empty_registry() -> ToolRegistry {
        ToolRegistry::new()
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn handle_initialize() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: Some(json!({"protocolVersion": "2024-11-05"})),
        };
        let reg = empty_registry();
        let runtime = rt();
        let resp = handle_request(&req, &[], &reg, &runtime);
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
        let reg = empty_registry();
        let runtime = rt();
        let resp = handle_request(&req, &tools, &reg, &runtime);
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
        let reg = empty_registry();
        let runtime = rt();
        let resp = handle_request(&req, &[], &reg, &runtime);
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
        let reg = empty_registry();
        let runtime = rt();
        let resp = handle_request(&req, &[], &reg, &runtime);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn handle_tools_call_unknown_tool_returns_error_envelope() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: "tools/call".into(),
            params: Some(json!({"name": "no_such_tool", "arguments": {}})),
        };
        let reg = empty_registry();
        let runtime = rt();
        let resp = handle_request(&req, &[], &reg, &runtime);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], serde_json::Value::Bool(true));
    }
}
