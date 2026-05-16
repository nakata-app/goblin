//! Shared wire and value types used across the API crate.
//!
//! Keeps `lib.rs` as a thin façade of `mod` declarations and `pub use`
//! re-exports. All message, tool, request/response, usage, and stream
//! shapes live here so provider adapters (anthropic/openai) can import
//! them without pulling crate-internal state.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors surfaced by the API client.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned non-success status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("response body could not be parsed: {0}")]
    Decode(String),
    #[error("API key missing — set the {0} environment variable")]
    MissingKey(&'static str),
    /// Provider call exceeded the per-attempt time budget. Surfaced
    /// when `tokio::time::timeout` fires around `chat_stream` because
    /// the upstream SSE connection stalled without closing. Treated
    /// as transient so the agent's existing retry loop can recover.
    #[error("provider call timed out after {seconds}s")]
    Timeout { seconds: u64 },
}

impl ApiError {
    /// True if the error is plausibly a transient/network/server-side
    /// problem that justifies retry or failover. False for terminal
    /// errors (auth, bad request, parse failures, missing config) that
    /// will repeat on retry and should surface to the user immediately.
    ///
    /// Used by `FailoverProvider` to decide whether to walk the chain
    /// or fail fast.
    pub fn is_transient(&self) -> bool {
        match self {
            // Network errors: connection refused, timeout, DNS, TLS — all transient.
            ApiError::Http(_) => true,
            // 5xx server-side, 408 request timeout, 429 rate limit
            // are transient. Other 4xx are caller errors and won't
            // change on retry.
            ApiError::Status { status, .. } => {
                *status >= 500 || *status == 408 || *status == 429
            }
            // Body parse failure or missing API key — terminal.
            ApiError::Decode(_) => false,
            ApiError::MissingKey(_) => false,
            // Provider hung past the per-attempt budget: retry with
            // a fresh connection is the right move.
            ApiError::Timeout { .. } => true,
        }
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// Role on a single chat message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single content block within a message. Most messages have a single
/// `Text` block, but multimodal messages may include `Image` or
/// `Document` blocks alongside text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        /// MIME type, e.g. "image/png", "image/jpeg".
        media_type: String,
        /// Base64-encoded image data.
        data: String,
    },
    Document {
        /// MIME type, e.g. "application/pdf".
        media_type: String,
        /// Base64-encoded document data.
        data: String,
    },
}

/// One message in a chat conversation. Mirrors the OpenAI wire format so
/// it can be serialized straight into the request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Multimodal content blocks. When non-empty, providers that support
    /// vision/documents will use these instead of the flat `content` string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// OpenAI's tool messages carry an optional `name`. Some providers
    /// reject the field if it's missing on tool replies, others reject
    /// it if it's present, so we make it optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Internal-only flag: if true, this message is exempt from compaction.
    /// Used to preserve critical context like the output of `read_file`
    /// across multiple turns. Not part of the wire format.
    #[serde(skip)]
    pub protected: bool,
    /// DeepSeek V4 thinking models return a `reasoning_content` field on
    /// assistant messages. The API requires it to be echoed back verbatim
    /// in subsequent turns — omitting it triggers a 400
    /// "The `reasoning_content` in the thinking mode must be passed back
    /// to the API". Captured during streaming and serialized back on the
    /// OpenAI-compat wire. `None` on every non-reasoning message so the
    /// field is absent from the body for normal assistants/users/tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        }
    }

    /// Creates a user message with multimodal content blocks.
    pub fn user_multimodal(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: None,
            content_blocks: blocks,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        }
    }

    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            protected: false,
            reasoning_content: None,
        }
    }

    /// Creates a tool result with multimodal content blocks (e.g., image from read_file).
    pub fn tool_result_multimodal(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        blocks: Vec<ContentBlock>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: None,
            content_blocks: blocks,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            protected: false,
            reasoning_content: None,
        }
    }
}

/// Tool description sent in the request `tools` array. Wraps a function
/// declaration in OpenAI's `{ "type": "function", "function": { ... } }`
/// envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Function,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    /// Raw JSON Schema describing the parameters object.
    pub parameters: serde_json::Value,
}

/// A single tool invocation requested by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_kind")]
    pub kind: String,
    pub function: ToolCallFunction,
}

fn default_tool_kind() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Raw JSON-encoded argument object as the model produced it.
    pub arguments: String,
}

/// Request body for `POST /v1/chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Enable extended thinking / reasoning mode. When true, the
    /// provider is asked to show its chain-of-thought before the
    /// final answer. Anthropic: `thinking` blocks. DeepSeek: uses
    /// the `deepseek-reasoner` model or `reasoning_content` field.
    /// Providers that don't support thinking ignore this flag.
    #[serde(skip_serializing)]
    pub thinking: bool,
    /// Budget for thinking tokens (default 10000). Only used when
    /// `thinking` is true. Anthropic uses this for
    /// `thinking.budget_tokens`.
    #[serde(skip_serializing)]
    pub thinking_budget: u32,
}

/// Response body returned by `POST /v1/chat/completions`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Token counters returned by the provider after each call.
///
/// The five counters are normalised across providers so the agent loop
/// and the cost meter can treat them uniformly:
///
/// * `prompt_tokens` — fresh, **non-cached** input tokens paid at full
///   input rate. OpenAI reports `prompt_tokens` as the total input
///   including cached reads; the custom [`Deserialize`] impl below
///   peels the cached portion off so this field is always just the new
///   input.
/// * `completion_tokens` — assistant output tokens.
/// * `total_tokens` — sum of everything paid for this turn
///   (`prompt + completion + cache_read + cache_write`). Always
///   recomputed locally rather than trusted from the wire so it stays
///   consistent with the normalised fresh-input count.
/// * `cache_read_tokens` — input tokens that hit the provider cache.
///   Anthropic's `cache_read_input_tokens`; OpenAI's
///   `prompt_tokens_details.cached_tokens`. Billed at a fraction of
///   the full input rate.
/// * `cache_write_tokens` — input tokens newly written into the cache.
///   Anthropic's `cache_creation_input_tokens`; OpenAI does not charge
///   separately for cache writes so it always stays at zero there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

/// Raw on-the-wire usage shape. Unions every field both providers
/// might emit so a single [`Deserialize`] pass can cover OpenAI Chat
/// Completions and Anthropic Messages responses without a second
/// manual parse.
#[derive(Debug, Default, Deserialize)]
struct UsageWire {
    // OpenAI Chat Completions shape.
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    // `total_tokens` is present on the wire but ignored — we always
    // recompute the total locally so it stays consistent with the
    // normalised fresh-input count, and trusting the wire value would
    // double-count Anthropic's cache reads.
    #[serde(default, rename = "total_tokens")]
    #[allow(dead_code)]
    _total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetailsWire>,
    // Anthropic Messages shape.
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetailsWire {
    #[serde(default)]
    cached_tokens: u32,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = UsageWire::deserialize(deserializer)?;
        Ok(Usage::from_wire(wire))
    }
}

impl Usage {
    /// Fold a raw wire shape into the normalised form. Branches on
    /// which provider's fields are populated:
    ///
    /// * If any of `prompt_tokens`, `completion_tokens`, or
    ///   `prompt_tokens_details` is set, treat this as an OpenAI shape
    ///   — cache reads live in `prompt_tokens_details.cached_tokens`
    ///   and are already counted inside `prompt_tokens`, so we peel
    ///   them off. OpenAI does not bill cache creation separately, so
    ///   `cache_write_tokens` stays zero.
    /// * Otherwise treat it as an Anthropic shape where `input_tokens`
    ///   is already the fresh non-cached count and the cache fields
    ///   are flat siblings.
    ///
    /// `total_tokens` is always recomputed locally so it stays
    /// consistent with the normalised fresh-input count; trusting the
    /// wire total would double-count cached reads on Anthropic.
    fn from_wire(wire: UsageWire) -> Self {
        let is_openai_shape = wire.prompt_tokens > 0
            || wire.prompt_tokens_details.is_some()
            || wire.completion_tokens > 0;
        if is_openai_shape {
            let cache_read = wire
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0);
            let fresh_prompt = wire.prompt_tokens.saturating_sub(cache_read);
            let total = fresh_prompt
                .saturating_add(wire.completion_tokens)
                .saturating_add(cache_read);
            Self {
                prompt_tokens: fresh_prompt,
                completion_tokens: wire.completion_tokens,
                total_tokens: total,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
            }
        } else {
            let total = wire
                .input_tokens
                .saturating_add(wire.output_tokens)
                .saturating_add(wire.cache_read_input_tokens)
                .saturating_add(wire.cache_creation_input_tokens);
            Self {
                prompt_tokens: wire.input_tokens,
                completion_tokens: wire.output_tokens,
                total_tokens: total,
                cache_read_tokens: wire.cache_read_input_tokens,
                cache_write_tokens: wire.cache_creation_input_tokens,
            }
        }
    }
}

/// Incremental event surfaced during a streamed chat call. The agent
/// loop forwards these to an optional user callback so the CLI can
/// render partial text as it arrives; non-streaming providers still
/// synthesise a single `TextDelta` + `Usage` pair via the default
/// [`ChatProvider::chat_stream`] implementation, so the same plumbing
/// works for real HTTP clients and for test doubles.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant text. Callbacks should print it immediately
    /// and flush, not buffer it.
    TextDelta(String),
    /// A chunk of thinking/reasoning text. Rendered dimmed so the user
    /// can follow the model's chain-of-thought without confusing it
    /// with the final answer.
    ThinkingDelta(String),
    /// Final token counters for the turn, emitted once near the end of
    /// the stream. Some providers only send usage after the last chunk,
    /// so callbacks must not assume it has arrived mid-stream.
    Usage(Usage),
    /// The agent is about to execute a tool call. Emitted by the agent
    /// loop (not by providers) right before `Tool::execute` runs, so the
    /// CLI can render a one-line preview like `→ read_file {"path":"a"}`.
    /// `arguments_preview` is the raw JSON argument string, already
    /// truncated to a sensible width by the emitter.
    ToolCall {
        name: String,
        arguments_preview: String,
    },
    /// A tool finished executing. Emitted by the agent loop after
    /// `Tool::execute` returns, so the CLI can show a collapsed result.
    ToolResult {
        name: String,
        /// First line or truncated preview of the result.
        preview: String,
        /// True if the result was an error.
        is_error: bool,
    },
    /// A transient error occurred mid-stream and the agent will retry.
    /// The REPL should clear any partial output from the failed attempt
    /// and reset the markdown renderer so the retry starts clean.
    RetryReset,
}
