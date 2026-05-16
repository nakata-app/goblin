//! OpenAI-compatible chat completions client.
//!
//! Speaks the OpenAI wire format against any base URL that honours it —
//! DeepSeek, OpenAI, xAI Grok, OpenRouter, Together, and the Gemini
//! OpenAI-compat endpoint. Two provider-specific adaptations live here
//! because they can't be modelled generically:
//!
//!   * **Gemini message sanitisation** — Google's OpenAI-compat layer
//!     rejects conversations whose turn ordering violates its internal
//!     rules. We rewrite the message array before sending.
//!   * **Claude-via-OpenAI-compat prompt caching** — OpenRouter and
//!     similar gateways carry Claude models, and we inject Anthropic
//!     `cache_control` markers so the ~90% prompt-cache discount still
//!     kicks in end-to-end.
//!
//! The streaming accumulator plus the streaming repeat detector also
//! live here; they're intimately tied to the chunk shape this client
//! speaks and have no meaning outside it.

use async_trait::async_trait;

use crate::{
    consume_sse_async, ApiError, ApiResult, ChatChoice, ChatMessage, ChatProvider, ChatRequest,
    ChatResponse, Role, StreamEvent, ToolCall, ToolCallFunction, Usage,
};

/// Async HTTP client for any OpenAI-compatible chat completions
/// endpoint. The base URL is configurable so the same client serves
/// DeepSeek, OpenAI, xAI Grok, OpenRouter, Together, and friends.
pub struct OpenAICompatClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

/// Backwards-compatible alias for the v0.1/v0.2 name. The client was
/// always generic over base URL — only the type name was DeepSeek-flavoured.
pub type DeepSeekClient = OpenAICompatClient;

// Manual `Debug` so the API key never reaches logs or panic messages.
impl std::fmt::Debug for OpenAICompatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenAICompatClient {
    /// Default DeepSeek API endpoint, kept as an associated constant for
    /// backwards compatibility with the v0.1 `DeepSeekClient` API.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.deepseek.com";

    /// Reads `DEEPSEEK_API_KEY` from the process environment and returns
    /// a client targeting the DeepSeek endpoint. Kept for compatibility;
    /// new code should prefer [`crate::Provider::client_from_env`].
    pub fn from_env() -> ApiResult<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| ApiError::MissingKey("DEEPSEEK_API_KEY"))?;
        Self::new(Self::DEFAULT_BASE_URL, api_key)
    }

    /// Constructs a client with an explicit base URL and key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("metis/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            http,
        })
    }

    /// Sends a chat completion request and returns the parsed response.
    pub async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        <Self as ChatProvider>::chat(self, request).await
    }
}

impl OpenAICompatClient {
    /// Serialises `request` into the JSON body used by the streaming
    /// endpoint: same shape as the non-stream body plus `stream: true`
    /// and `stream_options.include_usage: true` so providers that gate
    /// usage behind an opt-in (OpenAI, Together) still emit it in the
    /// final chunk. Public so tests can pin the wire shape without
    /// running HTTP.
    pub fn build_stream_body(request: &ChatRequest) -> serde_json::Value {
        let mut body = serde_json::to_value(request).expect("ChatRequest serializes");
        // Flatten content_blocks before anything else touches the body,
        // so inject_cache_control & friends see the final content shape.
        Self::rewrite_multimodal_content(&mut body);
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        if Self::is_claude_model(&request.model) {
            Self::inject_cache_control(&mut body);
        }
        if std::env::var_os("METIS_DEBUG_BODY").is_some() {
            let path = std::env::var("METIS_DEBUG_BODY_PATH")
                .unwrap_or_else(|_| "/tmp/aegis_last_body.json".to_string());
            let _ = std::fs::write(&path, serde_json::to_string(&body).unwrap_or_default());
            eprintln!(
                "===METIS_DEBUG_BODY_WRITTEN: {} ({} bytes)===",
                path,
                serde_json::to_string(&body).unwrap_or_default().len()
            );
        }
        body
    }

    /// Returns true when this client targets the Gemini OpenAI-compat
    /// endpoint, which enforces stricter message ordering rules.
    fn is_gemini(&self) -> bool {
        self.base_url.contains("generativelanguage.googleapis.com")
    }

    /// Returns true when the base_url points at z.ai. Their
    /// OpenAI-compat endpoint lives at `/chat/completions` without
    /// the `/v1/` segment every other OpenAI-compat provider
    /// expects, so the URL template branches on this.
    fn is_zai(&self) -> bool {
        self.base_url.contains("api.z.ai")
    }

    fn is_minimax(&self) -> bool {
        self.base_url.contains("api.minimax.io")
    }

    fn is_nvidia(&self) -> bool {
        self.base_url.contains("integrate.api.nvidia.com")
    }

    /// URL for `/chat/completions` under this client's base. Every
    /// OpenAI-compat provider except z.ai expects the `/v1/` segment.
    fn chat_completions_url(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        if self.is_zai() {
            format!("{trimmed}/chat/completions")
        } else {
            format!("{trimmed}/v1/chat/completions")
        }
    }

    /// Returns true when the model is an Anthropic Claude model accessed
    /// via an OpenAI-compat endpoint (e.g. OpenRouter). These support
    /// Anthropic-style prompt caching via cache_control markers.
    fn is_claude_model(model: &str) -> bool {
        let lower = model.to_ascii_lowercase();
        lower.contains("claude") || lower.starts_with("anthropic/")
    }

    /// Translate the raw `content_blocks` array produced by
    /// `ChatMessage::user_multimodal` / `tool_result_multimodal` into the
    /// OpenAI-compat wire format — i.e. a `content` array whose entries
    /// are `{type:"text",text:…}` or `{type:"image_url",image_url:{url:
    /// "data:<mime>;base64,<data>"}}`. Drops the `content_blocks` key
    /// afterwards so the resulting body is exactly what OpenAI, DeepSeek,
    /// OpenRouter, etc. expect.
    ///
    /// Without this translation, the default `Serialize` impl of
    /// `ChatMessage` emits `content: null` + `content_blocks: [...]`,
    /// which providers reject with a cryptic `missing field 'content'`
    /// 400 — the exact error users hit when attaching an image.
    ///
    /// `Document` blocks are left as-is for now: OpenAI's
    /// `/chat/completions` doesn't have a widely-portable PDF format,
    /// and the multimodal path is only exercised by user-attached
    /// images today. Tool-result messages with `content_blocks` are
    /// flattened to a placeholder string so the request stays valid.
    pub(crate) fn rewrite_multimodal_content(body: &mut serde_json::Value) {
        let Some(messages) = body["messages"].as_array_mut() else {
            return;
        };
        for msg in messages.iter_mut() {
            let blocks = match msg.get_mut("content_blocks") {
                Some(v) if v.is_array() => v.take(),
                _ => continue,
            };
            let blocks_arr = blocks.as_array().cloned().unwrap_or_default();
            if blocks_arr.is_empty() {
                // Empty content_blocks + null content — treat as empty
                // string so the request stays wire-valid.
                msg["content"] = serde_json::Value::String(String::new());
                if let Some(obj) = msg.as_object_mut() {
                    obj.remove("content_blocks");
                }
                continue;
            }
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            // Tool messages must keep `content` as a plain string on
            // OpenAI-compat. Collapse any text blocks; drop images
            // entirely (they'd be rejected anyway, and a tool that
            // wants to ship an image to a vision-capable model should
            // use the user-message path).
            if role == "tool" {
                let text: String = blocks_arr
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str()).map(str::to_string)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                msg["content"] = serde_json::Value::String(text);
                if let Some(obj) = msg.as_object_mut() {
                    obj.remove("content_blocks");
                }
                continue;
            }
            // User / assistant / system: build the OpenAI multimodal
            // `content` array.
            let mut content_arr: Vec<serde_json::Value> = Vec::with_capacity(blocks_arr.len());
            for b in &blocks_arr {
                let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "text" => {
                        let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        content_arr.push(serde_json::json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                    "image" => {
                        let media = b
                            .get("media_type")
                            .and_then(|m| m.as_str())
                            .unwrap_or("image/png");
                        let data = b.get("data").and_then(|d| d.as_str()).unwrap_or("");
                        let url = format!("data:{media};base64,{data}");
                        content_arr.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": url },
                        }));
                    }
                    _ => {
                        // Unknown block type — skip rather than blowing
                        // up the whole request. A future Document path
                        // would plug in here.
                    }
                }
            }
            msg["content"] = serde_json::Value::Array(content_arr);
            if let Some(obj) = msg.as_object_mut() {
                obj.remove("content_blocks");
            }
        }
    }

    /// True for providers known to not support vision at all, so we can
    /// fail fast with a clear error instead of letting the request go
    /// out and come back as a cryptic 400.
    ///
    /// Kept narrow on purpose: only families we can confirm reject
    /// multimodal user messages. Unknown models fall through and are
    /// tried against the provider; if the provider rejects, the user
    /// at least gets the provider's own error rather than a made-up one.
    /// Inject `tool_choice:"auto"` and `parallel_tool_calls:true` into a
    /// request body when tools are present and the provider supports the
    /// OpenAI tool_choice field. Gemini, MiniMax, and NVIDIA NIM either
    /// reject the field or use a different schema, so they must be excluded
    /// by the caller.
    pub(crate) fn inject_tool_choice(body: &mut serde_json::Value) {
        let has_tools = body
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_tools {
            body["tool_choice"] = serde_json::json!("auto");
            body["parallel_tool_calls"] = serde_json::json!(true);
        }
    }

    pub(crate) fn model_is_text_only(model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        // DeepSeek-chat (V3), deepseek-reasoner, deepseek-coder: all
        // text-only on the public /chat/completions endpoint. DeepSeek
        // has a VL model line but it isn't served here.
        if m.starts_with("deepseek-") || m.contains("/deepseek-") {
            return true;
        }
        false
    }

    /// Scan the request for user/tool messages carrying image blocks.
    /// Used by the text-only-model short-circuit so we can return a
    /// readable error before hitting the wire.
    pub(crate) fn request_has_images(request: &ChatRequest) -> bool {
        request.messages.iter().any(|m| {
            m.content_blocks
                .iter()
                .any(|b| matches!(b, crate::ContentBlock::Image { .. }))
        })
    }

    /// True iff the *last* user message carries an image block. The
    /// fail-fast in `chat`/`chat_stream` pivots on this rather than
    /// `request_has_images`: an image left over in history from an
    /// earlier turn must not lock the conversation — only a freshly
    /// attached image on the current turn should refuse the request.
    pub(crate) fn last_user_has_images(request: &ChatRequest) -> bool {
        request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::types::Role::User))
            .map(|m| {
                m.content_blocks
                    .iter()
                    .any(|b| matches!(b, crate::ContentBlock::Image { .. }))
            })
            .unwrap_or(false)
    }

    /// Strip image blocks from every message *except* the last user
    /// message. Replaces each dropped image with a short text marker so
    /// the model still sees that an attachment existed at that turn.
    /// Without this, a single image earlier in the conversation kept
    /// re-triggering the text-only-model fail-fast on every subsequent
    /// turn — the user types text, the predicate still sees the old
    /// image in history, and the request 400s with no way out.
    pub(crate) fn strip_image_blocks_from_history(
        messages: &[crate::ChatMessage],
    ) -> Vec<crate::ChatMessage> {
        let last_user_idx = messages
            .iter()
            .rposition(|m| matches!(m.role, crate::types::Role::User));
        messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if Some(i) == last_user_idx {
                    return m.clone();
                }
                if !m
                    .content_blocks
                    .iter()
                    .any(|b| matches!(b, crate::ContentBlock::Image { .. }))
                {
                    return m.clone();
                }
                let mut cleaned = m.clone();
                cleaned.content_blocks = m
                    .content_blocks
                    .iter()
                    .map(|b| match b {
                        crate::ContentBlock::Image { .. } => crate::ContentBlock::Text {
                            text: "[image dropped: routed to a text-only model]".to_string(),
                        },
                        other => other.clone(),
                    })
                    .collect();
                cleaned
            })
            .collect()
    }

    /// Injects `cache_control: {type: "ephemeral"}` markers into the
    /// serialised request body for Claude models on OpenAI-compat endpoints.
    /// This enables Anthropic's prompt cache: ~90% discount on repeated
    /// system prompt + tool spec tokens after the first turn.
    ///
    /// Mutates in-place so we don't clone the full body.
    fn inject_cache_control(body: &mut serde_json::Value) {
        // Cache the system message content block.
        if let Some(messages) = body["messages"].as_array_mut() {
            for msg in messages.iter_mut() {
                if msg["role"].as_str() == Some("system") {
                    if let Some(text) = msg["content"].as_str().map(|s| s.to_string()) {
                        msg["content"] = serde_json::json!([{
                            "type": "text",
                            "text": text,
                            "cache_control": {"type": "ephemeral"}
                        }]);
                    }
                    break;
                }
            }
        }
        // Cache the last tool spec — tools list rarely changes between turns.
        if let Some(tools) = body["tools"].as_array_mut() {
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
        }
    }

    /// Remove assistant turns whose tool_calls are not fully answered.
    ///
    /// If a session was interrupted after the assistant emitted tool_calls
    /// but before all tool results were written, the history will have an
    /// orphaned assistant message. Every OpenAI-compat provider (not just
    /// Gemini) rejects this with a 400. We drop the entire incomplete turn
    /// (the assistant message + any partial tool results that follow it).
    pub fn sanitize_tool_calls(messages: &[ChatMessage]) -> Vec<ChatMessage> {
        let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
        // Track tool_call ids introduced by the most recent assistant turn
        // with tool_calls, so tool responses can be validated against them.
        let mut active_call_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut i = 0;
        while i < messages.len() {
            let m = &messages[i];
            if m.role == Role::Tool {
                // Orphaned tool result — the preceding message either wasn't
                // an assistant turn with tool_calls or was already dropped.
                // Drop this message to avoid a provider 400. A tool reply
                // is only valid when the IMMEDIATE previous message is
                // either another Tool (same group) or an Assistant with
                // tool_calls — non-tool interleavers (user/system) break
                // the chain and a later tool can't heal it by pointing
                // back at an earlier group. Additionally, the tool_call_id
                // must reference a call announced by that assistant turn.
                let immediate_valid = out
                    .last()
                    .map(|p| {
                        p.role == Role::Tool
                            || (p.role == Role::Assistant && !p.tool_calls.is_empty())
                    })
                    .unwrap_or(false);
                let id_valid = match &m.tool_call_id {
                    Some(id) => active_call_ids.contains(id),
                    None => false,
                };
                if immediate_valid && id_valid {
                    out.push(m.clone());
                }
                i += 1;
            } else if m.role == Role::Assistant && !m.tool_calls.is_empty() {
                let required: std::collections::HashSet<&str> =
                    m.tool_calls.iter().map(|tc| tc.id.as_str()).collect();
                // Collect following tool result messages
                let mut j = i + 1;
                let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
                while j < messages.len() && messages[j].role == Role::Tool {
                    if let Some(ref id) = messages[j].tool_call_id {
                        covered.insert(id.as_str());
                    }
                    j += 1;
                }
                if covered == required {
                    // Complete turn — keep it, and remember these call ids
                    // so subsequent tool messages can be validated.
                    active_call_ids = required.iter().map(|s| s.to_string()).collect();
                    out.extend_from_slice(&messages[i..j]);
                } else {
                    // Incomplete turn — silently drop assistant + partial
                    // results and clear the active window so later orphan
                    // tools can't mis-match.
                    active_call_ids.clear();
                }
                i = j;
            } else {
                // New turn from user/system/plain-assistant invalidates
                // any previously-active tool_call ids.
                active_call_ids.clear();
                // Drop assistant messages with no content AND no tool_calls.
                // Providers reject these with a 400
                // ("Invalid assistant message: content or tool_calls must be set").
                // Happens when a reasoning model produces only thinking/
                // reasoning_content with no visible text and no function calls,
                // and the empty ghost assistant is saved to disk then echoed
                // back on the next turn. Other roles pass through untouched.
                if m.role != Role::Assistant
                    || m.content.is_some()
                    || !m.content_blocks.is_empty()
                    || !m.tool_calls.is_empty()
                {
                    out.push(m.clone());
                }
                i += 1;
            }
        }
        out
    }

    /// Sanitise the message array for Gemini's OpenAI-compat endpoint.
    ///
    /// Gemini rejects a request when a `function_call` turn (assistant
    /// with `tool_calls`) does not immediately follow a `user` or
    /// `function_response` (tool) turn. The agent loop injects system
    /// messages mid-conversation (compaction summaries, hook output)
    /// which violate this constraint. This method rewrites the array:
    ///
    /// 1. The first system message is kept (it's the system prompt).
    /// 2. Subsequent system messages are converted to user role.
    /// 3. Any user-role message sandwiched between an
    ///    assistant(tool_calls) turn and its tool results is moved
    ///    before the assistant turn so the function_call → tool_result
    ///    chain stays unbroken.
    pub(crate) fn sanitize_for_gemini(messages: &[ChatMessage]) -> Vec<ChatMessage> {
        // Step 1: convert ALL system messages to user. Gemini's
        // OpenAI-compat layer sometimes mishandles the system role,
        // especially when tools are defined. Converting to user with
        // a [system] prefix is universally safe.
        let mut msgs: Vec<ChatMessage> = Vec::with_capacity(messages.len());
        for m in messages {
            if m.role == Role::System {
                let mut converted = m.clone();
                converted.role = Role::User;
                // Prefix so the model knows this was a system instruction.
                if let Some(ref content) = converted.content {
                    converted.content = Some(format!("[system] {content}"));
                }
                msgs.push(converted);
            } else {
                msgs.push(m.clone());
            }
        }

        // Step 2: ensure nothing sits between assistant(tool_calls)
        // and its tool results. Collect indices of offending messages
        // and relocate them.
        let mut result: Vec<ChatMessage> = Vec::with_capacity(msgs.len());
        let mut i = 0;
        while i < msgs.len() {
            if msgs[i].role == Role::Assistant && !msgs[i].tool_calls.is_empty() {
                // Scan forward for tool results; any non-tool message
                // before the tool results must be moved before us.
                let mut deferred: Vec<ChatMessage> = Vec::new();
                let mut tool_group: Vec<ChatMessage> = Vec::new();
                tool_group.push(msgs[i].clone()); // the assistant turn
                let mut j = i + 1;
                while j < msgs.len() && msgs[j].role != Role::Assistant {
                    if msgs[j].role == Role::Tool {
                        tool_group.push(msgs[j].clone());
                    } else {
                        // User/system message between function_call and
                        // function_response — move before the group.
                        deferred.push(msgs[j].clone());
                    }
                    j += 1;
                }
                // Emit deferred messages first, then the intact group.
                result.extend(deferred);
                result.extend(tool_group);
                i = j;
            } else {
                result.push(msgs[i].clone());
                i += 1;
            }
        }

        // Step 3: merge consecutive user messages. Gemini maps
        // user turns to its native "user" role and may reject back-to-back
        // user turns that the reordering above can create.
        let mut merged: Vec<ChatMessage> = Vec::with_capacity(result.len());
        for m in result {
            if m.role == Role::User {
                if let Some(last) = merged.last_mut() {
                    if last.role == Role::User {
                        // Merge content into the previous user message.
                        let prev = last.content.get_or_insert_with(String::new);
                        if let Some(ref new) = m.content {
                            prev.push('\n');
                            prev.push_str(new);
                        }
                        continue;
                    }
                }
            }
            merged.push(m);
        }

        // Step 4: final Gemini-specific fixes.
        // a) Ensure conversation starts with a user turn (not assistant).
        //    Gemini rejects conversations that start with a model turn.
        // b) Remove consecutive assistant messages — merge or keep last.
        // c) Ensure no assistant(tool_calls) follows another assistant.
        let mut clean: Vec<ChatMessage> = Vec::with_capacity(merged.len());
        for m in merged {
            if m.role == Role::Assistant {
                // If previous message is also assistant, merge text.
                if let Some(last) = clean.last_mut() {
                    if last.role == Role::Assistant
                        && last.tool_calls.is_empty()
                        && m.tool_calls.is_empty()
                    {
                        // Two consecutive text-only assistant messages — merge.
                        let prev = last.content.get_or_insert_with(String::new);
                        if let Some(ref new) = m.content {
                            prev.push('\n');
                            prev.push_str(new);
                        }
                        continue;
                    }
                }
            }
            clean.push(m);
        }

        // Ensure first message is user role.
        if clean.first().map(|m| m.role != Role::User).unwrap_or(false) {
            // Already handled by system→user conversion, but guard anyway.
        }

        clean
    }

    /// Sanitise messages for MiniMax and NVIDIA NIM.
    ///
    /// These providers share Gemini's constraint that no message may sit
    /// between an assistant(tool_calls) turn and its tool results, and
    /// they also reject consecutive system messages (which the compaction
    /// layer can produce). Unlike Gemini they do not require system messages
    /// to be converted to user role, so we skip that coercion.
    pub(crate) fn sanitize_for_minimax(messages: &[ChatMessage]) -> Vec<ChatMessage> {
        // Step 1: merge consecutive system messages into one. Compaction
        // emits [system_original, system_synthetic]; send only one system.
        let mut merged: Vec<ChatMessage> = Vec::with_capacity(messages.len());
        for m in messages {
            if m.role == Role::System {
                if let Some(last) = merged.last_mut() {
                    if last.role == Role::System {
                        let prev = last.content.get_or_insert_with(String::new);
                        if let Some(ref new) = m.content {
                            prev.push('\n');
                            prev.push_str(new);
                        }
                        continue;
                    }
                }
            }
            merged.push(m.clone());
        }

        // Step 2: ensure nothing sits between assistant(tool_calls) and its
        // tool results. Any interleaved message is moved before the group.
        let mut result: Vec<ChatMessage> = Vec::with_capacity(merged.len());
        let mut i = 0;
        while i < merged.len() {
            if merged[i].role == Role::Assistant && !merged[i].tool_calls.is_empty() {
                let mut deferred: Vec<ChatMessage> = Vec::new();
                let mut tool_group: Vec<ChatMessage> = vec![merged[i].clone()];
                let mut j = i + 1;
                while j < merged.len() && merged[j].role != Role::Assistant {
                    if merged[j].role == Role::Tool {
                        tool_group.push(merged[j].clone());
                    } else {
                        deferred.push(merged[j].clone());
                    }
                    j += 1;
                }
                result.extend(deferred);
                result.extend(tool_group);
                i = j;
            } else {
                result.push(merged[i].clone());
                i += 1;
            }
        }

        result
    }
}

/// In-progress accumulator that reassembles an OpenAI streaming
/// conversation chunk-by-chunk into a single [`ChatResponse`]. Each
/// provider chunk is a JSON blob under a `data:` SSE frame; we forward
/// text deltas to the caller's [`StreamEvent`] sink and coalesce
/// per-index tool-call fragments in place.
#[derive(Default)]
pub(crate) struct OpenAiStreamAccumulator {
    /// Chain-of-thought text streamed under `delta.reasoning_content`.
    /// DeepSeek V4 thinking models require this to be echoed back in
    /// the next turn's assistant message or the next call 400s.
    reasoning_content: String,
    content: String,
    /// Char-indexed mirror of `content` so the streaming repeat
    /// detector can do char-aware window slicing without rebuilding a
    /// `Vec<char>` on every SSE frame.
    content_chars: Vec<char>,
    /// Hash → first-seen position for every char window of length
    /// `REPEAT_WINDOW_CHARS` we've already emitted in this response.
    /// If a freshly emitted window's hash collides with one already
    /// in the map, the model has just produced a verbatim repeat of
    /// an earlier fragment.
    seen_windows: std::collections::HashMap<u64, usize>,
    /// Number of characters in `content_chars` we've already scanned
    /// for new windows. Lets `feed` walk only the freshly appended
    /// region instead of re-hashing the whole content every chunk.
    scanned_up_to: usize,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    /// Set true the first time we observe a duplicate window. Once
    /// true, every subsequent SSE frame is dropped and `feed` returns
    /// Ok(false) so the HTTP stream is closed.
    pub(crate) repeat_canceled: bool,
    /// Tracks whether we've already emitted the "repeat detector
    /// tripped" warning for the current stream, so we don't spam the
    /// user with the same notice on every subsequent chunk in
    /// warn-only mode (the default).
    pub(crate) repeat_warned: bool,
    /// Whether tripping the repeat detector should hard-cancel the
    /// stream (true) or just emit a one-shot warning and keep going
    /// (false). Default is false: false positives surprised users
    /// mid-conversation in interactive REPL sessions and the loss
    /// of legit output was worse than letting a real loop run on.
    /// `new()` kills on repeat by default; set `METIS_REPEAT_NO_KILL` to warn only.
    /// at construction time so the behavior is fixed for the life
    /// of the stream and can't change underneath us mid-feed.
    pub(crate) kill_on_repeat: bool,
    /// Whether to run the streaming repeat detector at all. Some models
    /// (e.g. GLM 5.1 on z.ai) legitimately emit verbatim headings, list
    /// scaffolding and "Step 1…/Step 2…" patterns as part of normal
    /// reasoning; the 60/400 hash detector mis-fires on them and the
    /// user sees text silently truncated. `chat_stream` disables the
    /// scan for z.ai; everyone else keeps it on.
    pub(crate) scan_enabled: bool,
    /// Parallel scan state for `reasoning_content` (thinking) output.
    /// Models like MiniMax-M2 and DeepSeek-R1 stream their chain-of-thought
    /// in a separate `reasoning_content` field; the main-content scanner
    /// never sees it, so thinking loops ran unchecked. These three fields
    /// mirror `content_chars`, `seen_windows`, and `scanned_up_to` for the
    /// thinking stream.
    thinking_chars: Vec<char>,
    thinking_seen_windows: std::collections::HashMap<u64, usize>,
    thinking_scanned_up_to: usize,
}

/// Append one diagnostic line to ~/.metis/scan_debug.log when the
/// `METIS_DEBUG_SCAN` environment variable is set. Best-effort: any
/// IO error is silently dropped because the detector is on the hot
/// streaming path and a logging failure must never break a turn.
///
/// Format (tab-separated):
///   <unix_ms>\t<event>\t<chunk_chars>\t<total_chars>\t<scanned_up_to>
///
/// Inspect with: `tail -f ~/.metis/scan_debug.log` while reproducing.
fn debug_log_scan(event: &str, chunk_chars: usize, total_chars: usize, scanned_up_to: usize) {
    if std::env::var_os("METIS_DEBUG_SCAN").is_none() {
        return;
    }
    use std::io::Write;
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let dir = format!("{home}/.metis");
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{dir}/scan_debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(
            f,
            "{ts}\t{event}\t{chunk_chars}\t{total_chars}\t{scanned_up_to}"
        );
    }
}

/// Window length (in characters) for the streaming repeat detector.
///
/// Tuning notes:
/// - 30 is short enough to catch a single duplicated fragment within
///   the same sentence ("Burada kod yazmaya... Burada kod yazmaya...")
///   without waiting for the model to fill paragraphs of garbage.
/// - 30 is long enough that everyday Turkish/English prose almost
///   never produces a verbatim 30-char repeat by chance (we tested
///   with conversational sample text and saw zero false positives).
/// - The hash-based detector is char-aware so multibyte UTF-8
///   (Turkish dotted-i, emoji, CJK) doesn't break window slicing.
pub(crate) const REPEAT_WINDOW_CHARS: usize = 60;

/// Maximum char distance between two duplicate windows that still
/// counts as a "loop". A repeat farther apart than this is treated as
/// a legitimate distant reference (e.g. the model citing the same
/// identifier twice in a long response) and ignored. The DeepSeek
/// loops we observed in practice repeat within ~100 chars; 400 gives
/// generous margin without admitting whole-paragraph false positives.
pub(crate) const REPEAT_PROXIMITY_CHARS: usize = 400;

impl OpenAiStreamAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            // Kill mode is on by default — a looping model burns tokens and
            // produces nothing useful. Set METIS_REPEAT_NO_KILL to disable.
            kill_on_repeat: std::env::var_os("METIS_REPEAT_NO_KILL").is_none(),
            scan_enabled: true,
            ..Self::default()
        }
    }

    /// Feed one SSE `data:` payload. Returns `Ok(true)` to keep reading,
    /// `Ok(false)` when the stream's `[DONE]` sentinel is hit OR when
    /// the streaming repeat detector has fired.
    pub(crate) fn feed(
        &mut self,
        payload: &str,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> ApiResult<bool> {
        if payload == "[DONE]" {
            return Ok(false);
        }
        // Once a repetition was detected, drop every subsequent frame
        // and ask the SSE consumer to close the connection. The model
        // is in a loop; nothing useful is coming next, and continuing
        // burns tokens for content the user will never read.
        if self.repeat_canceled {
            return Ok(false);
        }
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| ApiError::Decode(format!("openai sse json: {e}")))?;

        // Usage frames: when `stream_options.include_usage` is set,
        // OpenAI sends a trailing chunk whose `choices` array is empty
        // and whose `usage` object carries the final counters.
        if let Some(usage_value) = value.get("usage") {
            if !usage_value.is_null() {
                if let Ok(usage) = serde_json::from_value::<Usage>(usage_value.clone()) {
                    self.usage = Some(usage);
                    on_event(StreamEvent::Usage(usage));
                }
            }
        }

        let Some(choices) = value.get("choices").and_then(|v| v.as_array()) else {
            return Ok(true);
        };
        for choice in choices {
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                self.finish_reason = Some(fr.to_string());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };

            // DeepSeek-reasoner / MiniMax-M2 emit chain-of-thought in a
            // `reasoning_content` field alongside regular `content`.
            if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    self.reasoning_content.push_str(text);
                    for ch in text.chars() {
                        self.thinking_chars.push(ch);
                    }
                    on_event(StreamEvent::ThinkingDelta(text.to_string()));
                    if self.scan_enabled && self.scan_for_thinking_repeat() {
                        if self.kill_on_repeat {
                            self.repeat_canceled = true;
                            on_event(StreamEvent::ThinkingDelta(
                                "\n\n[metis: thinking truncated — model entered a repeat loop]\n"
                                    .to_string(),
                            ));
                            return Ok(false);
                        }
                        if !self.repeat_warned {
                            self.repeat_warned = true;
                            on_event(StreamEvent::ThinkingDelta(
                                "\n[metis: repeat detector tripped in thinking — Ctrl+C to interrupt]\n"
                                    .to_string(),
                            ));
                        }
                        self.thinking_scanned_up_to = self.thinking_chars.len();
                    }
                }
            }

            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    self.content.push_str(text);
                    for ch in text.chars() {
                        self.content_chars.push(ch);
                    }
                    on_event(StreamEvent::TextDelta(text.to_string()));
                    debug_log_scan(
                        "feed",
                        text.chars().count(),
                        self.content_chars.len(),
                        self.scanned_up_to,
                    );
                    if self.scan_enabled && self.scan_for_repeat() {
                        // Kill mode is on by default. Set METIS_REPEAT_NO_KILL=1
                        // to switch to warn-only (shows the full reply even when
                        // a loop is detected).
                        if self.kill_on_repeat {
                            self.repeat_canceled = true;
                            on_event(StreamEvent::TextDelta(
                                "\n\n\x1b[31m[metis: response truncated — model entered a repeat loop]\x1b[0m\n"
                                    .to_string(),
                            ));
                            return Ok(false);
                        }
                        // Warn-only path: emit a single dim notice
                        // the first time we trip on this stream and
                        // then advance the scan cursor past the
                        // current content so we don't immediately
                        // re-fire on the same window. The stream
                        // continues normally and the user can Ctrl+C
                        // if it really is stuck in a loop.
                        if !self.repeat_warned {
                            self.repeat_warned = true;
                            on_event(StreamEvent::TextDelta(
                                "\n\x1b[2m[metis: repeat detector tripped — continuing anyway, Ctrl+C to interrupt]\x1b[0m\n"
                                    .to_string(),
                            ));
                        }
                        self.scanned_up_to = self.content_chars.len();
                    }
                }
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    // Grow the slot vector so we can address by index.
                    // OpenAI streams tool_calls sparsely — the first
                    // chunk for a call carries `id` and `function.name`,
                    // later chunks only append argument fragments.
                    while self.tool_calls.len() <= idx {
                        self.tool_calls.push(ToolCall {
                            id: String::new(),
                            kind: "function".to_string(),
                            function: ToolCallFunction {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        });
                    }
                    let slot = &mut self.tool_calls[idx];
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(kind) = tc.get("type").and_then(|v| v.as_str()) {
                        slot.kind = kind.to_string();
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                slot.function.name.push_str(name);
                            }
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            slot.function.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// Finalise the accumulator into an OpenAI-shaped [`ChatResponse`].
    pub(crate) fn into_response(self) -> ChatResponse {
        let content = if self.content.is_empty() {
            None
        } else {
            Some(self.content)
        };
        // Some providers (e.g. GLM/z.ai) stream tool_calls without an `id`.
        // An empty id is rejected by every OpenAI-compat API on the next turn
        // ("insufficient tool messages"). Fill in a synthetic id so the
        // assistant message + tool results round-trip cleanly.
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, mut tc)| {
                if tc.id.is_empty() {
                    tc.id = format!("call_{i}");
                }
                tc
            })
            .collect();
        let reasoning_content = if self.reasoning_content.is_empty() {
            None
        } else {
            Some(self.reasoning_content)
        };
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
                    reasoning_content,
                },
                finish_reason: self.finish_reason,
            }],
            usage: self.usage,
        }
    }

    /// Walk every char window of length [`REPEAT_WINDOW_CHARS`] that
    /// just became available since the last scan, and check whether
    /// any of them is a repeat of an earlier window within
    /// [`REPEAT_PROXIMITY_CHARS`].
    ///
    /// Why per-window hashing instead of suffix matching:
    /// - The previous suffix-anchored detector only checked the very
    ///   last 30 chars of the content. If the model produced
    ///   "ABC...ABC..." followed by a unique tail "...XYZ", the tail
    ///   pushed the duplicate out of the suffix window and detection
    ///   went blind. Hashing every window catches the repeat the
    ///   moment it lands, regardless of what the model emits next.
    /// - The proximity check (400 chars) suppresses legitimate
    ///   distant repetition — the same identifier mentioned twice in
    ///   a long explanation, for example — while still flagging the
    ///   tight loops that DeepSeek produces in practice.
    /// - State (`seen_windows`, `scanned_up_to`, `content_chars`) is
    ///   per-accumulator and a fresh accumulator is built per HTTP
    ///   call, so no cross-turn pollution. The previous bug where
    ///   shared mute state strangled later turns can't recur.
    ///
    /// Returns true on the first detected repeat. The caller flips
    /// `repeat_canceled` and stops feeding the stream.
    fn scan_for_repeat(&mut self) -> bool {
        use std::collections::hash_map::{DefaultHasher, Entry};
        use std::hash::{Hash, Hasher};

        let total = self.content_chars.len();
        if total < REPEAT_WINDOW_CHARS * 2 {
            return false;
        }
        // Walk every char window of length REPEAT_WINDOW_CHARS that
        // we haven't yet hashed. The first valid window starts at
        // index 0 (chars[0..W]); its `end` value is W. We must include
        // that window so the very start of the content can serve as a
        // match target later — earlier code started from W+1 and the
        // chars[0..W] window was lost forever, blinding the detector
        // to second-copy-starts-at-position-N patterns.
        let first_end = self.scanned_up_to.max(REPEAT_WINDOW_CHARS);
        for end in first_end..=total {
            let win_start = end - REPEAT_WINDOW_CHARS;
            let window: String = self.content_chars[win_start..end].iter().collect();
            let mut hasher = DefaultHasher::new();
            window.hash(&mut hasher);
            let h = hasher.finish();
            match self.seen_windows.entry(h) {
                Entry::Vacant(v) => {
                    v.insert(win_start);
                }
                Entry::Occupied(mut o) => {
                    let prev_start = *o.get();
                    let distance = win_start.saturating_sub(prev_start);
                    debug_log_scan("hit", distance, prev_start, win_start);
                    if distance > 0 && distance <= REPEAT_PROXIMITY_CHARS {
                        // Loop pattern. Pin the scanned mark so the
                        // detector doesn't reprocess this window if
                        // a future feed somehow runs again.
                        self.scanned_up_to = end;
                        debug_log_scan("FIRE", distance, prev_start, win_start);
                        return true;
                    }
                    // distance > REPEAT_PROXIMITY_CHARS: the stored occurrence
                    // is too far back to count as a loop now. Update it to the
                    // current position so future occurrences measure distance
                    // against the most recent one. Without this, a window that
                    // first appeared in pre-loop text (>400 chars ago) would
                    // permanently blind the detector to loop occurrences of the
                    // same pattern.
                    // distance == 0: re-hash of same window, don't update.
                    if distance > REPEAT_PROXIMITY_CHARS {
                        *o.get_mut() = win_start;
                    }
                }
            }
        }
        self.scanned_up_to = total + 1;
        false
    }

    /// Mirror of [`scan_for_repeat`] that operates on the thinking
    /// (`reasoning_content`) buffer instead of the main content buffer.
    fn scan_for_thinking_repeat(&mut self) -> bool {
        use std::collections::hash_map::{DefaultHasher, Entry};
        use std::hash::{Hash, Hasher};

        let total = self.thinking_chars.len();
        if total < REPEAT_WINDOW_CHARS * 2 {
            return false;
        }
        let first_end = self.thinking_scanned_up_to.max(REPEAT_WINDOW_CHARS);
        for end in first_end..=total {
            let win_start = end - REPEAT_WINDOW_CHARS;
            let window: String = self.thinking_chars[win_start..end].iter().collect();
            let mut hasher = DefaultHasher::new();
            window.hash(&mut hasher);
            let h = hasher.finish();
            match self.thinking_seen_windows.entry(h) {
                Entry::Vacant(v) => {
                    v.insert(win_start);
                }
                Entry::Occupied(mut o) => {
                    let prev_start = *o.get();
                    let distance = win_start.saturating_sub(prev_start);
                    if distance > 0 && distance <= REPEAT_PROXIMITY_CHARS {
                        self.thinking_scanned_up_to = end;
                        return true;
                    }
                    if distance > REPEAT_PROXIMITY_CHARS {
                        *o.get_mut() = win_start;
                    }
                }
            }
        }
        self.thinking_scanned_up_to = total + 1;
        false
    }
}

#[async_trait]
impl ChatProvider for OpenAICompatClient {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        // Two-step image handling on a text-only model:
        //   1. Strip leftover image blocks from earlier turns. Without
        //      this, one historical image would lock the conversation
        //      forever — every subsequent text turn would re-trigger
        //      the fail-fast below.
        //   2. Only the *current* user turn's image triggers the 400.
        //      The REPL retry layer can catch that error and route to
        //      a vision-capable provider via `vision_fallback`.
        let request: std::borrow::Cow<'_, ChatRequest> = if Self::model_is_text_only(&request.model)
            && Self::request_has_images(request)
            && !Self::last_user_has_images(request)
        {
            std::borrow::Cow::Owned(ChatRequest {
                messages: Self::strip_image_blocks_from_history(&request.messages),
                ..request.clone()
            })
        } else {
            std::borrow::Cow::Borrowed(request)
        };
        let request = request.as_ref();
        if Self::last_user_has_images(request) && Self::model_is_text_only(&request.model) {
            return Err(ApiError::Status {
                status: 400,
                body: format!(
                    "model `{}` doesn't support image input. Switch to a vision-capable \
                     model (e.g. gpt-4o, claude-sonnet-4-5, glm-4.5v) before attaching images.",
                    request.model
                ),
            });
        }
        let url = self.chat_completions_url();
        // Always strip orphaned tool_call turns first, then apply
        // provider-specific sanitization on top.
        let clean_messages = Self::sanitize_tool_calls(&request.messages);
        let clean_request = ChatRequest {
            messages: clean_messages,
            ..request.clone()
        };
        let mut body = if self.is_gemini() {
            let sanitized = ChatRequest {
                messages: Self::sanitize_for_gemini(&clean_request.messages),
                ..clean_request.clone()
            };
            serde_json::to_value(&sanitized).expect("ChatRequest serializes")
        } else if self.is_minimax() || self.is_nvidia() {
            let sanitized = ChatRequest {
                messages: Self::sanitize_for_minimax(&clean_request.messages),
                ..clean_request.clone()
            };
            serde_json::to_value(&sanitized).expect("ChatRequest serializes")
        } else {
            serde_json::to_value(&clean_request).expect("ChatRequest serializes")
        };
        Self::rewrite_multimodal_content(&mut body);
        if !self.is_gemini() && Self::is_claude_model(&clean_request.model) {
            Self::inject_cache_control(&mut body);
        }
        if !self.is_gemini() && !self.is_minimax() && !self.is_nvidia() {
            Self::inject_tool_choice(&mut body);
        }
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
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
        response
            .json::<ChatResponse>()
            .await
            .map_err(|err| ApiError::Decode(err.to_string()))
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        // Same two-step image handling as the non-stream path:
        // strip leftover history images on a text-only model, then
        // fail-fast only on the current turn's user image.
        let request: std::borrow::Cow<'_, ChatRequest> = if Self::model_is_text_only(&request.model)
            && Self::request_has_images(request)
            && !Self::last_user_has_images(request)
        {
            std::borrow::Cow::Owned(ChatRequest {
                messages: Self::strip_image_blocks_from_history(&request.messages),
                ..request.clone()
            })
        } else {
            std::borrow::Cow::Borrowed(request)
        };
        let request = request.as_ref();
        if Self::last_user_has_images(request) && Self::model_is_text_only(&request.model) {
            return Err(ApiError::Status {
                status: 400,
                body: format!(
                    "model `{}` doesn't support image input. Switch to a vision-capable \
                     model (e.g. gpt-4o, claude-sonnet-4-5, glm-4.5v) before attaching images.",
                    request.model
                ),
            });
        }
        let url = self.chat_completions_url();
        // Strip orphaned tool_call turns, then provider-specific sanitization.
        let actual_request;
        let req = if self.is_gemini() {
            actual_request = ChatRequest {
                messages: Self::sanitize_for_gemini(&Self::sanitize_tool_calls(&request.messages)),
                ..request.clone()
            };
            &actual_request
        } else if self.is_minimax() || self.is_nvidia() {
            // MiniMax and NVIDIA NIM apply the same tool-call ordering
            // fixes as Gemini (no interleaved messages between a tool_calls
            // turn and its results, no consecutive system messages) but
            // without the Gemini-specific user-role coercions.
            actual_request = ChatRequest {
                messages: Self::sanitize_for_minimax(&Self::sanitize_tool_calls(&request.messages)),
                ..request.clone()
            };
            &actual_request
        } else {
            actual_request = ChatRequest {
                messages: Self::sanitize_tool_calls(&request.messages),
                ..request.clone()
            };
            &actual_request
        };
        let mut body = Self::build_stream_body(req);
        // MiniMax and NVIDIA NIM don't support the OpenAI-specific
        // stream_options extension and return 400 when it's present.
        if self.is_minimax() || self.is_nvidia() {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("stream_options");
            }
        }
        // Inject tool_choice:"auto" + parallel_tool_calls for providers that
        // support the OpenAI tool_choice field. Without this, DeepSeek V4 and
        // similar models sometimes default to text responses even when a tool
        // call would be correct. Gemini, MiniMax, NVIDIA NIM excluded.
        if !self.is_gemini() && !self.is_minimax() && !self.is_nvidia() {
            Self::inject_tool_choice(&mut body);
        }
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
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

        let mut acc = OpenAiStreamAccumulator::new();
        // GLM / z.ai and DeepSeek models emit legitimate verbatim
        // repetition as part of reasoning output (headings, "Step 1/Step 2"
        // scaffolding, bulleted templates). The 60-char / 400-proximity hash
        // detector treats these as loops and truncates mid-response. Skip
        // the scan for these providers; real loops there are rare and visible.
        if self.is_zai() || self.base_url.contains("api.deepseek.com") {
            acc.scan_enabled = false;
        }
        consume_sse_async(response, |payload| acc.feed(payload, on_event)).await?;
        Ok(acc.into_response())
    }
}
