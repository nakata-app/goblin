//! Local HTTP API. Off by default; enabled from `[http]` in
//! ~/.goblin/config.toml. Designed for phone / second-laptop / cron
//! callers that want the same agent the desktop window is driving.
//!
//! Every request must carry `Authorization: Bearer <token>` matching
//! `http.token`. An empty token refuses to start the server — we'd
//! rather Goblin be unreachable than reachable without auth.
//!
//! Endpoints (v0):
//!   GET  /health                  liveness probe
//!   POST /message                  { "text": "...", "model": ?str }
//!   GET  /sessions                  list recent sessions
//!   GET  /memory?q=...              search memory

use crate::agent::r#loop::AgentLoop;
use crate::config::{Config, HttpConfig};
use crate::memory::MemoryDb;
use crate::session::SessionStore;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct HttpState {
    pub agent: Arc<Mutex<Option<AgentLoop>>>,
    pub config: Arc<std::sync::RwLock<Config>>,
    pub memory: Arc<MemoryDb>,
    pub session_id: Arc<StdMutex<String>>,
    pub session_store: Arc<SessionStore>,
}

#[derive(Debug, Deserialize)]
struct MessageReq {
    text: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct MessageResp {
    content: String,
    tokens_in: u32,
    tokens_out: u32,
    model: String,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct MemoryQuery {
    q: String,
    #[serde(default)]
    ns: Option<String>,
}

pub async fn serve(state: HttpState, cfg: HttpConfig) -> Result<(), String> {
    if cfg.token.is_empty() {
        return Err(
            "http.enabled = true but http.token is empty — refusing to start. Set a token in ~/.goblin/config.toml.".to_string()
        );
    }

    let token = cfg.token.clone();
    let auth_layer = axum::middleware::from_fn(move |req, next| {
        let token = token.clone();
        async move { auth_check(token, req, next).await }
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/message", post(post_message))
        .route("/sessions", get(get_sessions))
        .route("/memory", get(search_memory))
        .layer(auth_layer)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| format!("bind {}: {}", cfg.bind, e))?;

    println!("[http] listening on {}", cfg.bind);
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve: {}", e))
}

async fn auth_check(
    expected: String,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if presented == expected && !presented.is_empty() {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn post_message(
    State(state): State<HttpState>,
    _headers: HeaderMap,
    Json(req): Json<MessageReq>,
) -> Result<Json<MessageResp>, (StatusCode, String)> {
    let session_id = state
        .session_id
        .lock()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("session lock: {}", e)))?
        .clone();

    let ns = format!("session:{}", session_id);
    let memories = crate::memory::inject::inject_memories(&state.memory, &ns, 5);
    let learned = crate::memory::inject::inject_learned(&state.memory, 5);

    let selected_model = if req.model.is_none() {
        let cfg = state
            .config
            .read()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("config lock: {}", e)))?;
        Some(cfg.auto_route_model(&req.text, false).to_string())
    } else {
        req.model
    };

    let mut agent_guard = state.agent.lock().await;
    let agent = agent_guard
        .as_mut()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "Agent not initialized — configure a provider in ~/.goblin/config.toml".to_string()))?;

    let soul = crate::agent::soul::load_soul();
    let response = agent
        .send_message(
            &req.text,
            None,
            &memories,
            &learned,
            selected_model.as_deref(),
            None,
            soul.as_deref(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(MessageResp {
        content: response.content,
        tokens_in: response.tokens_in,
        tokens_out: response.tokens_out,
        model: response.model,
        session_id,
    }))
}

async fn get_sessions(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sessions = state
        .session_store
        .list(50)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({ "sessions": sessions })))
}

async fn search_memory(
    State(state): State<HttpState>,
    Query(q): Query<MemoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let records = state
        .memory
        .search_memories(q.ns.as_deref(), &q.q, 20)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({ "memories": records })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The auth middleware is the most security-sensitive part of this
    // module; the rest is plumbing. We test the bearer-token compare in
    // isolation against the helper below, mirroring the same string
    // logic the middleware uses.
    fn extract_bearer(header_value: Option<&str>) -> &str {
        header_value
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("")
    }

    #[test]
    fn bearer_token_match() {
        assert_eq!(extract_bearer(Some("Bearer abc123")), "abc123");
        assert_eq!(extract_bearer(Some("Bearer ")), "");
        assert_eq!(extract_bearer(Some("Basic abc")), "");
        assert_eq!(extract_bearer(None), "");
    }

    #[test]
    fn empty_presented_never_matches_empty_expected() {
        // Equivalent to the && !presented.is_empty() guard in
        // auth_check: matching empty-to-empty must not succeed.
        let presented = "";
        let expected = "";
        let ok = presented == expected && !presented.is_empty();
        assert!(!ok);
    }
}
