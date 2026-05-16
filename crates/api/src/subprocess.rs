//! Claude Code subprocess provider — full agentic bridge.
//!
//! Runs `claude -p "<task>" --output-format stream-json --verbose` and
//! parses the JSON event stream so Aegis can display tool calls and
//! text deltas as they arrive, using the user's Pro/Max subscription.
//!
//! # Event format (observed from claude 2.1.126)
//!
//! ```json
//! {"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{...}}]}}
//! {"type":"user","message":{"content":[{"type":"tool_result","content":"...","tool_use_id":"..."}]}}
//! {"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}
//! {"type":"result","subtype":"success","result":"...","num_turns":2}
//! ```
//!
//! Tool calls are formatted as inline annotations (`⟳ Bash: ...`) so the
//! user can see what Claude is doing. Tool results are shown dimmed.
//! The final `result` field from the `type=result` event is the canonical
//! answer returned to the agent loop.

use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::{
    ApiError, ApiResult, ChatChoice, ChatMessage, ChatRequest, ChatResponse, Role, StreamEvent,
    Usage,
};

pub struct ClaudeSubprocessClient;

impl ClaudeSubprocessClient {
    pub fn new() -> Self {
        Self
    }

    pub fn is_available() -> bool {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|dir| std::path::Path::new(dir).join("claude").exists())
    }

    /// Format conversation history as a single prompt string for `-p`.
    fn format_conversation(messages: &[ChatMessage]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for msg in messages {
            let text = match &msg.content {
                Some(t) if !t.is_empty() => t.as_str(),
                _ => continue,
            };
            let labeled = match msg.role {
                Role::System => format!("[System context]: {text}"),
                Role::User => format!("Human: {text}"),
                Role::Assistant => format!("Assistant: {text}"),
                Role::Tool => continue,
            };
            parts.push(labeled);
        }
        parts.join("\n\n")
    }

    /// Parse one JSON line from the stream-json output and emit events.
    /// Returns the final result text if this is a `type=result` line.
    fn handle_line(line: &str, on_event: &mut (dyn FnMut(StreamEvent) + Send)) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        let kind = v.get("type")?.as_str()?;

        match kind {
            "assistant" => {
                let content = v.get("message")?.get("content")?.as_array()?;
                for block in content {
                    let btype = block.get("type")?.as_str()?;
                    match btype {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    on_event(StreamEvent::TextDelta(text.to_string()));
                                }
                            }
                        }
                        "tool_use" => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("Tool");
                            // Format the input as a short inline hint.
                            let input_hint = if let Some(input) = block.get("input") {
                                // For Bash, show the command. For others, first field.
                                if let Some(cmd) =
                                    input.get("command").and_then(Value::as_str)
                                {
                                    let preview: String = cmd.chars().take(80).collect();
                                    format!("`{preview}`")
                                } else if let Some(path) =
                                    input.get("path").and_then(Value::as_str)
                                {
                                    format!("`{path}`")
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };
                            let hint = if input_hint.is_empty() {
                                format!("\n⟳ {name}\n")
                            } else {
                                format!("\n⟳ {name}: {input_hint}\n")
                            };
                            on_event(StreamEvent::TextDelta(hint));
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                // Canonical final answer — may differ from streaming text
                // if the model summarised at the end.
                let result = v.get("result").and_then(Value::as_str).unwrap_or("");
                // Emit usage if present.
                if let Some(usage_val) = v.get("usage") {
                    let input = usage_val
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    let output = usage_val
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    on_event(StreamEvent::Usage(Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cache_read_tokens: usage_val
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        cache_write_tokens: usage_val
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                    }));
                }
                return Some(result.to_string());
            }
            _ => {}
        }
        None
    }
}

impl Default for ClaudeSubprocessClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::ChatProvider for ClaudeSubprocessClient {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        let mut full_text = String::new();
        self.chat_stream(request, &mut |event| {
            if let StreamEvent::TextDelta(t) = event {
                full_text.push_str(&t);
            }
        })
        .await
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        let prompt = Self::format_conversation(&request.messages);
        if prompt.is_empty() {
            return Err(ApiError::Decode(
                "no messages to send to claude subprocess".into(),
            ));
        }

        let mut child = Command::new("claude")
            .arg("-p")
            .arg(&prompt)
            .arg("--model")
            .arg(&request.model)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // hook noise goes to stderr, suppress it
            .spawn()
            .map_err(|e| ApiError::Status {
                status: 500,
                body: format!(
                    "claude subprocess failed to spawn: {e} — is `claude` on $PATH?"
                ),
            })?;

        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();

        let mut final_result: Option<String> = None;

        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| ApiError::Decode(format!("subprocess read error: {e}")))?
        {
            if line.is_empty() {
                continue;
            }
            if let Some(result) = Self::handle_line(&line, on_event) {
                final_result = Some(result);
            }
        }

        let exit = child
            .wait()
            .await
            .map_err(|e| ApiError::Decode(format!("subprocess wait error: {e}")))?;

        if !exit.success() && final_result.is_none() {
            let code = exit.code().unwrap_or(-1);
            return Err(ApiError::Status {
                status: 500,
                body: format!("claude exited with code {code}"),
            });
        }

        let text = final_result.unwrap_or_default();

        Ok(ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: Role::Assistant,
                    content: Some(text),
                    content_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                    protected: false,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_empty() {
        let out = ClaudeSubprocessClient::format_conversation(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn format_single_user() {
        let out = ClaudeSubprocessClient::format_conversation(&[ChatMessage::user("hello")]);
        assert_eq!(out, "Human: hello");
    }

    #[test]
    fn format_system_and_user() {
        let msgs = vec![
            ChatMessage::system("be helpful"),
            ChatMessage::user("what is 2+2?"),
        ];
        let out = ClaudeSubprocessClient::format_conversation(&msgs);
        assert!(out.contains("[System context]: be helpful"));
        assert!(out.contains("Human: what is 2+2?"));
    }

    #[test]
    fn handle_text_line() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        let mut got = String::new();
        ClaudeSubprocessClient::handle_line(line, &mut |ev| {
            if let StreamEvent::TextDelta(t) = ev {
                got.push_str(&t);
            }
        });
        assert_eq!(got, "hello");
    }

    #[test]
    fn handle_tool_use_line() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls /"}}]}}"#;
        let mut got = String::new();
        ClaudeSubprocessClient::handle_line(line, &mut |ev| {
            if let StreamEvent::TextDelta(t) = ev {
                got.push_str(&t);
            }
        });
        assert!(got.contains("Bash"), "expected tool name: {got}");
        assert!(got.contains("ls /"), "expected command: {got}");
    }

    #[test]
    fn handle_result_line() {
        let line = r#"{"type":"result","subtype":"success","result":"done"}"#;
        let result = ClaudeSubprocessClient::handle_line(line, &mut |_| {});
        assert_eq!(result, Some("done".to_string()));
    }
}
