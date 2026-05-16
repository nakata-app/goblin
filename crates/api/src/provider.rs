//! Provider abstraction: the [`ChatProvider`] trait the agent loop
//! talks to, and the built-in [`Provider`] table that maps a CLI
//! `--provider <id>` flag to a base URL, env var, default model, and
//! wire format.
//!
//! Two concrete providers live here: the default fallback streaming
//! impl on the trait, plus a blanket impl for `Box<dyn ChatProvider>`
//! so the CLI can carry one boxed client through the agent loop
//! without explicit `.as_ref()` calls.

use async_trait::async_trait;

use crate::{
    AnthropicClient, ApiError, ApiResult, ChatRequest, ChatResponse, ClaudeSubprocessClient,
    OpenAICompatClient, StreamEvent,
};

/// The single capability the agent loop needs from a provider: send a
/// chat request and get a parsed response back. Pinning this behind a
/// trait lets the loop swap in a mock in tests without dragging HTTP in,
/// and paves the way for multi-provider support later without touching
/// the call sites.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse>;

    /// Streamed variant. The provider calls `on_event` with each
    /// incremental [`StreamEvent`] as it arrives and returns the fully
    /// assembled [`ChatResponse`] when the stream ends — semantically
    /// identical to [`chat`](Self::chat), but with partial text visible
    /// as it is produced.
    ///
    /// The default implementation is non-streaming: it calls `chat`,
    /// synthesises a single `TextDelta` + `Usage` pair, and returns.
    /// Real HTTP clients override it to parse SSE. Test doubles that
    /// only implement `chat` automatically inherit the fallback, which
    /// is why every existing `ScriptedProvider`-style test keeps
    /// compiling without touching the new method.
    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        let response = self.chat(request).await?;
        if let Some(choice) = response.choices.first() {
            if let Some(text) = choice.message.content.as_deref() {
                if !text.is_empty() {
                    on_event(StreamEvent::TextDelta(text.to_string()));
                }
            }
        }
        if let Some(usage) = response.usage {
            on_event(StreamEvent::Usage(usage));
        }
        Ok(response)
    }
}

/// Blanket impl so a `Box<dyn ChatProvider>` can stand in anywhere a
/// borrow of a `ChatProvider` is needed. Lets the CLI hold one boxed
/// client and pass it straight to the agent loop without a manual
/// `as_ref()` step at every call site.
#[async_trait]
impl<T: ChatProvider + ?Sized> ChatProvider for Box<T> {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        (**self).chat(request).await
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        (**self).chat_stream(request, on_event).await
    }
}

/// Wire format spoken by a provider. Most providers in the wild reuse
/// the OpenAI Chat Completions shape; Anthropic ships its own Messages
/// API that uses a separate `system` field, content blocks for tool
/// calls, and `x-api-key` headers instead of bearer auth. The CLI hides
/// the difference behind the [`ChatProvider`] trait — the agent loop
/// only ever sees an OpenAI-shaped request and response, and Anthropic's
/// adapter does the translation in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// OpenAI Chat Completions, also spoken by DeepSeek, xAI Grok,
    /// OpenRouter, Together, and most local inference servers.
    OpenAICompat,
    /// Anthropic Messages API.
    Anthropic,
    /// Local `claude` CLI subprocess (Claude Code). No API key needed —
    /// uses the user's existing Pro/Max subscription.
    ClaudeSubprocess,
}

/// Static description of a provider: how to reach it, what env var
/// holds its key, what wire format it speaks, and what model to use
/// when the user does not pass `--model`. Built-in entries live in
/// [`Provider::BUILTINS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Short id used on the CLI (`--provider deepseek`).
    pub id: &'static str,
    /// Base URL of the provider's API root, without any path suffix.
    pub base_url: &'static str,
    /// Environment variable name that holds the API key.
    pub env_var: &'static str,
    /// Model used when the user does not pass `--model`.
    pub default_model: &'static str,
    /// Which wire format this provider speaks.
    pub wire: WireFormat,
}

impl Provider {
    /// Built-in providers. Eight OpenAI-compatible endpoints plus
    /// Anthropic, which speaks its own Messages API and is bridged by
    /// [`AnthropicClient`] under the same [`ChatProvider`] trait.
    pub const BUILTINS: &'static [Provider] = &[
        Provider {
            id: "deepseek",
            base_url: "https://api.deepseek.com",
            env_var: "DEEPSEEK_API_KEY",
            default_model: "deepseek-v4-flash",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "openai",
            base_url: "https://api.openai.com",
            env_var: "OPENAI_API_KEY",
            default_model: "gpt-4o-mini",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "openrouter",
            base_url: "https://openrouter.ai/api",
            env_var: "OPENROUTER_API_KEY",
            default_model: "deepseek/deepseek-chat",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "glm",
            // The earlier base_url `https://z.ai/api` returned
            // `{"code":500,"msg":"404 NOT_FOUND"}` on every call —
            // no handler listens on that host/path. The real
            // OpenAI-compatible endpoint is on the `api.z.ai`
            // subdomain under `/api/paas/v4/`. Verified by direct
            // curl against /chat/completions and /models (glm-5.1
            // returns a normal completion and shows up in the list).
            base_url: "https://api.z.ai/api/paas/v4",
            env_var: "ZAI_API_KEY",
            default_model: "glm-5.1",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "gemini",
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
            env_var: "GEMINI_API_KEY",
            default_model: "gemini-2.5-flash",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "anthropic",
            base_url: "https://api.anthropic.com",
            env_var: "ANTHROPIC_API_KEY",
            // Cheapest current Anthropic model — keeps default runs
            // affordable; users override with `--model` for Sonnet/Opus.
            default_model: "claude-haiku-4-5-20251001",
            wire: WireFormat::Anthropic,
        },
        Provider {
            id: "minimax",
            base_url: "https://api.minimax.io/anthropic",
            env_var: "MINIMAX_API_KEY",
            default_model: "MiniMax-M2.7",
            wire: WireFormat::Anthropic,
        },
        Provider {
            id: "nvidia",
            base_url: "https://integrate.api.nvidia.com",
            env_var: "NVIDIA_API_KEY",
            // V4 Flash = NIM's stable default workhorse; matches what
            // Atakan was running on DeepSeek-direct before the balance
            // ran out, so swapping the provider to NIM is byte-for-byte
            // the same model on a key that's actually funded.
            default_model: "deepseek-ai/deepseek-v4-flash",
            wire: WireFormat::OpenAICompat,
        },
        Provider {
            id: "claude",
            // Not a real URL — subprocess doesn't do HTTP. Kept non-empty
            // so the struct is uniform; `client_from_env` ignores it.
            base_url: "subprocess://claude",
            // No API key required — Claude Code CLI uses the user's
            // existing Pro/Max subscription.
            env_var: "",
            default_model: "claude-sonnet-4-6",
            wire: WireFormat::ClaudeSubprocess,
        },
    ];

    /// Looks up a built-in provider by its short id (case-insensitive).
    pub fn lookup(id: &str) -> Option<&'static Provider> {
        let lowered = id.to_ascii_lowercase();
        Self::BUILTINS.iter().find(|p| p.id == lowered)
    }

    /// Builds a client for this provider, reading the API key from the
    /// declared environment variable. Returns a boxed [`ChatProvider`]
    /// so OpenAI-compat and Anthropic clients can be selected behind a
    /// single dispatch point.
    pub fn client_from_env(&self) -> ApiResult<Box<dyn ChatProvider>> {
        match self.wire {
            WireFormat::ClaudeSubprocess => {
                if !ClaudeSubprocessClient::is_available() {
                    return Err(ApiError::Status {
                        status: 503,
                        body: "`claude` binary not found on $PATH — install Claude Code CLI first".into(),
                    });
                }
                Ok(Box::new(ClaudeSubprocessClient::new()))
            }
            _ => {
                let api_key = std::env::var(self.env_var)
                    .map_err(|_| ApiError::MissingKey(self.env_var))?;
                match self.wire {
                    WireFormat::OpenAICompat => {
                        Ok(Box::new(OpenAICompatClient::new(self.base_url, api_key)?))
                    }
                    WireFormat::Anthropic => {
                        Ok(Box::new(AnthropicClient::new(self.base_url, api_key)?))
                    }
                    WireFormat::ClaudeSubprocess => unreachable!(),
                }
            }
        }
    }
}
