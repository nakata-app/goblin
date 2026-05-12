//! Generic MCP (Model Context Protocol) client hub. Auto-boots every
//! server declared in `[mcp.servers.*]`, keeps a long-lived stdio
//! JSON-RPC connection to each, and exposes a single dispatcher so the
//! agent can call any tool from any configured server.
//!
//! Why a hub instead of registering one Goblin tool per MCP tool:
//! discovery happens at boot time and the agent's tool list is built
//! once. Re-registering when an MCP server adds tools mid-session would
//! mean rebuilding the agent. The dispatcher approach pays a small UX
//! cost (LLM has to learn `mcp_call`) for a much simpler lifecycle.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSummary {
    pub name: String,
    pub description: Option<String>,
    /// Raw JSON Schema as advertised by the MCP server. Forwarded to the
    /// LLM verbatim so it can shape its tool call correctly.
    pub input_schema: serde_json::Value,
}

pub struct McpServerProc {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    state: Mutex<ChildState>,
    next_id: AtomicU64,
    tools: Mutex<Vec<McpToolSummary>>,
}

struct ChildState {
    child: Option<Child>,
}

impl McpServerProc {
    pub fn new(
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            name,
            command,
            args,
            env,
            state: Mutex::new(ChildState { child: None }),
            next_id: AtomicU64::new(1),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// One-shot probe: spawn, initialize, list tools, store the list. If
    /// any of that fails we return Err and the hub skips this server
    /// instead of leaving a half-broken connection live.
    pub fn boot(&self) -> Result<(), String> {
        // Send initialize as part of spawn so any failure shows up here
        // before we declare the server "ready".
        self.ensure_child()?;
        let listing = self.request("tools/list", serde_json::Value::Null)?;
        let tools = parse_tools_list(&listing).unwrap_or_default();
        *self.tools.lock().unwrap() = tools;
        Ok(())
    }

    pub fn list_tools(&self) -> Vec<McpToolSummary> {
        self.tools.lock().unwrap().clone()
    }

    pub fn call(&self, tool: &str, arguments: serde_json::Value) -> Result<String, String> {
        let params = serde_json::json!({ "name": tool, "arguments": arguments });
        let response = self.request("tools/call", params)?;
        // MCP convention: result.content[0].text.
        if let Some(text) = response.get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
        {
            return Ok(text.to_string());
        }
        if let Some(result) = response.get("result") {
            return Ok(result.to_string());
        }
        if let Some(err) = response.get("error") {
            return Err(format!("MCP '{}::{}' error: {}", self.name, tool, err));
        }
        Err(format!("unexpected MCP response from '{}::{}': {}", self.name, tool, response))
    }

    fn ensure_child(&self) -> Result<(), String> {
        let mut guard = self.state.lock().map_err(|e| format!("Lock error: {}", e))?;
        if guard.child.is_some() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {}", self.name, e))?;

        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "clientInfo": { "name": "goblin", "version": "0.1" }
            }
        });
        write_line(&mut child, &init)?;
        let _ = read_response(&mut child, 0)?;

        guard.child = Some(child);
        Ok(())
    }

    fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        for attempt in 0..2 {
            match self.send_request(id, method, &params) {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 => {
                    if let Ok(mut guard) = self.state.lock() {
                        if let Some(mut c) = guard.child.take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                    }
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
        Err(format!("mcp '{}' exhausted retries", self.name))
    }

    fn send_request(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let mut guard = self.state.lock().map_err(|e| format!("Lock error: {}", e))?;
        if guard.child.is_none() {
            // ensure_child also holds the same mutex, so do it inline.
            drop(guard);
            self.ensure_child()?;
            guard = self.state.lock().map_err(|e| format!("Lock error: {}", e))?;
        }
        let child = guard.child.as_mut().unwrap();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        write_line(child, &request)?;
        read_response(child, id)
    }
}

impl Drop for McpServerProc {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub struct McpHub {
    servers: HashMap<String, std::sync::Arc<McpServerProc>>,
}

impl McpHub {
    pub fn new() -> Self {
        Self { servers: HashMap::new() }
    }

    /// Spawn every server in `entries` (probably from
    /// `config.mcp.servers`). Failures are logged and skipped so a single
    /// broken server cannot disable the whole subsystem.
    pub fn boot_from_config(
        entries: impl IntoIterator<Item = (String, crate::config::McpServerConfig)>,
    ) -> Self {
        let mut hub = Self::new();
        for (name, cfg) in entries {
            if !cfg.enabled {
                continue;
            }
            let proc = std::sync::Arc::new(McpServerProc::new(
                name.clone(),
                cfg.command.clone(),
                cfg.args.clone(),
                cfg.env.clone(),
            ));
            match proc.boot() {
                Ok(()) => {
                    let n = proc.list_tools().len();
                    println!("[mcp] '{}' booted with {} tool(s).", name, n);
                    hub.servers.insert(name, proc);
                }
                Err(e) => {
                    eprintln!("[mcp] '{}' boot failed: {} — skipping.", name, e);
                }
            }
        }
        hub
    }

    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.servers.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn list_tools(&self, server: &str) -> Option<Vec<McpToolSummary>> {
        self.servers.get(server).map(|s| s.list_tools())
    }

    pub fn call(&self, server: &str, tool: &str, args: serde_json::Value) -> Result<String, String> {
        let proc = self.servers.get(server)
            .ok_or_else(|| format!("Unknown MCP server: {}", server))?;
        proc.call(tool, args)
    }
}

fn parse_tools_list(response: &serde_json::Value) -> Option<Vec<McpToolSummary>> {
    let tools = response.get("result")?.get("tools")?.as_array()?;
    let mut out = Vec::new();
    for t in tools {
        let name = t.get("name").and_then(|v| v.as_str())?.to_string();
        let description = t.get("description").and_then(|v| v.as_str()).map(String::from);
        let input_schema = t.get("inputSchema").cloned().unwrap_or(serde_json::Value::Null);
        out.push(McpToolSummary { name, description, input_schema });
    }
    Some(out)
}

fn write_line(child: &mut Child, value: &serde_json::Value) -> Result<(), String> {
    let stdin = child.stdin.as_mut().ok_or("mcp: stdin closed")?;
    let line = serde_json::to_string(value).map_err(|e| format!("Serialize: {}", e))?;
    stdin.write_all(line.as_bytes()).map_err(|e| format!("Write: {}", e))?;
    stdin.write_all(b"\n").map_err(|e| format!("Write newline: {}", e))?;
    stdin.flush().map_err(|e| format!("Flush: {}", e))?;
    Ok(())
}

fn read_response(child: &mut Child, expected_id: u64) -> Result<serde_json::Value, String> {
    let stdout = child.stdout.as_mut().ok_or("mcp: stdout closed")?;
    let mut reader = BufReader::new(stdout);
    for _ in 0..16 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| format!("Read: {}", e))?;
        if n == 0 {
            return Err("mcp: stdout EOF".to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("Parse: {} (line: {})", e, trimmed))?;
        match v.get("id").and_then(|i| i.as_u64()) {
            Some(id) if id == expected_id => return Ok(v),
            _ => continue,
        }
    }
    Err("mcp: no matching response after 16 frames".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_list_empty() {
        let v = serde_json::json!({"result": {"tools": []}});
        let tools = parse_tools_list(&v).unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn parse_tools_list_basic() {
        let v = serde_json::json!({
            "result": {"tools": [
                {"name": "search", "description": "Web search", "inputSchema": {"type": "object"}}
            ]}
        });
        let tools = parse_tools_list(&v).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description.as_deref(), Some("Web search"));
    }

    #[test]
    fn parse_tools_list_missing_input_schema() {
        // Some servers omit inputSchema; we accept null instead of erroring.
        let v = serde_json::json!({
            "result": {"tools": [
                {"name": "noargs"}
            ]}
        });
        let tools = parse_tools_list(&v).unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].input_schema.is_null());
    }

    #[test]
    fn parse_tools_list_malformed_returns_none() {
        let v = serde_json::json!({"error": "no result"});
        assert!(parse_tools_list(&v).is_none());
    }

    #[test]
    fn hub_unknown_server_errors() {
        let hub = McpHub::new();
        let err = hub.call("ghost", "anything", serde_json::Value::Null).unwrap_err();
        assert!(err.contains("Unknown MCP server"));
    }

    #[test]
    fn boot_from_config_skips_disabled() {
        let mut cfg = std::collections::HashMap::new();
        cfg.insert(
            "off".to_string(),
            crate::config::McpServerConfig {
                command: "/bin/true".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                enabled: false,
            },
        );
        let hub = McpHub::boot_from_config(cfg);
        assert!(hub.server_names().is_empty());
    }

    /// Real-binary smoke test: boots the mnemonics MCP server and verifies
    /// `tools/list` actually returns tools. Skipped when
    /// GOBLIN_MNEMONICS_BIN is not set.
    #[test]
    fn real_server_boot_lists_tools() {
        let Some(bin) = std::env::var("GOBLIN_MNEMONICS_BIN").ok() else { return };
        let mut env = std::collections::HashMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );
        let mut cfg = std::collections::HashMap::new();
        cfg.insert(
            "mnemonics".to_string(),
            crate::config::McpServerConfig {
                command: bin,
                args: vec!["mcp".to_string()],
                env,
                enabled: true,
            },
        );
        let hub = McpHub::boot_from_config(cfg);
        assert_eq!(hub.server_names(), vec!["mnemonics".to_string()]);
        let tools = hub.list_tools("mnemonics").unwrap();
        assert!(!tools.is_empty(), "expected some tools, got 0");
        assert!(tools.iter().any(|t| t.name.contains("retrieve")));
    }
}
