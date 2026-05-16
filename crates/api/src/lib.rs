//! HTTP client and OpenAI-compatible message types for `aegis`.
//!
//! The v0.1 client speaks the OpenAI Chat Completions wire format and is
//! pointed at DeepSeek by default. The schema deliberately stays close to
//! the upstream OpenAI shape so that adding xAI / OpenAI / Together / etc.
//! later costs nothing more than a new base URL and API-key env var.
//!
//! Design choices that are intentional:
//!
//! * **Async HTTP via tokio.** All provider calls are async, running on a
//!   tokio multi-thread runtime. This enables concurrent tool execution,
//!   non-blocking streaming, and paves the way for parallel subagents.
//! * **Flat `ChatMessage` struct.** Rather than modelling messages as a
//!   role-tagged enum, we mirror OpenAI's wire format directly: a single
//!   struct with optional `content`, `tool_calls`, and `tool_call_id`.
//!   This makes serialization a one-liner and removes a translation step
//!   between our internal representation and the network format.

mod anthropic;
pub mod autotune;
mod failover;
mod openai;
mod provider;
mod sse;
mod subprocess;
pub mod types;
mod vision_fallback;

pub use anthropic::AnthropicClient;
#[cfg(test)]
use anthropic::AnthropicStreamAccumulator;
pub use failover::{CircuitBreaker, FailoverEvent, FailoverLink, FailoverProvider};
pub use openai::{DeepSeekClient, OpenAICompatClient};
pub use subprocess::ClaudeSubprocessClient;
pub use vision_fallback::VisionFallbackProvider;
#[cfg(test)]
use openai::{OpenAiStreamAccumulator, REPEAT_PROXIMITY_CHARS, REPEAT_WINDOW_CHARS};
pub use provider::{ChatProvider, Provider, WireFormat};
pub use sse::{consume_sse, consume_sse_async};
pub use types::{
    ApiError, ApiResult, ChatChoice, ChatMessage, ChatRequest, ChatResponse, ContentBlock,
    FunctionSpec, Role, StreamEvent, ToolCall, ToolCallFunction, ToolKind, ToolSpec, Usage,
};

#[cfg(test)]
mod tests;
