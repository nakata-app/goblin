//! IDE bridge — JSON-RPC server over TCP for VS Code / JetBrains integration.
//!
//! The bridge exposes the agent's capabilities through a standard JSON-RPC 2.0
//! protocol. IDEs connect via TCP (default port 9222), send requests, and
//! receive responses + notifications. This allows IDE extensions to embed
//! Aegis as an inline coding assistant without spawning a separate CLI process.
//!
//! ## Protocol
//!
//! - Transport: TCP (newline-delimited JSON, one message per line)
//! - Framing: JSON-RPC 2.0 (request/response/notification)
//!
//! ## Supported methods
//!
//! - `aegis/run` — run a prompt through the agent and return the result
//! - `aegis/tools` — list available tools
//! - `aegis/status` — return agent status (model, session, config)
//! - `aegis/cancel` — cancel a running agent invocation
//! - `aegis/shutdown` — gracefully shut down the server
//!
//! ## Notifications (server → client)
//!
//! - `aegis/textDelta` — streaming text output
//! - `aegis/toolCall` — tool invocation preview
//! - `aegis/toolResult` — tool result summary
//! - `aegis/usage` — token usage update

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use aegis_api::{ChatProvider, StreamEvent};
use aegis_core::{Agent, AgentConfig, Permission, SessionStore, ToolContext, ToolRegistry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// Start the IDE bridge server on the given port.
///
/// This is an async call that listens for TCP connections. Each
/// connection is handled in its own tokio task, allowing multiple IDE
/// clients to connect concurrently (e.g. multiple editor windows).
pub async fn run_ide_server(
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    model: &str,
    port: u16,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not bind IDE bridge to port {port}"))?;

    eprintln!("[goblin] IDE bridge listening on 127.0.0.1:{port}");
    eprintln!("[goblin] Connect your IDE extension to this address");

    let workspace = workspace.to_path_buf();
    let model = model.to_string();

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                eprintln!("[goblin] IDE client connected from {addr}");
                let client = Arc::clone(&client);
                let registry = Arc::clone(&registry);
                let ws = workspace.clone();
                let cfg = config.clone();
                let perm = Arc::clone(&permission);
                let mdl = model.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_connection(stream, client, registry, &ws, cfg, perm, &mdl).await
                    {
                        eprintln!("[goblin] IDE connection error: {e}");
                    }
                    eprintln!("[goblin] IDE client disconnected");
                });
            }
            Err(e) => {
                eprintln!("[goblin] IDE accept error: {e}");
            }
        }
    }
}

/// Handle a single IDE client connection.
async fn handle_connection(
    stream: TcpStream,
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    model: &str,
) -> Result<()> {
    let (reader_half, writer_half) = stream.into_split();
    let reader = BufReader::new(reader_half);
    let writer = Arc::new(Mutex::new(writer_half));

    // Create a session for this IDE connection
    let session_id = SessionStore::new_id();
    let session =
        SessionStore::open(workspace, &session_id).context("could not open IDE session")?;

    let hooks = aegis_core::load_hooks(workspace);
    let mut ctx = ToolContext::new(workspace.to_path_buf()).with_hooks(hooks);

    // Wire shared agent-spawner so IDE-driven sessions can fan out via
    // `agent` / `parallel_agents` tool calls.
    let spawner = crate::agent_spawner::build(
        Arc::clone(&client),
        Arc::clone(&registry),
        workspace,
        config.clone(),
        Arc::clone(&permission),
        ctx.background_agents.clone(),
    );
    ctx = ctx.with_agent_spawner(spawner);

    let mut agent = Agent::new(&*client, &registry, ctx, config.clone())
        .with_permission(permission)
        .with_guardrail(aegis_core::guardrail::load_default(workspace))
        .with_session(session);

    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send_error(&writer, Value::Null, -32700, &format!("parse error: {e}")).await;
                continue;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        match method {
            "metis/run" => {
                let prompt = params
                    .get("prompt")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string();

                if prompt.is_empty() {
                    send_error(&writer, id, -32602, "missing 'prompt' parameter").await;
                    continue;
                }

                // Set up streaming callback to send notifications
                let w = Arc::clone(&writer);
                agent = agent.with_stream_callback(move |event| {
                    let notification = match event {
                        StreamEvent::TextDelta(text) => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/textDelta",
                            "params": { "text": text }
                        }),
                        StreamEvent::ToolCall {
                            name,
                            arguments_preview,
                        } => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/toolCall",
                            "params": { "name": name, "preview": arguments_preview }
                        }),
                        StreamEvent::ToolResult {
                            name,
                            preview,
                            is_error,
                        } => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/toolResult",
                            "params": { "name": name, "preview": preview, "isError": is_error }
                        }),
                        StreamEvent::ThinkingDelta(text) => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/thinkingDelta",
                            "params": { "text": text }
                        }),
                        StreamEvent::Usage(usage) => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/usage",
                            "params": {
                                "prompt_tokens": usage.prompt_tokens,
                                "completion_tokens": usage.completion_tokens,
                            }
                        }),
                        StreamEvent::RetryReset => json!({
                            "jsonrpc": "2.0",
                            "method": "metis/retryReset",
                            "params": {}
                        }),
                    };
                    // Use block_in_place to send from the sync callback
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(send_json(&w, &notification));
                    });
                });

                match agent.run(prompt).await {
                    Ok(output) => {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "text": output.final_text,
                                "turns": output.turns,
                                "usage": {
                                    "input_tokens": output.usage.input_tokens,
                                    "output_tokens": output.usage.output_tokens,
                                    "cache_read_tokens": output.usage.cache_read_tokens,
                                    "cache_write_tokens": output.usage.cache_write_tokens,
                                }
                            }
                        });
                        send_json(&writer, &response).await;
                    }
                    Err(e) => {
                        send_error(&writer, id, -32000, &format!("agent error: {e}")).await;
                    }
                }
            }

            "metis/tools" => {
                let specs: Vec<Value> = registry
                    .specs()
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.function.name,
                            "description": s.function.description,
                        })
                    })
                    .collect();
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": specs }
                });
                send_json(&writer, &response).await;
            }

            "metis/status" => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "model": model,
                        "session": session_id,
                        "workspace": workspace.display().to_string(),
                    }
                });
                send_json(&writer, &response).await;
            }

            "metis/shutdown" => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "status": "shutting_down" }
                });
                send_json(&writer, &response).await;
                return Ok(());
            }

            _ => {
                send_error(&writer, id, -32601, &format!("method not found: {method}")).await;
            }
        }
    }

    Ok(())
}

/// Send a JSON-RPC response or notification.
async fn send_json(writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>, value: &Value) {
    if let Ok(mut w) = writer.try_lock() {
        let mut msg = serde_json::to_string(value).unwrap_or_default();
        msg.push('\n');
        let _ = w.write_all(msg.as_bytes()).await;
        let _ = w.flush().await;
    }
}

/// Send a JSON-RPC error response.
async fn send_error(
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    id: Value,
    code: i32,
    message: &str,
) {
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    });
    send_json(writer, &response).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_json_formats_newline_delimited() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, writer_half) = stream.into_split();
            let writer = Arc::new(Mutex::new(writer_half));
            send_json(&writer, &json!({"jsonrpc": "2.0", "id": 1, "result": "ok"})).await;
        });

        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();
        let line = lines.next_line().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["result"], "ok");

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn send_error_has_error_object() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, writer_half) = stream.into_split();
            let writer = Arc::new(Mutex::new(writer_half));
            send_error(&writer, json!(42), -32601, "not found").await;
        });

        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();
        let line = lines.next_line().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["error"]["code"], -32601);
        assert_eq!(parsed["error"]["message"], "not found");

        handle.await.unwrap();
    }
}
