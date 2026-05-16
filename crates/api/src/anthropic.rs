//! Anthropic Messages adapter.
//!
//! The agent loop only ever sees an OpenAI-shaped [`ChatRequest`] /
//! [`ChatResponse`]. To talk to Anthropic without forking the loop, this
//! client translates in both directions:
//!
//!   * outgoing: OpenAI `ChatRequest` -> Anthropic `MessagesRequest`
//!   * incoming: Anthropic `MessagesResponse` -> OpenAI `ChatResponse`
//!
//! The translation is mechanical and lossy in only one direction —
//! Anthropic's content blocks are richer than the flat OpenAI shape, so
//! non-text blocks (image, document) get an `[anthropic <kind> block]`
//! placeholder rather than being silently dropped, and the model can see
//! something happened.

use async_trait::async_trait;

use crate::{
    consume_sse_async, ApiError, ApiResult, ChatChoice, ChatMessage, ChatProvider, ChatRequest,
    ChatResponse, ContentBlock, Role, StreamEvent, ToolCall, ToolCallFunction, Usage,
};

/// Async HTTP client for Anthropic's `/v1/messages` endpoint.
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AnthropicClient {
    /// Default Anthropic API endpoint.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.anthropic.com";

    /// Wire version pinned in the `anthropic-version` header. We track a
    /// stable, well-supported revision rather than chasing the bleeding
    /// edge so a Messages API beta change can't silently break us.
    pub const ANTHROPIC_VERSION: &'static str = "2023-06-01";

    /// `max_tokens` is required by the Messages API but optional in the
    /// shared `ChatRequest`. When the caller leaves it unset we use this
    /// fallback so the request still validates server-side.
    pub const DEFAULT_MAX_TOKENS: u32 = 4096;

    /// Constructs a client with an explicit base URL and key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("metis/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(10))
            .http1_only()
            .build()?;
        Ok(Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            http,
        })
    }

    /// Translates an OpenAI-shaped [`ChatRequest`] into the Anthropic
    /// Messages JSON body. Public so tests can pin the wire shape.
    pub fn build_messages_body(request: &ChatRequest) -> serde_json::Value {
        // Pull every system message out of the conversation — Anthropic
        // wants them concatenated into a single top-level `system` field
        // rather than carried inline. Multiple system turns are joined
        // with double newlines so they read as paragraphs.
        let mut system_chunks: Vec<&str> = Vec::new();
        let mut wire_messages: Vec<serde_json::Value> = Vec::new();

        for message in &request.messages {
            match message.role {
                Role::System => {
                    if let Some(text) = message.content.as_deref() {
                        system_chunks.push(text);
                    }
                }
                Role::User => {
                    if message.content_blocks.is_empty() {
                        wire_messages.push(serde_json::json!({
                            "role": "user",
                            "content": message.content.clone().unwrap_or_default(),
                        }));
                    } else {
                        let blocks: Vec<serde_json::Value> = message
                            .content_blocks
                            .iter()
                            .map(|b| match b {
                                ContentBlock::Text { text } => {
                                    serde_json::json!({"type": "text", "text": text})
                                }
                                ContentBlock::Image { media_type, data } => {
                                    serde_json::json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": media_type,
                                            "data": data
                                        }
                                    })
                                }
                                ContentBlock::Document { media_type, data } => {
                                    serde_json::json!({
                                        "type": "document",
                                        "source": {
                                            "type": "base64",
                                            "media_type": media_type,
                                            "data": data
                                        }
                                    })
                                }
                            })
                            .collect();
                        wire_messages.push(serde_json::json!({
                            "role": "user",
                            "content": blocks,
                        }));
                    }
                }
                Role::Assistant => {
                    // Assistant turns may carry text, tool calls, or
                    // both. Anthropic wants them as a content-block
                    // array with `text` and `tool_use` entries.
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if let Some(text) = message.content.as_deref() {
                        if !text.is_empty() {
                            blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                    }
                    for call in &message.tool_calls {
                        // Anthropic's `input` field is a JSON object,
                        // not a string. The OpenAI shape stores it as a
                        // JSON-encoded string, so we re-parse here and
                        // fall back to an empty object if the model
                        // produced something unparseable.
                        let input: serde_json::Value =
                            serde_json::from_str(&call.function.arguments)
                                .unwrap_or_else(|_| serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                    if blocks.is_empty() {
                        // Anthropic rejects empty assistant turns. Plant
                        // a single empty text block so the conversation
                        // structure stays valid.
                        blocks.push(serde_json::json!({"type": "text", "text": ""}));
                    }
                    wire_messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
                Role::Tool => {
                    // OpenAI tool messages become a single `tool_result`
                    // block inside a user-role turn — Anthropic models
                    // tool results as user content, not as a separate
                    // role.
                    let tool_content = if message.content_blocks.is_empty() {
                        serde_json::json!(message.content.clone().unwrap_or_default())
                    } else {
                        let blocks: Vec<serde_json::Value> = message
                            .content_blocks
                            .iter()
                            .map(|b| match b {
                                ContentBlock::Text { text } => {
                                    serde_json::json!({"type": "text", "text": text})
                                }
                                ContentBlock::Image { media_type, data } => {
                                    serde_json::json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": media_type,
                                            "data": data
                                        }
                                    })
                                }
                                ContentBlock::Document { media_type, data } => {
                                    serde_json::json!({
                                        "type": "document",
                                        "source": {
                                            "type": "base64",
                                            "media_type": media_type,
                                            "data": data
                                        }
                                    })
                                }
                            })
                            .collect();
                        serde_json::json!(blocks)
                    };
                    let block = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                        "content": tool_content,
                    });
                    wire_messages.push(serde_json::json!({
                        "role": "user",
                        "content": [block],
                    }));
                }
            }
        }

        // Tool specs: OpenAI wraps each tool in a `{type, function}`
        // envelope; Anthropic wants the function fields flattened with
        // `input_schema` instead of `parameters`. The last tool in the
        // array carries a `cache_control` ephemeral marker so the
        // whole tool block becomes a prompt-cache breakpoint — every
        // subsequent turn with an identical tool set then reads from
        // cache instead of re-paying the full tool-spec cost.
        let tools = request.tools.as_ref().map(|specs| {
            let last = specs.len().saturating_sub(1);
            specs
                .iter()
                .enumerate()
                .map(|(i, spec)| {
                    let mut tool = serde_json::json!({
                        "name": spec.function.name,
                        "description": spec.function.description,
                        "input_schema": spec.function.parameters,
                    });
                    if i == last {
                        tool["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                    }
                    tool
                })
                .collect::<Vec<_>>()
        });

        let mut body = serde_json::json!({
            "model": request.model,
            "max_tokens": request.max_tokens.unwrap_or(Self::DEFAULT_MAX_TOKENS),
            "messages": wire_messages,
        });
        if !system_chunks.is_empty() {
            // Emit the system prompt as a content-block array with a
            // single `text` block so we can attach a `cache_control`
            // ephemeral marker. Anthropic treats the marker as a
            // breakpoint: every token up to and including it becomes a
            // prompt-cache boundary, so the system preamble is cached
            // across turns at a ~90% discount after the first write.
            let system_text = system_chunks.join("\n\n");
            body["system"] = serde_json::json!([
                {
                    "type": "text",
                    "text": system_text,
                    "cache_control": { "type": "ephemeral" }
                }
            ]);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools);
            }
        }
        // Extended thinking: Anthropic expects a top-level `thinking`
        // object with `type: "enabled"` and a `budget_tokens` cap.
        if request.thinking {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": request.thinking_budget,
            });
            // Anthropic requires temperature to be unset (or 1) when
            // thinking is enabled — remove it to avoid a 400.
            if let serde_json::Value::Object(ref mut map) = body {
                map.remove("temperature");
            }
        }
        if std::env::var_os("METIS_DEBUG_BODY").is_some() {
            let path = std::env::var("METIS_DEBUG_BODY_PATH")
                .unwrap_or_else(|_| "/tmp/aegis_last_body.json".to_string());
            let _ = std::fs::write(&path, serde_json::to_string(&body).unwrap_or_default());
            eprintln!(
                "===METIS_ANTHROPIC_BODY_WRITTEN: {} ({} bytes)===",
                path,
                serde_json::to_string(&body).unwrap_or_default().len()
            );
        }
        body
    }

    /// Translates an Anthropic `/v1/messages` response into the
    /// OpenAI-shaped [`ChatResponse`] the agent loop expects. Public so
    /// tests can drive it from a captured fixture without HTTP.
    pub fn parse_messages_response(value: &serde_json::Value) -> ApiResult<ChatResponse> {
        let content = value
            .get("content")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ApiError::Decode("messages response missing `content` array".into()))?;

        let mut text_chunks: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for block in content {
            let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        text_chunks.push(text.to_string());
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let arguments = serde_json::to_string(&input).map_err(|err| {
                        ApiError::Decode(format!("could not encode tool input: {err}"))
                    })?;
                    tool_calls.push(ToolCall {
                        id,
                        kind: "function".to_string(),
                        function: ToolCallFunction { name, arguments },
                    });
                }
                "thinking" => {
                    // Extended thinking blocks are informational — the
                    // model's chain-of-thought. We prefix them so the
                    // caller can detect them, but they don't affect the
                    // final answer text. In streaming mode these are
                    // emitted as ThinkingDelta events instead.
                }
                other => {
                    text_chunks.push(format!("[anthropic {other} block]"));
                }
            }
        }

        let content_text = if text_chunks.is_empty() {
            None
        } else {
            Some(text_chunks.join("\n"))
        };

        let stop_reason = value
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Map Anthropic stop reasons to OpenAI finish_reason vocabulary
        // so existing log lines and tests don't have to know the
        // difference.
        let finish_reason = stop_reason.map(|s| match s.as_str() {
            "end_turn" | "stop_sequence" => "stop".to_string(),
            "tool_use" => "tool_calls".to_string(),
            "max_tokens" => "length".to_string(),
            other => other.to_string(),
        });

        // Use the normalised [`Usage`] deserializer so both
        // `cache_read_input_tokens` and `cache_creation_input_tokens`
        // (when prompt caching is in effect) flow through without a
        // second manual parse.
        let usage = value
            .get("usage")
            .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok());

        Ok(ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: Role::Assistant,
                    content: content_text,
                    content_blocks: Vec::new(),
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                    protected: false,
                    reasoning_content: None,
                },
                finish_reason,
            }],
            usage,
        })
    }
}

/// In-progress accumulator for Anthropic streamed `/v1/messages`
/// responses. Anthropic pushes a sequence of typed SSE events —
/// `message_start`, `content_block_start`, `content_block_delta`,
/// `content_block_stop`, `message_delta`, `message_stop` — rather than
/// a flat list of text chunks. We rebuild the final message by tracking
/// one slot per content block and folding deltas into it.
#[derive(Default)]
pub(crate) struct AnthropicStreamAccumulator {
    blocks: Vec<AnthropicBlockInProgress>,
    stop_reason: Option<String>,
    input_tokens: u32,
    output_tokens: u32,
    /// `cache_creation_input_tokens` from `message_start.usage`. Tracked
    /// so the synthesised final [`Usage`] event reports cache writes
    /// when prompt caching is in effect.
    cache_write_tokens: u32,
    /// `cache_read_input_tokens` from `message_start.usage`. Anthropic
    /// reports cache hits as a flat sibling of `input_tokens`, not
    /// folded into it, so we have to track it separately.
    cache_read_tokens: u32,
}

pub(crate) enum AnthropicBlockInProgress {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

impl AnthropicStreamAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one SSE `data:` payload. Returns `Ok(true)` to keep reading,
    /// `Ok(false)` once `message_stop` has been seen.
    pub(crate) fn feed(
        &mut self,
        payload: &str,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> ApiResult<bool> {
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| ApiError::Decode(format!("anthropic sse json: {e}")))?;
        let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "message_start" => {
                // Lift the full usage object through the normalised
                // [`Usage`] deserializer so cache_creation and
                // cache_read land in the right slots — Anthropic emits
                // all input-side counters in this single event.
                if let Some(u) = value.pointer("/message/usage") {
                    if let Ok(parsed) = serde_json::from_value::<Usage>(u.clone()) {
                        self.input_tokens = parsed.prompt_tokens;
                        self.cache_read_tokens = parsed.cache_read_tokens;
                        self.cache_write_tokens = parsed.cache_write_tokens;
                    }
                }
            }
            "content_block_start" => {
                let idx = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let block = value.get("content_block").cloned().unwrap_or_default();
                let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                while self.blocks.len() <= idx {
                    self.blocks
                        .push(AnthropicBlockInProgress::Text(String::new()));
                }
                self.blocks[idx] = match kind {
                    "tool_use" => AnthropicBlockInProgress::ToolUse {
                        id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_json: String::new(),
                    },
                    "thinking" => AnthropicBlockInProgress::Thinking(String::new()),
                    _ => AnthropicBlockInProgress::Text(String::new()),
                };
            }
            "content_block_delta" => {
                let idx = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let delta = value.get("delta").cloned().unwrap_or_default();
                let dtype = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(block) = self.blocks.get_mut(idx) {
                    match (dtype, block) {
                        ("text_delta", AnthropicBlockInProgress::Text(s)) => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    s.push_str(t);
                                    on_event(StreamEvent::TextDelta(t.to_string()));
                                }
                            }
                        }
                        ("thinking_delta", AnthropicBlockInProgress::Thinking(s)) => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    s.push_str(t);
                                    on_event(StreamEvent::ThinkingDelta(t.to_string()));
                                }
                            }
                        }
                        (
                            "input_json_delta",
                            AnthropicBlockInProgress::ToolUse { input_json, .. },
                        ) => {
                            if let Some(t) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                input_json.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = value.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(ot) = value
                    .pointer("/usage/output_tokens")
                    .and_then(|v| v.as_u64())
                {
                    self.output_tokens = ot as u32;
                }
            }
            "message_stop" => {
                let usage = self.snapshot_usage();
                on_event(StreamEvent::Usage(usage));
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    }

    /// Finalise into the OpenAI-shaped [`ChatResponse`] the agent loop
    /// expects. Mirrors [`AnthropicClient::parse_messages_response`]'s
    /// finish-reason vocabulary mapping.
    pub(crate) fn into_response(mut self) -> ChatResponse {
        let mut text_chunks: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let blocks = std::mem::take(&mut self.blocks);
        for block in blocks {
            match block {
                AnthropicBlockInProgress::Text(s) => {
                    if !s.is_empty() {
                        text_chunks.push(s);
                    }
                }
                AnthropicBlockInProgress::Thinking(_) => {
                    // Thinking content is streamed via ThinkingDelta
                    // events but not included in the final response text.
                }
                AnthropicBlockInProgress::ToolUse {
                    id,
                    name,
                    input_json,
                } => {
                    let arguments = if input_json.is_empty() {
                        "{}".to_string()
                    } else {
                        input_json
                    };
                    tool_calls.push(ToolCall {
                        id,
                        kind: "function".to_string(),
                        function: ToolCallFunction { name, arguments },
                    });
                }
            }
        }
        let content = if text_chunks.is_empty() {
            None
        } else {
            Some(text_chunks.join("\n"))
        };
        let finish_reason = self.stop_reason.as_deref().map(|s| match s {
            "end_turn" | "stop_sequence" => "stop".to_string(),
            "tool_use" => "tool_calls".to_string(),
            "max_tokens" => "length".to_string(),
            other => other.to_string(),
        });
        let usage = self.snapshot_usage();
        ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: Role::Assistant,
                    content,
                    content_blocks: Vec::new(),
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                    protected: false,
                    reasoning_content: None,
                },
                finish_reason,
            }],
            usage: Some(usage),
        }
    }

    /// Build a normalised [`Usage`] snapshot from the accumulator's
    /// per-field counters. Factored out so `message_stop` and
    /// `into_response` cannot drift — both paths must report the same
    /// totals, including cache reads and cache writes.
    fn snapshot_usage(&self) -> Usage {
        let total = self
            .input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens);
        Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: total,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicClient {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = Self::build_messages_body(request);
        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", Self::ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| ApiError::Decode(err.to_string()))?;
        Self::parse_messages_response(&value)
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut body = Self::build_messages_body(request);
        body["stream"] = serde_json::json!(true);
        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", Self::ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let mut acc = AnthropicStreamAccumulator::new();
        consume_sse_async(response, |payload| acc.feed(payload, on_event)).await?;
        Ok(acc.into_response())
    }
}
