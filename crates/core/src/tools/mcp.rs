//! MCP-related tools — `mcp_authenticate` (OAuth helper) and the
//! `McpTool` bridge that wraps every tool advertised by an MCP server.
//!
//! `register_mcp_server` spawns an MCP child, lists its tools, and
//! registers one `McpTool` per advertised tool. All tools from the same
//! server share a single async-mutex handle so concurrent JSON-RPC
//! calls serialise on the stdio pipe.

use std::io::{Read, Write as IoWrite};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aegis_api::ContentBlock;
use aegis_mcp::{McpServer, McpToolInfo};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError, ToolRegistry};

// ---------------------------------------------------------------------
// McpAuthenticate — OAuth helper for MCP server auth flows
// ---------------------------------------------------------------------

pub struct McpAuthenticate;

#[derive(Debug, Deserialize)]
struct McpAuthArgs {
    /// The OAuth authorization URL to open in the user's browser.
    auth_url: String,
    /// Service name for credential storage (e.g. "gmail", "calendar").
    service: String,
    /// Port for the local callback server (default 9120).
    #[serde(default = "default_auth_port")]
    port: u16,
    /// Timeout in seconds to wait for the callback (default 120).
    #[serde(default = "default_auth_timeout")]
    timeout_secs: u64,
}

fn default_auth_port() -> u16 {
    9120
}
fn default_auth_timeout() -> u64 {
    120
}

#[async_trait]
impl Tool for McpAuthenticate {
    fn name(&self) -> &str {
        "mcp_authenticate"
    }
    fn description(&self) -> &str {
        "Handle OAuth authentication for MCP servers. Opens the auth URL \
         in the user's browser and starts a local callback server to receive \
         the token. Credentials are persisted in ~/.metis/credentials/."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "auth_url": {
                    "type": "string",
                    "description": "OAuth authorization URL to open in the browser"
                },
                "service": {
                    "type": "string",
                    "description": "Service name for credential storage (e.g. 'gmail', 'calendar')"
                },
                "port": {
                    "type": "integer",
                    "description": "Port for local OAuth callback server (default 9120)",
                    "default": 9120
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Seconds to wait for OAuth callback (default 120)",
                    "default": 120
                }
            },
            "required": ["auth_url", "service"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: McpAuthArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Ensure credentials directory exists
        let home = dirs::home_dir().ok_or_else(|| {
            ToolError::InvalidArgs("could not determine home directory".to_string())
        })?;
        let cred_dir = home.join(".metis").join("credentials");
        std::fs::create_dir_all(&cred_dir).map_err(|e| ToolError::Io {
            path: cred_dir.display().to_string(),
            source: e,
        })?;

        // Build callback URL
        let callback_url = format!("http://127.0.0.1:{}/callback", args.port);

        // Open browser to auth URL (append redirect_uri if not already present)
        let full_url = if args.auth_url.contains("redirect_uri") {
            args.auth_url.clone()
        } else {
            let sep = if args.auth_url.contains('?') {
                "&"
            } else {
                "?"
            };
            format!(
                "{}{sep}redirect_uri={}",
                args.auth_url,
                urlencoding(&callback_url)
            )
        };

        // Open browser
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(&full_url).spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open")
            .arg(&full_url)
            .spawn();

        // No eprintln — TUI alt-screen would be corrupted

        // Start callback server
        let listener = std::net::TcpListener::bind(("127.0.0.1", args.port))
            .map_err(|e| ToolError::Spawn(format!("could not bind port {}: {e}", args.port)))?;

        let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ToolError::Spawn(format!(
                    "OAuth callback timed out after {}s",
                    args.timeout_secs
                )));
            }

            listener.set_nonblocking(true).ok();
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut buf = vec![0u8; 65536];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();

                    // Parse the callback URL for code/token parameters
                    let first_line = raw.lines().next().unwrap_or("");
                    if let Some(path) = first_line.split_whitespace().nth(1) {
                        if let Some(query) = path.split_once('?').map(|(_, q)| q) {
                            // Extract code or access_token from query
                            let params: std::collections::HashMap<String, String> = query
                                .split('&')
                                .filter_map(|p| {
                                    let (k, v) = p.split_once('=')?;
                                    Some((k.to_string(), v.to_string()))
                                })
                                .collect();

                            // Send success response to browser
                            let html = "<html><body><h2>Authentication successful!</h2>\
                                        <p>You can close this tab.</p></body></html>";
                            let resp =
                                format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n{html}");
                            let _ = stream.write_all(resp.as_bytes());

                            // Save credentials
                            let cred_file = cred_dir.join(format!("{}.json", args.service));
                            let cred_json = serde_json::to_string_pretty(&params)
                                .unwrap_or_else(|_| "{}".to_string());
                            std::fs::write(&cred_file, &cred_json).map_err(|e| ToolError::Io {
                                path: cred_file.display().to_string(),
                                source: e,
                            })?;

                            let token_preview = params
                                .get("code")
                                .or_else(|| params.get("access_token"))
                                .cloned()
                                .unwrap_or_else(|| "(no code/token found)".to_string());

                            return Ok(format!(
                                "OAuth callback received for {}. Credentials saved to {}.\n\
                                 Token/code: {}...{}",
                                args.service,
                                cred_file.display(),
                                &token_preview[..token_preview.len().min(8)],
                                if token_preview.len() > 8 {
                                    "(truncated)"
                                } else {
                                    ""
                                }
                            ));
                        }
                    }
                    // Not a callback request — send 404 and continue
                    let resp = "HTTP/1.1 404 Not Found\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(ToolError::Spawn(format!("accept failed: {e}")));
                }
            }
        }
    }
}

/// Minimal percent-encoding for URLs (spaces, special chars).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// MCP bridge — every tool an MCP server exposes shows up in the registry
// as a regular `Tool`, so the agent loop and the cost meter don't need
// to know they aren't built-in.
// ---------------------------------------------------------------------

/// Wraps a single tool advertised by an MCP server. The wrapper owns its
/// `name` and `description` strings (so it can hand back borrowed `&str`
/// from the trait) and shares a `Mutex<McpServer>` with every other tool
/// from the same server, so that one server's tools serialise their
/// stdin/stdout traffic without us having to spawn the child more than
/// once.
///
/// The shared handle is stored as `Arc<tokio::sync::Mutex<Option<McpServer>>>`.
/// The `Option` exists so unit tests can construct an `McpTool` for spec
/// rendering without spawning a real child; in production code the
/// `register_mcp_server` helper always installs `Some(server)`.
pub struct McpTool {
    name: String,
    description: String,
    parameters: Value,
    server: Arc<tokio::sync::Mutex<Option<McpServer>>>,
    /// Spawn recipe for crash recovery — None for unit-test stubs.
    /// When the underlying child dies (broken pipe, EOF), we use this
    /// to respawn before failing the tool call.
    spec: Option<Arc<McpSpec>>,
}

/// Recipe used to respawn an MCP server after a crash. Stored as
/// `Arc<McpSpec>` and shared by every `McpTool` from the same server,
/// so a single respawn refreshes the handle for the whole sibling set.
#[derive(Debug, Clone)]
pub struct McpSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl McpTool {
    /// Builds a wrapper from a tool advertised by `tools/list`. The
    /// `parameters_schema` field is `inputSchema` from the MCP wire
    /// format, with a permissive `{"type": "object"}` fallback if the
    /// server omitted it.
    pub fn new(info: McpToolInfo, server: Arc<tokio::sync::Mutex<Option<McpServer>>>) -> Self {
        Self::with_spec(info, server, None)
    }

    /// Same as `new` but carries the spawn recipe needed for crash
    /// recovery. Production code (`spawn_mcp_server`) goes through
    /// here; unit tests use the no-spec `new`.
    pub fn with_spec(
        info: McpToolInfo,
        server: Arc<tokio::sync::Mutex<Option<McpServer>>>,
        spec: Option<Arc<McpSpec>>,
    ) -> Self {
        let mut parameters = info
            .input_schema
            .unwrap_or_else(|| json!({"type": "object"}));
        // Some MCP servers (e.g. claude-mem) emit `"type": null` which
        // DeepSeek and other strict providers reject. Normalise to "object".
        if parameters.get("type").map(|v| v.is_null()).unwrap_or(true) {
            if let Some(obj) = parameters.as_object_mut() {
                obj.insert("type".to_string(), json!("object"));
            }
        }
        let description = info.description.unwrap_or_default();
        Self {
            name: info.name,
            description,
            parameters,
            server,
            spec,
        }
    }
}

/// True when an `McpError` looks like the underlying child has died and
/// the server slot needs to be respawned (versus an in-protocol error
/// that the model should see and react to).
fn is_dead_server_error(err: &aegis_mcp::McpError) -> bool {
    matches!(
        err,
        aegis_mcp::McpError::UnexpectedEof { .. } | aegis_mcp::McpError::Io(_)
    )
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.parameters.clone()
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        // First attempt against the existing server handle.
        let first_err = {
            let mut guard = self.server.lock().await;
            let server = guard.as_mut().ok_or_else(|| ToolError::McpFailed {
                name: self.name.clone(),
                message: "server handle missing (test stub)".into(),
            })?;
            match server.call_tool(&self.name, &args).await {
                Ok(result) => {
                    if result.is_error {
                        return Err(ToolError::McpFailed {
                            name: self.name.clone(),
                            message: result.text,
                        });
                    }
                    return Ok(result.text);
                }
                Err(err) => {
                    // If the child is alive (in-protocol RPC error,
                    // decode error, timeout) we surface the failure as
                    // is — respawning would not change the answer.
                    // Only crashy errors trigger recovery.
                    if !is_dead_server_error(&err) {
                        return Err(ToolError::McpFailed {
                            name: self.name.clone(),
                            message: err.to_string(),
                        });
                    }
                    err
                }
            }
        };

        // Crash path: respawn the server, swap the handle, retry once.
        // Without a spec we can't respawn (test stub) — fall through
        // with the original error so the model sees what happened.
        let spec = match &self.spec {
            Some(s) => Arc::clone(s),
            None => {
                return Err(ToolError::McpFailed {
                    name: self.name.clone(),
                    message: format!("{first_err} (no respawn recipe — test stub)"),
                });
            }
        };

        let mut respawned = match McpServer::spawn(&spec.command, &spec.args).await {
            Ok(s) => s,
            Err(spawn_err) => {
                return Err(ToolError::McpFailed {
                    name: self.name.clone(),
                    message: format!(
                        "MCP server crashed ({first_err}) and respawn failed: {spawn_err}"
                    ),
                });
            }
        };
        let retry = respawned.call_tool(&self.name, &args).await;
        // Install the fresh server (or take it back on retry failure
        // so the next caller can try again with a still-alive child).
        {
            let mut guard = self.server.lock().await;
            *guard = Some(respawned);
        }
        match retry {
            Ok(result) => {
                if result.is_error {
                    Err(ToolError::McpFailed {
                        name: self.name.clone(),
                        message: format!("(after auto-respawn) {}", result.text),
                    })
                } else {
                    eprintln!(
                        "\x1b[2m[aegis] mcp `{}` respawned after crash\x1b[0m",
                        spec.command
                    );
                    Ok(result.text)
                }
            }
            Err(retry_err) => Err(ToolError::McpFailed {
                name: self.name.clone(),
                message: format!(
                    "MCP server crashed ({first_err}); respawned but retry also failed: {retry_err}"
                ),
            }),
        }
    }

    async fn execute_multimodal(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<super::ToolOutput, ToolError> {
        let mut guard = self.server.lock().await;
        let server = guard.as_mut().ok_or_else(|| ToolError::McpFailed {
            name: self.name.clone(),
            message: "server handle missing".into(),
        })?;
        let result = server.call_tool(&self.name, &args).await.map_err(|e| {
            ToolError::McpFailed {
                name: self.name.clone(),
                message: e.to_string(),
            }
        })?;
        if result.is_error {
            return Err(ToolError::McpFailed {
                name: self.name.clone(),
                message: result.text,
            });
        }
        if result.image_blocks.is_empty() {
            return Ok(super::ToolOutput::Text(result.text));
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !result.text.is_empty() {
            blocks.push(ContentBlock::Text { text: result.text.clone() });
        }
        for block in &result.image_blocks {
            let mime = block
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let data = block
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            blocks.push(ContentBlock::Image { media_type: mime, data });
        }
        Ok(super::ToolOutput::Multimodal {
            fallback_text: result.text,
            blocks,
        })
    }
}

/// Spawns an MCP server, lists its tools, wraps each one as an
/// [`McpTool`], and registers the lot with `registry`. Returns the
/// number of tools registered so callers can log a startup line.
///
/// All tools from a single server share one async mutex-guarded handle,
/// so concurrent calls from the agent loop serialise on a per-server
/// basis rather than racing on the JSON-RPC pipe.
pub async fn register_mcp_server(
    registry: &mut ToolRegistry,
    command: &str,
    args: &[String],
) -> Result<usize, aegis_mcp::McpError> {
    let handle = spawn_mcp_server(command, args).await?;
    Ok(handle.register_into(registry))
}

/// Spawned-but-unregistered MCP server, ready to be merged into a
/// `ToolRegistry`. Splitting spawn from registry lets the CLI start
/// every server's stdio handshake concurrently (the slow part) while
/// still merging into the single-threaded registry sequentially.
pub struct SpawnedMcpServer {
    infos: Vec<McpToolInfo>,
    shared: Arc<tokio::sync::Mutex<Option<McpServer>>>,
    spec: Arc<McpSpec>,
}

impl SpawnedMcpServer {
    pub fn tool_count(&self) -> usize {
        self.infos.len()
    }

    pub fn register_into(self, registry: &mut ToolRegistry) -> usize {
        let count = self.infos.len();
        for info in self.infos {
            registry.register(Box::new(McpTool::with_spec(
                info,
                Arc::clone(&self.shared),
                Some(Arc::clone(&self.spec)),
            )));
        }
        count
    }

    /// Late-register into a shared registry without needing exclusive access.
    /// Safe to call while the agent is running between turns.
    pub fn register_into_shared(self, registry: &Arc<ToolRegistry>) -> usize {
        let count = self.infos.len();
        for info in self.infos {
            registry.register_late(Box::new(McpTool::with_spec(
                info,
                Arc::clone(&self.shared),
                Some(Arc::clone(&self.spec)),
            )));
        }
        count
    }
}

/// Spawn an MCP child process and list its tools without touching any
/// registry. The returned handle owns the live server, the tool
/// metadata, and the spawn recipe needed to respawn after a crash.
/// Call `register_into` to attach the tools to a `ToolRegistry`.
pub async fn spawn_mcp_server(
    command: &str,
    args: &[String],
) -> Result<SpawnedMcpServer, aegis_mcp::McpError> {
    spawn_mcp_server_inner(command, args, None).await
}

/// Cache-aware variant of [`spawn_mcp_server`]. Behaves identically
/// (still spawns the live process, still calls `tools/list`) but
/// write-throughs the catalogue into
/// `<workspace>/.metis/mcp-cache.json` so future tooling — UI
/// previews, doc generators, debug commands — can answer "what tools
/// does this server expose" without paying the spawn + RPC cost. The
/// live agent loop still uses the freshly-listed tools from the
/// returned handle, so the cache lives strictly as a side-channel:
/// disk-write failures are advisory and never fail the spawn.
pub async fn spawn_mcp_server_with_cache(
    command: &str,
    args: &[String],
    workspace: &std::path::Path,
) -> Result<SpawnedMcpServer, aegis_mcp::McpError> {
    spawn_mcp_server_inner(command, args, Some(workspace)).await
}

async fn spawn_mcp_server_inner(
    command: &str,
    args: &[String],
    cache_workspace: Option<&std::path::Path>,
) -> Result<SpawnedMcpServer, aegis_mcp::McpError> {
    // Guard against supply-chain / config-injection attacks: the command
    // token is spawned directly, not via a shell, so shell metacharacters
    // in the binary name have no effect — but they are a strong signal that
    // someone is trying to inject a shell pipeline ("curl … | bash").
    // Legitimate MCP server binary names never contain these characters.
    const SHELL_META: &[char] = &['|', ';', '&', '$', '`', '>', '<', '(', ')', '{', '}'];
    if command.chars().any(|c| SHELL_META.contains(&c)) {
        return Err(aegis_mcp::McpError::Decode(format!(
            "MCP command `{command}` contains shell metacharacters — refusing to spawn. \
             Provide a plain binary path, not a shell pipeline."
        )));
    }
    let mut server = McpServer::spawn(command, args).await?;
    let infos = server.list_tools().await?;

    // Write-through to the on-disk catalogue cache. Disk failures are
    // advisory — boot must not fail because `.metis/` is read-only or
    // missing.
    if let Some(workspace) = cache_workspace {
        let mut cache = crate::mcp_cache::McpCache::load(workspace);
        let key = crate::mcp_cache::McpCache::key_for(command, args);
        cache.put(key, infos.clone());
        let _ = cache.save(workspace);
    }

    let shared = Arc::new(tokio::sync::Mutex::new(Some(server)));
    let spec = Arc::new(McpSpec {
        command: command.to_string(),
        args: args.to_vec(),
    });
    Ok(SpawnedMcpServer {
        infos,
        shared,
        spec,
    })
}
