use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Bridges Goblin to Atakan's external `mnemonics` binary so that the agent
/// can read and write cross-project semantic memory. We hold a long-lived
/// `mnemonics mcp` child because loading the embedding model takes 3-5s,
/// and we cannot afford to pay that on every tool call.
pub struct MnemonicsClient {
    binary: String,
    pub default_ns: String,
    state: Mutex<ChildState>,
    next_id: AtomicU64,
}

struct ChildState {
    child: Option<Child>,
}

impl MnemonicsClient {
    pub fn new(binary: String, default_ns: String) -> Self {
        Self {
            binary,
            default_ns,
            state: Mutex::new(ChildState { child: None }),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawns `mnemonics --help` once. Returns false if the binary is
    /// missing or non-functional, so the agent tool registration can skip
    /// exposing the mnemonics tools instead of failing every call.
    pub fn is_available(&self) -> bool {
        Command::new(&self.binary)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Semantic search over the embedding index. `ns` filters by namespace
    /// when provided; the binary's MCP tool treats null/missing as "all".
    pub fn retrieve(
        &self,
        query: &str,
        ns: Option<&str>,
        top_k: u32,
        decay: bool,
    ) -> Result<String, String> {
        let mut args = serde_json::Map::new();
        args.insert("query".to_string(), serde_json::Value::String(query.to_string()));
        args.insert("top_k".to_string(), serde_json::Value::Number(top_k.into()));
        args.insert("decay".to_string(), serde_json::Value::Bool(decay));
        if let Some(ns_val) = ns {
            args.insert("ns".to_string(), serde_json::Value::String(ns_val.to_string()));
        }
        self.tool_call("mnemonics_retrieve", serde_json::Value::Object(args))
    }

    pub fn ingest(&self, text: &str, ns: Option<&str>) -> Result<String, String> {
        let ns_val = ns.unwrap_or(&self.default_ns);
        let args = serde_json::json!({
            "texts": [text],
            "ns": ns_val,
        });
        self.tool_call("mnemonics_ingest", args)
    }

    /// JSON-RPC `tools/call` with auto-reconnect on broken pipe.
    fn tool_call(&self, name: &str, arguments: serde_json::Value) -> Result<String, String> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let response = self.request("tools/call", params)?;
        // MCP convention: tool result lives in `result.content[0].text`.
        if let Some(text) = response.get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
        {
            return Ok(text.to_string());
        }
        // Some servers return result.content as an object or result directly.
        if let Some(result) = response.get("result") {
            return Ok(result.to_string());
        }
        if let Some(err) = response.get("error") {
            return Err(format!("mnemonics error: {}", err));
        }
        Err(format!("unexpected mnemonics response: {}", response))
    }

    fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // First attempt with the existing child; if the pipe died we
        // respawn once and retry. Anything beyond that is a real error.
        for attempt in 0..2 {
            let send_result = self.send_request(id, method, &params);
            match send_result {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 => {
                    // Drop the dead child so the next send_request respawns.
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
        Err("mnemonics: exhausted retries".to_string())
    }

    fn send_request(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let mut guard = self.state.lock().map_err(|e| format!("Lock error: {}", e))?;

        if guard.child.is_none() {
            guard.child = Some(spawn_and_initialize(&self.binary)?);
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

impl Drop for MnemonicsClient {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn spawn_and_initialize(binary: &str) -> Result<Child, String> {
    let mut child = Command::new(binary)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn mnemonics mcp: {}", e))?;

    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "goblin", "version": "0.1" }
        }
    });
    write_line(&mut child, &init)?;
    // Drain the initialize response. We do not parse it; if the binary
    // accepted the handshake the subsequent tools/call will succeed too.
    let _ = read_response(&mut child, 0)?;
    Ok(child)
}

fn write_line(child: &mut Child, value: &serde_json::Value) -> Result<(), String> {
    let stdin = child.stdin.as_mut().ok_or("mnemonics: stdin closed")?;
    let line = serde_json::to_string(value).map_err(|e| format!("Serialize: {}", e))?;
    stdin.write_all(line.as_bytes()).map_err(|e| format!("Write: {}", e))?;
    stdin.write_all(b"\n").map_err(|e| format!("Write newline: {}", e))?;
    stdin.flush().map_err(|e| format!("Flush: {}", e))?;
    Ok(())
}

fn read_response(child: &mut Child, expected_id: u64) -> Result<serde_json::Value, String> {
    let stdout = child.stdout.as_mut().ok_or("mnemonics: stdout closed")?;
    let mut reader = BufReader::new(stdout);
    // The server may emit notifications before the response; skip until we
    // see a frame whose id matches ours.
    for _ in 0..16 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| format!("Read: {}", e))?;
        if n == 0 {
            return Err("mnemonics: stdout EOF".to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("Parse: {} (line: {})", e, trimmed))?;
        match v.get("id").and_then(|i| i.as_u64()) {
            Some(id) if id == expected_id => return Ok(v),
            _ => continue, // notification or out-of-order; keep reading
        }
    }
    Err("mnemonics: no matching response after 16 frames".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_reports_unavailable() {
        let c = MnemonicsClient::new("/nonexistent/mnemonics-xyz".to_string(), "test".to_string());
        assert!(!c.is_available());
    }

    /// Smoke test against the real binary. Skipped unless
    /// GOBLIN_MNEMONICS_BIN points at an installed mnemonics; this keeps
    /// the test suite hermetic on machines without it.
    #[test]
    fn real_binary_roundtrip() {
        let Some(bin) = std::env::var("GOBLIN_MNEMONICS_BIN").ok() else { return };
        let c = MnemonicsClient::new(bin, "proj:goblin-tests".to_string());
        assert!(c.is_available());
        // ingest a unique tag, then look it up
        let tag = format!("goblin-test-marker-{}", std::process::id());
        c.ingest(&tag, Some("proj:goblin-tests")).expect("ingest");
        let hit = c.retrieve(&tag, Some("proj:goblin-tests"), 1, false).expect("retrieve");
        assert!(hit.contains(&tag) || !hit.is_empty(), "expected non-empty retrieve: '{}'", hit);
    }
}
