//! Cross-provider vision fallback wrapper.
//!
//! Wraps a primary `ChatProvider` (typically a text-only model like
//! `deepseek-v4-flash`) so an attached image transparently re-routes
//! to a vision-capable provider when the primary refuses. Triggered
//! by `OpenAICompatClient`'s "doesn't support image input" 400 emitted
//! when a text-only model gets handed an image; on every other error
//! class the wrapper just bubbles the primary's result up unchanged.
//!
//! Unlike `FailoverProvider`, this is *not* a transient-error retry:
//! image rejection is a deterministic 4xx, not a flaky 5xx. The two
//! wrappers compose — `vision_fallback(failover(primary), vision)` —
//! so 5xx blips on the primary still walk the failover chain, while
//! image attachments shortcut straight to a vision-capable client
//! without touching the failover chain (which would just hit the
//! same 400 on another text-only fallback).

use async_trait::async_trait;

use crate::provider::ChatProvider;
use crate::types::{ApiError, ApiResult, ChatRequest, ChatResponse, StreamEvent};

/// Wraps a primary client + a vision-capable client. On a primary
/// "doesn't support image input" rejection, re-issues the request
/// against the vision client with `vision_model` swapped in.
pub struct VisionFallbackProvider {
    primary: Box<dyn ChatProvider>,
    vision: Box<dyn ChatProvider>,
    vision_model: String,
}

impl VisionFallbackProvider {
    pub fn new(
        primary: Box<dyn ChatProvider>,
        vision: Box<dyn ChatProvider>,
        vision_model: impl Into<String>,
    ) -> Self {
        Self {
            primary,
            vision,
            vision_model: vision_model.into(),
        }
    }

    /// True when the primary's error is the client-side image-input
    /// rejection emitted by `OpenAICompatClient`. Anything else
    /// (auth, network, 5xx, decode) is passed through unchanged so
    /// vision-routing does not mask unrelated failures.
    fn is_image_input_error(err: &ApiError) -> bool {
        matches!(err, ApiError::Status { body, .. } if body.contains("doesn't support image input"))
    }

    /// Build a request that points at the vision model. The agentic
    /// system prompt and tool spec are dropped: a typical vision
    /// model has a much smaller context window than the primary text
    /// model (NIM's `meta/llama-3.2-90b-vision-instruct` caps at
    /// 32K), and a 60K-character agentic prompt + tools list blew
    /// past that on the very first image attempt. The vision call
    /// only needs the user's text + image; conversation history is
    /// kept in case earlier turns supply context the user is asking
    /// about, but tool definitions and the multi-page system prompt
    /// are stripped because the vision model is not going to call a
    /// tool — it only answers about the picture.
    fn rerouted(&self, request: &ChatRequest) -> ChatRequest {
        let trimmed_messages: Vec<crate::ChatMessage> = request
            .messages
            .iter()
            .filter(|m| !matches!(m.role, crate::types::Role::System))
            .cloned()
            .collect();
        ChatRequest {
            model: self.vision_model.clone(),
            messages: trimmed_messages,
            tools: None,
            ..request.clone()
        }
    }
}

#[async_trait]
impl ChatProvider for VisionFallbackProvider {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        match self.primary.chat(request).await {
            Ok(response) => Ok(response),
            Err(err) if Self::is_image_input_error(&err) => {
                let req = self.rerouted(request);
                self.vision.chat(&req).await
            }
            Err(err) => Err(err),
        }
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        // Image-input rejection is a synchronous client-side check
        // that fires before any HTTP request goes out, so the primary
        // emits zero stream events before failing. That makes a
        // fallback re-stream against the vision client safe — the
        // caller has not yet seen any tokens. For every other
        // failure class we intentionally do NOT fall back: once
        // tokens have started arriving via `on_event`, restarting
        // the stream against a different model would duplicate
        // output the user already saw.
        match self.primary.chat_stream(request, on_event).await {
            Ok(response) => Ok(response),
            Err(err) if Self::is_image_input_error(&err) => {
                let req = self.rerouted(request);
                if std::env::var("AEGIS_VISION_DEBUG").is_ok() {
                    eprintln!(
                        "[vision_fallback] routing to {} | messages={} | tools={} | system_prompt_len={}",
                        req.model,
                        req.messages.len(),
                        req.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                        req.messages
                            .iter()
                            .find(|m| matches!(m.role, crate::types::Role::System))
                            .and_then(|m| m.content.as_ref())
                            .map(|s| s.len())
                            .unwrap_or(0)
                    );
                    if let Ok(json) = serde_json::to_string(&req) {
                        let _ = std::fs::write("/tmp/aegis_vision_dump.json", &json);
                        eprintln!(
                            "[vision_fallback] full request dumped to /tmp/aegis_vision_dump.json ({} bytes)",
                            json.len()
                        );
                    }
                }
                let result = self.vision.chat_stream(&req, on_event).await;
                if std::env::var("AEGIS_VISION_DEBUG").is_ok() {
                    if let Err(ref e) = result {
                        eprintln!("[vision_fallback] vision call failed: {e:#}");
                    }
                }
                result
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ContentBlock};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Minimal scripted provider: returns a fixed result for `chat`,
    /// records the model on every call so tests can assert which
    /// client received which request.
    struct ScriptedProvider {
        result: std::sync::Mutex<Option<ApiResult<ChatResponse>>>,
        last_model: Arc<std::sync::Mutex<Option<String>>>,
        call_count: Arc<AtomicUsize>,
    }

    impl ScriptedProvider {
        fn new(result: ApiResult<ChatResponse>) -> Self {
            Self {
                result: std::sync::Mutex::new(Some(result)),
                last_model: Arc::new(std::sync::Mutex::new(None)),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn last_model(&self) -> Arc<std::sync::Mutex<Option<String>>> {
            Arc::clone(&self.last_model)
        }

        fn call_count(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.call_count)
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
            *self.last_model.lock().unwrap() = Some(request.model.clone());
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("scripted provider used twice — give it a second result")
        }
    }

    fn ok_response(text: &str) -> ChatResponse {
        ChatResponse {
            choices: vec![crate::types::ChatChoice {
                message: ChatMessage::assistant_text(text),
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        }
    }

    fn image_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user_multimodal(vec![
                ContentBlock::Text {
                    text: "describe".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ])],
            tools: None,
            temperature: None,
            max_tokens: None,
            thinking: false,
            thinking_budget: 0,
        }
    }

    #[tokio::test]
    async fn image_input_400_routes_to_vision_with_swapped_model() {
        // Primary refuses with the exact body OpenAICompatClient
        // emits; wrapper must re-issue against the vision client
        // and rewrite `request.model` to the vision model.
        let primary = Box::new(ScriptedProvider::new(Err(ApiError::Status {
            status: 400,
            body: "model `deepseek-v4-flash` doesn't support image input. Switch to a vision-capable model".to_string(),
        })));
        let vision = Box::new(ScriptedProvider::new(Ok(ok_response("Red."))));
        let vision_model_seen = vision.last_model();
        let vision_calls = vision.call_count();

        let wrapper = VisionFallbackProvider::new(
            primary,
            vision,
            "meta/llama-3.2-90b-vision-instruct",
        );
        let resp = wrapper
            .chat(&image_request("deepseek-v4-flash"))
            .await
            .expect("vision fallback must succeed");

        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Red.")
        );
        assert_eq!(vision_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            vision_model_seen.lock().unwrap().as_deref(),
            Some("meta/llama-3.2-90b-vision-instruct"),
            "vision client must see the vision model, not the primary's"
        );
    }

    #[tokio::test]
    async fn non_image_error_passes_through_untouched() {
        // 503 from primary must NOT trigger vision fallback —
        // image-routing is reserved for the deterministic 4xx
        // image-input rejection. Otherwise every transient blip
        // would burn the bigger vision model's quota.
        let primary = Box::new(ScriptedProvider::new(Err(ApiError::Status {
            status: 503,
            body: "Service Unavailable".to_string(),
        })));
        let vision = Box::new(ScriptedProvider::new(Ok(ok_response("should not be called"))));
        let vision_calls = vision.call_count();

        let wrapper =
            VisionFallbackProvider::new(primary, vision, "meta/llama-3.2-90b-vision-instruct");
        let err = wrapper
            .chat(&image_request("deepseek-v4-flash"))
            .await
            .expect_err("503 must surface, not fall back");
        match err {
            ApiError::Status { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Status 503, got {other:?}"),
        }
        assert_eq!(
            vision_calls.load(Ordering::SeqCst),
            0,
            "vision client must not be called on non-image errors"
        );
    }

    /// Capture the request the vision client receives so a test can
    /// assert that system messages and tool specs were stripped.
    struct CapturingProvider {
        captured: Arc<std::sync::Mutex<Option<ChatRequest>>>,
        result: std::sync::Mutex<Option<ApiResult<ChatResponse>>>,
    }

    impl CapturingProvider {
        fn new(result: ApiResult<ChatResponse>) -> Self {
            Self {
                captured: Arc::new(std::sync::Mutex::new(None)),
                result: std::sync::Mutex::new(Some(result)),
            }
        }

        fn captured(&self) -> Arc<std::sync::Mutex<Option<ChatRequest>>> {
            Arc::clone(&self.captured)
        }
    }

    #[async_trait]
    impl ChatProvider for CapturingProvider {
        async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
            *self.captured.lock().unwrap() = Some(request.clone());
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("scripted provider used twice")
        }
    }

    #[tokio::test]
    async fn vision_request_drops_system_prompt_and_tools() {
        // The NIM Llama 3.2 90B Vision model has a 32K context
        // window. Aegis' default system prompt is ~60K characters,
        // which blew past that on the first real image attempt and
        // came back as a length-related 400. The wrapper must strip
        // the system message and the tool spec before re-issuing
        // against the vision client — neither is useful to a
        // vision-only model and both eat scarce tokens.
        let primary = Box::new(ScriptedProvider::new(Err(ApiError::Status {
            status: 400,
            body: "doesn't support image input".to_string(),
        })));
        let vision_inner = CapturingProvider::new(Ok(ok_response("ok")));
        let captured = vision_inner.captured();
        let vision = Box::new(vision_inner);

        let req = ChatRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                ChatMessage::system("LONG AGENTIC SYSTEM PROMPT (60K chars in real life)…"),
                ChatMessage::user_multimodal(vec![
                    ContentBlock::Text {
                        text: "what's in the image?".into(),
                    },
                    ContentBlock::Image {
                        media_type: "image/jpeg".into(),
                        data: "AAAA".into(),
                    },
                ]),
            ],
            tools: Some(vec![]),
            temperature: None,
            max_tokens: None,
            thinking: false,
            thinking_budget: 0,
        };
        let wrapper =
            VisionFallbackProvider::new(primary, vision, "meta/llama-3.2-90b-vision-instruct");
        wrapper.chat(&req).await.expect("vision succeeds");

        let seen = captured.lock().unwrap().clone().expect("vision was called");
        assert!(
            !seen
                .messages
                .iter()
                .any(|m| matches!(m.role, crate::types::Role::System)),
            "system messages must be stripped before hitting the vision model"
        );
        assert!(seen.tools.is_none(), "tool spec must be dropped on vision retry");
        assert_eq!(seen.model, "meta/llama-3.2-90b-vision-instruct");
    }

    #[tokio::test]
    async fn primary_success_does_not_call_vision() {
        // Happy path: primary handles the request fine (e.g. an image
        // attached to a vision-capable model directly). Wrapper must
        // be a transparent no-op.
        let primary = Box::new(ScriptedProvider::new(Ok(ok_response("from primary"))));
        let vision = Box::new(ScriptedProvider::new(Ok(ok_response("should not be called"))));
        let vision_calls = vision.call_count();

        let wrapper =
            VisionFallbackProvider::new(primary, vision, "meta/llama-3.2-90b-vision-instruct");
        let resp = wrapper
            .chat(&image_request("gpt-4o"))
            .await
            .expect("primary success");
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("from primary"));
        assert_eq!(vision_calls.load(Ordering::SeqCst), 0);
    }
}
