//! Minimal async JSON-RPC 2.0 stdio client for Model Context Protocol
//! (MCP) servers.
//!
//! Uses `tokio::process` for child management and `tokio::io` for
//! async stdio framing. The MCP wire protocol is small enough that a
//! few hundred lines of Rust covers the surface area we need:
//!
//! * `initialize` — handshake with version + client info
//! * `tools/list` — discover the server's tool catalogue
//! * `tools/call` — invoke a single tool, get a string result
//!
//! Notifications, sampling, prompts, resources, and SSE transport are
//! intentionally out of scope for v0.4. They can be added later
//! without changing the call sites.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Errors that can surface from an MCP server interaction.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to spawn MCP server `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io error talking to MCP server: {0}")]
    Io(#[from] std::io::Error),
    #[error("MCP server closed its stdout before answering request {id}")]
    UnexpectedEof { id: i64 },
    #[error("could not parse MCP message: {0}")]
    Decode(String),
    #[error("MCP server returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("MCP server response missing `result` field")]
    MissingResult,
    #[error("MCP server `{label}` did not respond within {secs}s")]
    Timeout { label: String, secs: u64 },
}

pub type McpResult<T> = std::result::Result<T, McpError>;

/// Description of a single tool advertised by an MCP server, in the
/// shape we care about. Mirrors the `tools/list` response entries
/// without dragging in every optional MCP field. `Serialize` lets the
/// disk-backed cache (`aegis_core::McpCache`) round-trip this struct
/// without an intermediate DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing the tool arguments. MCP calls this
    /// `inputSchema`; we mirror that on the wire.
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

/// Result of a `tools/call` invocation. Text blocks are joined into `text`;
/// image blocks are preserved as raw JSON for the caller to convert.
#[derive(Debug, Clone)]
pub struct McpCallResult {
    pub text: String,
    pub is_error: bool,
    /// Raw MCP image blocks: `{"type":"image","data":"<base64>","mimeType":"image/png"}`.
    pub image_blocks: Vec<Value>,
}

/// A live, spawned MCP server child process plus the JSON-RPC framing
/// machinery used to talk to it. The struct owns the child and kills it
/// on drop, so dropping the client is the only cleanup callers need.
pub struct McpServer {
    /// Human-readable label for diagnostics — usually the command line.
    label: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Stderr drain task so the server can `eprintln!` freely without
    /// filling its pipe and deadlocking. The handle is kept so we can
    /// abort it on drop; the task exits naturally when the child closes
    /// its stderr.
    _stderr_pump: Option<tokio::task::JoinHandle<()>>,
    next_id: AtomicI64,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("label", &self.label)
            .finish()
    }
}

/// What to send in the `clientInfo` field of `initialize`. The MCP spec
/// requires a name and version; we send Metis's so servers can log who
/// connected.
const CLIENT_INFO_NAME: &str = "metis";
const CLIENT_INFO_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol version we negotiate. Server is free to downgrade in its
/// reply; we don't enforce a specific version because servers in the
/// wild lag the spec.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Default timeout for a single MCP JSON-RPC request. Long enough for
/// expensive tools (code search, web fetch) but short enough that a
/// stuck server doesn't hang the agent loop forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl McpServer {
    /// Spawns `command` with `args`, performs the MCP `initialize`
    /// handshake, and returns a ready-to-use server handle. The child's
    /// stdin/stdout are wired up; stderr is drained on a background
    /// tokio task so a chatty server cannot block the protocol pipe.
    pub async fn spawn(command: &str, args: &[String]) -> McpResult<Self> {
        let label = if args.is_empty() {
            command.to_string()
        } else {
            format!("{} {}", command, args.join(" "))
        };

        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|source| McpError::Spawn {
                cmd: label.clone(),
                source,
            })?;

        let stdin = child.stdin.take().expect("piped stdin requested");
        let stdout = child.stdout.take().expect("piped stdout requested");
        let stderr = child.stderr.take().expect("piped stderr requested");

        let stderr_pump = tokio::spawn(async move {
            // Read and discard. We don't surface MCP server stderr to
            // the agent loop today; future work could plumb it into the
            // metis log channel.
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => line.clear(),
                }
            }
        });

        let mut server = Self {
            label,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            _stderr_pump: Some(stderr_pump),
            next_id: AtomicI64::new(1),
        };

        server.initialize().await?;
        Ok(server)
    }

    /// Sends the MCP `initialize` request and the matching `initialized`
    /// notification. Caller must invoke this exactly once before any
    /// other RPC; `spawn` already does that internally.
    async fn initialize(&mut self) -> McpResult<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_INFO_NAME,
                "version": CLIENT_INFO_VERSION,
            }
        });
        let _ = self.request("initialize", params).await?;

        // Per the spec, the client must follow up with an `initialized`
        // notification (no id, no response expected) before issuing
        // further requests. Notifications are JSON-RPC messages without
        // an `id` field.
        let note = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        self.write_message(&note).await?;
        Ok(())
    }

    /// Returns the catalogue of tools the server exposes via the
    /// `tools/list` RPC. Pagination cursors are ignored — most servers
    /// in the wild fit comfortably in one page and we can revisit if a
    /// real corpus needs paging.
    pub async fn list_tools(&mut self) -> McpResult<Vec<McpToolInfo>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools_value = result
            .get("tools")
            .cloned()
            .ok_or_else(|| McpError::Decode("tools/list response missing `tools`".into()))?;
        serde_json::from_value::<Vec<McpToolInfo>>(tools_value)
            .map_err(|err| McpError::Decode(err.to_string()))
    }

    /// Invokes a single tool by name with a JSON arguments object.
    /// Flattens the MCP `content` array (which is a vector of typed
    /// blocks) into a single string suitable for the agent loop's tool
    /// message body.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> McpResult<McpCallResult> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        let result = self.request("tools/call", params).await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut chunks = Vec::new();
        let mut image_blocks = Vec::new();
        if let Some(content_array) = result.get("content").and_then(|v| v.as_array()) {
            for block in content_array {
                let kind = block
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if kind == "text" {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        chunks.push(text.to_string());
                    }
                } else if kind == "image" {
                    image_blocks.push(block.clone());
                } else {
                    chunks.push(format!("[{kind} content omitted]"));
                }
            }
        }
        let text = if chunks.is_empty() && image_blocks.is_empty() {
            "[ok]".to_string()
        } else {
            chunks.join("\n")
        };

        Ok(McpCallResult { text, is_error, image_blocks })
    }

    /// Human-readable label for diagnostics.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Issues a single JSON-RPC request and waits for the matching
    /// response. Reads lines from stdout until it finds one whose `id`
    /// matches the outgoing request — any unexpected notifications or
    /// log messages on the wire are skipped. Times out after
    /// [`REQUEST_TIMEOUT`] so a stuck server cannot hang the agent loop.
    async fn request(&mut self, method: &str, params: Value) -> McpResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&req).await?;

        let label = self.label.clone();
        let read_loop = async {
            loop {
                let mut line = String::new();
                let bytes = self.stdout.read_line(&mut line).await?;
                if bytes == 0 {
                    return Err(McpError::UnexpectedEof { id });
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(trimmed)
                    .map_err(|err| McpError::Decode(err.to_string()))?;

                let resp_id = value.get("id").and_then(|v| v.as_i64());
                if resp_id != Some(id) {
                    continue;
                }

                if let Some(err) = value.get("error") {
                    let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no message)")
                        .to_string();
                    return Err(McpError::Rpc { code, message });
                }

                return value.get("result").cloned().ok_or(McpError::MissingResult);
            }
        };

        tokio::time::timeout(REQUEST_TIMEOUT, read_loop)
            .await
            .map_err(|_| McpError::Timeout {
                label,
                secs: REQUEST_TIMEOUT.as_secs(),
            })?
    }

    async fn write_message(&mut self, message: &Value) -> McpResult<()> {
        let mut bytes =
            serde_json::to_vec(message).map_err(|err| McpError::Decode(err.to_string()))?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Best-effort cleanup: kill the child if it's still running.
        // tokio::process::Child::kill is a sync method in the start()
        // sense — it sends SIGKILL. We also call start_kill which is
        // non-blocking on tokio's Child.
        let _ = self.child.start_kill();
    }
}

/// JSON-RPC request envelope. Public so callers building their own
/// custom transports can reuse the type.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'a str,
    pub id: i64,
    pub method: &'a str,
    pub params: Value,
}

// ─── SSE Transport ──────────────────────────────────────────────────

/// MCP client that connects to a remote server via HTTP SSE (Server-Sent
/// Events) transport instead of spawning a child process.
///
/// Protocol: the server exposes an SSE endpoint (GET) that streams
/// JSON-RPC responses, and a POST endpoint that accepts JSON-RPC requests.
pub struct McpSseClient {
    label: String,
    /// SSE endpoint URL (GET — server pushes events here).
    sse_url: String,
    /// Message endpoint URL (POST — client sends requests here).
    post_url: Option<String>,
    http: reqwest::Client,
    next_id: AtomicI64,
}

impl McpSseClient {
    /// Connect to an MCP SSE server. The `url` is the SSE endpoint.
    /// The server's first SSE event should provide the POST endpoint URL.
    pub async fn connect(url: &str) -> McpResult<Self> {
        let http = reqwest::Client::new();

        // Start SSE connection to discover the POST endpoint
        let resp = http
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| McpError::Decode(format!("SSE connect failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(McpError::Decode(format!(
                "SSE endpoint returned {}",
                resp.status()
            )));
        }

        // Read the first SSE event to get the message endpoint
        let body = resp
            .text()
            .await
            .map_err(|e| McpError::Decode(format!("SSE read: {e}")))?;

        // Parse SSE: look for "event: endpoint" with "data: <url>"
        let post_url = body.lines().find(|l| l.starts_with("data:")).map(|l| {
            let data = l.strip_prefix("data:").unwrap().trim();
            // If relative URL, resolve against base
            if data.starts_with('/') {
                if let Ok(base) = reqwest::Url::parse(url) {
                    if let Ok(resolved) = base.join(data) {
                        return resolved.to_string();
                    }
                }
            }
            data.to_string()
        });

        let mut client = Self {
            label: url.to_string(),
            sse_url: url.to_string(),
            post_url,
            http,
            next_id: AtomicI64::new(1),
        };

        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> McpResult<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_INFO_NAME,
                "version": CLIENT_INFO_VERSION,
            }
        });
        let _result = self.request("initialize", params).await?;
        // Send initialized notification (no response expected)
        let post_url = self.post_url().ok_or(McpError::MissingResult)?;
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        let _ = self.http.post(&post_url).json(&notif).send().await;
        Ok(())
    }

    fn post_url(&self) -> Option<String> {
        self.post_url.clone()
    }

    /// List available tools from the SSE server.
    pub async fn list_tools(&mut self) -> McpResult<Vec<McpToolInfo>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(McpToolInfo {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            input_schema: t.get("inputSchema").cloned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tools)
    }

    /// Call a tool on the SSE server.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> McpResult<McpCallResult> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        let result = self.request("tools/call", params).await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut chunks = Vec::new();
        let mut image_blocks = Vec::new();
        if let Some(blocks) = result.get("content").and_then(|v| v.as_array()) {
            for block in blocks {
                let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "image" {
                    image_blocks.push(block.clone());
                } else if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    chunks.push(t.to_string());
                }
            }
        } else {
            chunks.push(serde_json::to_string_pretty(&result).unwrap_or_default());
        }
        let text = chunks.join("\n");
        Ok(McpCallResult { text, is_error, image_blocks })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Send a JSON-RPC request via POST and read the response.
    async fn request(&mut self, method: &str, params: Value) -> McpResult<Value> {
        let post_url = self
            .post_url
            .as_deref()
            .ok_or(McpError::Decode("no POST endpoint discovered".into()))?
            .to_string();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp =
            tokio::time::timeout(REQUEST_TIMEOUT, self.http.post(&post_url).json(&req).send())
                .await
                .map_err(|_| McpError::Timeout {
                    label: self.label.clone(),
                    secs: REQUEST_TIMEOUT.as_secs(),
                })?
                .map_err(|e| McpError::Decode(format!("POST failed: {e}")))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Decode(format!("response json: {e}")))?;

        if let Some(err) = body.get("error") {
            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)")
                .to_string();
            return Err(McpError::Rpc { code, message });
        }

        body.get("result").cloned().ok_or(McpError::MissingResult)
    }
}

impl std::fmt::Debug for McpSseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSseClient")
            .field("label", &self.label)
            .field("sse_url", &self.sse_url)
            .field("post_url", &self.post_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Writes a tiny shell script that pretends to be an MCP server:
    /// it answers `initialize` with an empty result, `tools/list` with a
    /// single fake `echo` tool, and `tools/call` by echoing back the
    /// `arguments.message` field. Returns the script's path inside the
    /// caller-provided tempdir so it gets cleaned up automatically.
    fn write_fake_server_script(dir: &std::path::Path) -> std::path::PathBuf {
        let script = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method", "")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo back", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        result = {"content": [{"type": "text", "text": args.get("message", "")}]}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}) + "\n")
    sys.stdout.flush()
"#;
        let path = dir.join("fake_mcp_server.py");
        let mut f = std::fs::File::create(&path).expect("create script");
        f.write_all(script.as_bytes()).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn fake_server_initialize_list_call_round_trip() {
        // Skip on hosts without python3 — CI installs it via the toolchain.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 unavailable, skipping MCP smoke test");
            return;
        }

        let dir = std::env::temp_dir().join(format!("metis-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = write_fake_server_script(&dir);

        let mut server = McpServer::spawn("python3", &[script.to_string_lossy().into_owned()])
            .await
            .expect("spawn fake server");

        let tools = server.list_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("echo back"));

        let result = server
            .call_tool("echo", &json!({"message": "hello mcp"}))
            .await
            .expect("call tool");
        assert!(!result.is_error);
        assert_eq!(result.text, "hello mcp");

        // Cleanup
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_method_surfaces_as_decode_or_rpc_error_not_panic() {
        // Drives the request loop against a server that closes stdout
        // immediately, exercising the EOF branch. We use `true` which
        // exits 0 with no output.
        let server_result = McpServer::spawn("true", &[]).await;
        match &server_result {
            Ok(_) => panic!("expected error from `true` as MCP server"),
            Err(McpError::UnexpectedEof { .. }) | Err(McpError::Io(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }
}
