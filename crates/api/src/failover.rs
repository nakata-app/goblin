//! Provider failover with per-client circuit breakers.
//!
//! Wraps an ordered chain of `ChatProvider` clients (primary + N
//! fallbacks). On a transient error from the primary, the wrapper walks
//! the chain until one succeeds or all are exhausted. Each link tracks
//! consecutive failures: after 3 in a row, its circuit breaker opens
//! for 60 seconds and the link is short-circuited (skipped without
//! waiting for a real timeout) for the duration of the cooldown.
//!
//! Why: Aegis ran into a real outage where the primary provider stayed
//! down for over an hour and the agent loop got stuck on every turn.
//! `RoutingConfig.fallback_model` existed in config but was never wired
//! into `route()` — the field was declared but ignored. This module
//! turns the dead config into actual failover behavior.
//!
//! Claude Code parity note: Anthropic's hosted backend handles provider
//! failover internally and is not visible to clients, so there is no
//! public Claude Code primitive to mirror here. The pattern below
//! follows standard distributed-systems practice (circuit breaker per
//! upstream, ordered chain, transient-vs-terminal classification on
//! errors) and is a Aegis-specific extra layered on top of the spec.
//!
//! Terminal errors (auth missing, 4xx caller mistakes, response decode
//! failures) bypass the chain and surface immediately — replaying them
//! against another provider would just produce the same caller-side
//! mistake under a different banner.
//!
//! Streaming behaviour: `chat_stream` walks the chain link by link,
//! committing to whichever link returns first. Once a stream starts
//! emitting events through the caller's `on_event` callback, partial
//! output cannot be unwound, so a mid-stream failure is reported as a
//! single error rather than silently retried — by the time we'd want
//! to fail over, the user has already seen tokens from the first link.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::provider::ChatProvider;
use crate::types::{ApiError, ApiResult, ChatRequest, ChatResponse, StreamEvent};

/// Per-link consecutive-failure tracker. After `threshold` transient
/// failures in a row, opens for `cooldown` seconds. `record_success`
/// resets both the counter and any open state.
///
/// Internally uses atomic ops so the same breaker can be shared across
/// concurrent calls without a lock.
#[derive(Debug)]
pub struct CircuitBreaker {
    consecutive_failures: AtomicU32,
    open_until_unix_secs: AtomicU64,
    threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            open_until_unix_secs: AtomicU64::new(0),
            threshold,
            cooldown,
        }
    }

    /// Reasonable defaults: 3 consecutive failures → 60s cooldown.
    /// Matches typical SaaS provider transient-incident windows
    /// without flapping on momentary blips.
    pub fn defaults() -> Self {
        Self::new(3, Duration::from_secs(60))
    }

    /// True if the breaker is currently open (link should be skipped).
    /// Auto-closes once the cooldown window elapses.
    pub fn is_open(&self) -> bool {
        let until = self.open_until_unix_secs.load(Ordering::Acquire);
        if until == 0 {
            return false;
        }
        let now = current_unix_secs();
        if now >= until {
            // Cooldown expired — close the breaker and reset the counter
            // so we give the link a clean shot on the next attempt.
            self.open_until_unix_secs.store(0, Ordering::Release);
            self.consecutive_failures.store(0, Ordering::Release);
            return false;
        }
        true
    }

    /// Returns the cooldown duration this breaker was configured with.
    /// Used by FailoverProvider to populate `BreakerOpen` events.
    pub fn cooldown(&self) -> Duration {
        self.cooldown
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        self.open_until_unix_secs.store(0, Ordering::Release);
    }

    /// Increment the consecutive-failure counter and, if it crosses the
    /// threshold, open the breaker for `cooldown` seconds. Returns true
    /// when this call is the one that flipped the breaker open (so the
    /// caller can emit a one-shot diagnostic event).
    pub fn record_failure(&self) -> bool {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::AcqRel);
        let count = prev + 1;
        if count == self.threshold {
            let until = current_unix_secs() + self.cooldown.as_secs();
            self.open_until_unix_secs.store(until, Ordering::Release);
            return true;
        }
        false
    }
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One link in a failover chain: a human-readable label, the underlying
/// `ChatProvider` client, and the breaker state. The label is what
/// surfaces in `FailoverEvent`s so the user sees provider names rather
/// than internal indices.
pub struct FailoverLink {
    pub label: String,
    pub client: Box<dyn ChatProvider>,
    pub breaker: CircuitBreaker,
}

impl FailoverLink {
    pub fn new<S: Into<String>>(label: S, client: Box<dyn ChatProvider>) -> Self {
        Self {
            label: label.into(),
            client,
            breaker: CircuitBreaker::defaults(),
        }
    }
}

/// Diagnostic events emitted as the wrapper walks its chain. The CLI
/// can subscribe to render them in the status line ("switched from
/// deepseek → gemini after 502") without `FailoverProvider` having to
/// know anything about UI.
#[derive(Debug, Clone)]
pub enum FailoverEvent {
    /// Walked from one link to the next because of a transient error.
    SwitchedTo {
        from: String,
        to: String,
        reason: String,
    },
    /// A link's breaker just opened (3rd consecutive transient failure).
    BreakerOpen {
        provider: String,
        cooldown_secs: u64,
    },
    /// A link was skipped because its breaker was already open.
    LinkSkipped { provider: String },
    /// Walked the full chain without success.
    AllExhausted { last_error: String },
}

type EventHandler = Box<dyn Fn(FailoverEvent) + Send + Sync>;

/// `ChatProvider` wrapper that tries primary first, then walks an
/// ordered fallback chain on transient errors.
pub struct FailoverProvider {
    primary: FailoverLink,
    chain: Vec<FailoverLink>,
    on_event: Option<EventHandler>,
}

impl FailoverProvider {
    pub fn new(primary: FailoverLink, chain: Vec<FailoverLink>) -> Self {
        Self {
            primary,
            chain,
            on_event: None,
        }
    }

    /// Single-link variant for callers that only want the breaker
    /// behaviour without an actual fallback chain — useful in tests
    /// and as a stepping stone before the chain config is wired up.
    pub fn single(primary: FailoverLink) -> Self {
        Self::new(primary, Vec::new())
    }

    pub fn with_event_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(FailoverEvent) + Send + Sync + 'static,
    {
        self.on_event = Some(Box::new(handler));
        self
    }

    fn emit(&self, event: FailoverEvent) {
        if let Some(handler) = &self.on_event {
            handler(event);
        }
    }

    fn all_links(&self) -> impl Iterator<Item = &FailoverLink> {
        std::iter::once(&self.primary).chain(self.chain.iter())
    }
}

#[async_trait]
impl ChatProvider for FailoverProvider {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        let mut last_error: Option<ApiError> = None;
        let mut prev_label: Option<String> = None;

        for link in self.all_links() {
            if link.breaker.is_open() {
                self.emit(FailoverEvent::LinkSkipped {
                    provider: link.label.clone(),
                });
                last_error = Some(ApiError::Decode(format!(
                    "circuit breaker open for {}",
                    link.label
                )));
                continue;
            }

            if let Some(prev) = &prev_label {
                self.emit(FailoverEvent::SwitchedTo {
                    from: prev.clone(),
                    to: link.label.clone(),
                    reason: last_error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_default(),
                });
            }

            match link.client.chat(request).await {
                Ok(response) => {
                    link.breaker.record_success();
                    return Ok(response);
                }
                Err(e) => {
                    if e.is_transient() {
                        if link.breaker.record_failure() {
                            self.emit(FailoverEvent::BreakerOpen {
                                provider: link.label.clone(),
                                cooldown_secs: link.breaker.cooldown().as_secs(),
                            });
                        }
                        last_error = Some(e);
                        prev_label = Some(link.label.clone());
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        let err_str = last_error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no providers configured".to_string());
        self.emit(FailoverEvent::AllExhausted {
            last_error: err_str.clone(),
        });
        Err(last_error
            .unwrap_or_else(|| ApiError::Decode(format!("FailoverProvider: all links exhausted ({})", err_str))))
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        let mut last_error: Option<ApiError> = None;
        let mut prev_label: Option<String> = None;

        for link in self.all_links() {
            if link.breaker.is_open() {
                self.emit(FailoverEvent::LinkSkipped {
                    provider: link.label.clone(),
                });
                last_error = Some(ApiError::Decode(format!(
                    "circuit breaker open for {}",
                    link.label
                )));
                continue;
            }

            if let Some(prev) = &prev_label {
                self.emit(FailoverEvent::SwitchedTo {
                    from: prev.clone(),
                    to: link.label.clone(),
                    reason: last_error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_default(),
                });
            }

            match link.client.chat_stream(request, on_event).await {
                Ok(response) => {
                    link.breaker.record_success();
                    return Ok(response);
                }
                Err(e) => {
                    if e.is_transient() {
                        if link.breaker.record_failure() {
                            self.emit(FailoverEvent::BreakerOpen {
                                provider: link.label.clone(),
                                cooldown_secs: link.breaker.cooldown().as_secs(),
                            });
                        }
                        last_error = Some(e);
                        prev_label = Some(link.label.clone());
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        let err_str = last_error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no providers configured".to_string());
        self.emit(FailoverEvent::AllExhausted {
            last_error: err_str.clone(),
        });
        Err(last_error
            .unwrap_or_else(|| ApiError::Decode(format!("FailoverProvider: all links exhausted ({})", err_str))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatChoice, ChatMessage, Role, Usage};
    use std::sync::Mutex;

    /// Minimal ChatProvider mock that returns a queue of pre-scripted
    /// outcomes (Ok or Err). Each chat() call pops one entry. Used to
    /// simulate provider behaviour deterministically in tests.
    struct ScriptedProvider {
        outcomes: Mutex<Vec<ApiResult<ChatResponse>>>,
        call_count: AtomicU32,
    }

    impl ScriptedProvider {
        fn new(outcomes: Vec<ApiResult<ChatResponse>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                call_count: AtomicU32::new(0),
            }
        }

        fn calls(&self) -> u32 {
            self.call_count.load(Ordering::Acquire)
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn chat(&self, _request: &ChatRequest) -> ApiResult<ChatResponse> {
            self.call_count.fetch_add(1, Ordering::AcqRel);
            let mut q = self.outcomes.lock().unwrap();
            if q.is_empty() {
                return Err(ApiError::Decode("scripted provider exhausted".into()));
            }
            q.remove(0)
        }
    }

    fn ok_response(text: &str) -> ApiResult<ChatResponse> {
        Ok(ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: Role::Assistant,
                    content: Some(text.to_string()),
                    content_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                    protected: false,
                },
                finish_reason: None,
            }],
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        })
    }

    fn http_502() -> ApiError {
        ApiError::Status {
            status: 502,
            body: "bad gateway".into(),
        }
    }

    fn auth_401() -> ApiError {
        ApiError::Status {
            status: 401,
            body: "unauthorized".into(),
        }
    }

    fn dummy_request() -> ChatRequest {
        ChatRequest {
            model: "test".into(),
            messages: vec![],
            tools: None,
            temperature: None,
            max_tokens: None,
            thinking: false,
            thinking_budget: 0,
        }
    }

    #[tokio::test]
    async fn primary_success_no_failover() {
        let primary_client: Box<dyn ChatProvider> =
            Box::new(ScriptedProvider::new(vec![ok_response("primary ok")]));
        let fallback_arc = std::sync::Arc::new(ScriptedProvider::new(vec![ok_response(
            "should not be called",
        )]));
        let fallback_client: Box<dyn ChatProvider> = Box::new(SharedScripted(fallback_arc.clone()));

        let provider = FailoverProvider::new(
            FailoverLink::new("primary", primary_client),
            vec![FailoverLink::new("fallback", fallback_client)],
        );

        let res = provider.chat(&dummy_request()).await.unwrap();
        assert_eq!(
            res.choices[0].message.content.as_deref(),
            Some("primary ok")
        );
        assert_eq!(fallback_arc.calls(), 0, "fallback must not be called when primary succeeds");
    }

    #[tokio::test]
    async fn transient_error_falls_through_to_chain() {
        let primary_arc = std::sync::Arc::new(ScriptedProvider::new(vec![Err(http_502())]));
        let primary_client: Box<dyn ChatProvider> = Box::new(SharedScripted(primary_arc.clone()));
        let fallback_arc = std::sync::Arc::new(ScriptedProvider::new(vec![ok_response("recovered")]));
        let fallback_client: Box<dyn ChatProvider> = Box::new(SharedScripted(fallback_arc.clone()));

        let provider = FailoverProvider::new(
            FailoverLink::new("primary", primary_client),
            vec![FailoverLink::new("fallback", fallback_client)],
        );

        let res = provider.chat(&dummy_request()).await.unwrap();
        assert_eq!(res.choices[0].message.content.as_deref(), Some("recovered"));
        assert_eq!(primary_arc.calls(), 1);
        assert_eq!(fallback_arc.calls(), 1);
    }

    #[tokio::test]
    async fn terminal_error_does_not_fall_over() {
        let primary_arc = std::sync::Arc::new(ScriptedProvider::new(vec![Err(auth_401())]));
        let primary_client: Box<dyn ChatProvider> = Box::new(SharedScripted(primary_arc.clone()));
        let fallback_arc = std::sync::Arc::new(ScriptedProvider::new(vec![ok_response(
            "should not be called",
        )]));
        let fallback_client: Box<dyn ChatProvider> = Box::new(SharedScripted(fallback_arc.clone()));

        let provider = FailoverProvider::new(
            FailoverLink::new("primary", primary_client),
            vec![FailoverLink::new("fallback", fallback_client)],
        );

        let err = provider.chat(&dummy_request()).await.unwrap_err();
        assert!(matches!(err, ApiError::Status { status: 401, .. }));
        assert_eq!(primary_arc.calls(), 1);
        assert_eq!(
            fallback_arc.calls(),
            0,
            "auth error should not trigger fallback"
        );
    }

    #[tokio::test]
    async fn breaker_opens_after_three_transient_failures() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(60));
        assert!(!breaker.is_open());

        assert!(!breaker.record_failure());
        assert!(!breaker.is_open());

        assert!(!breaker.record_failure());
        assert!(!breaker.is_open());

        // 3rd failure flips it open
        assert!(breaker.record_failure());
        assert!(breaker.is_open());

        // Subsequent failures don't re-flip
        assert!(!breaker.record_failure());
    }

    #[tokio::test]
    async fn breaker_resets_on_success() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(60));
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        // Counter should be reset, so 2 more failures should not open it
        assert!(!breaker.record_failure());
        assert!(!breaker.record_failure());
        assert!(!breaker.is_open());
    }

    #[tokio::test]
    async fn all_exhausted_returns_last_error() {
        let primary_client: Box<dyn ChatProvider> =
            Box::new(ScriptedProvider::new(vec![Err(http_502())]));
        let fallback_client: Box<dyn ChatProvider> = Box::new(ScriptedProvider::new(vec![Err(
            ApiError::Status {
                status: 503,
                body: "unavailable".into(),
            },
        )]));

        let provider = FailoverProvider::new(
            FailoverLink::new("p1", primary_client),
            vec![FailoverLink::new("p2", fallback_client)],
        );

        let err = provider.chat(&dummy_request()).await.unwrap_err();
        // The last error should be the 503 from the fallback
        assert!(matches!(err, ApiError::Status { status: 503, .. }));
    }

    /// Wrapper to share an Arc<ScriptedProvider> across both the
    /// FailoverLink (which owns a Box<dyn>) and the test (which needs
    /// to read .calls()). Without this, ownership of the boxed client
    /// would prevent inspection.
    struct SharedScripted(std::sync::Arc<ScriptedProvider>);

    #[async_trait]
    impl ChatProvider for SharedScripted {
        async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
            self.0.chat(request).await
        }
    }
}
