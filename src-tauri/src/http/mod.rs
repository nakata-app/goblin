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
    /// Bootstrap session's agent slot. Only used as the fallback when
    /// the multi-agent map has no entry for the current session id.
    pub agent: Arc<Mutex<Option<AgentLoop>>>,
    /// Same map AppState exposes — lets the HTTP handler target the
    /// foreground session's slot instead of always hitting bootstrap.
    pub agents: Arc<std::sync::RwLock<std::collections::HashMap<String, Arc<Mutex<Option<AgentLoop>>>>>>,
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

    let app = build_app(state, cfg.token.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| format!("bind {}: {}", cfg.bind, e))?;

    println!("[http] listening on {}", cfg.bind);
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve: {}", e))
}

pub(crate) fn build_app(state: HttpState, token: String) -> Router {
    let auth_layer = axum::middleware::from_fn(move |req, next| {
        let token = token.clone();
        async move { auth_check(token, req, next).await }
    });

    Router::new()
        .route("/health", get(health))
        .route("/message", post(post_message))
        .route("/sessions", get(get_sessions))
        .route("/memory", get(search_memory))
        .layer(auth_layer)
        .with_state(state)
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

    // Target the slot owned by the current session id, falling back to
    // the bootstrap slot only if the agents map has nothing for it.
    // This keeps HTTP and desktop in lockstep when the user has
    // switched tabs in the foreground window.
    let slot = {
        let agents = state.agents.read()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("agents lock: {}", e)))?;
        agents.get(&session_id).cloned().unwrap_or_else(|| state.agent.clone())
    };
    let mut agent_guard = slot.lock().await;
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
            &[],
            &[],
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

    // Real-port smoke test: bind a fresh router on 127.0.0.1:0, then drive
    // it with reqwest exactly the way a phone or curl on the LAN would.
    // Proves the auth middleware, route table, and JSON serialization all
    // wire up against a live tokio listener — not just an in-process tower
    // oneshot. If this passes, `cargo run --bin goblin` exposes the same
    // surface (modulo a real provider behind `agent`).
    #[tokio::test]
    async fn http_smoke_real_listener() {
        use crate::memory::MemoryDb;
        use crate::session::SessionStore;
        use rusqlite::Connection;
        use std::sync::{Arc, Mutex as StdMutex};
        use tokio::sync::Mutex;

        let tmp = std::env::temp_dir().join(format!("goblin-http-smoke-{}.db", uuid::Uuid::new_v4()));
        let mem_db = MemoryDb::open(tmp.to_str().unwrap()).expect("memory db open");
        mem_db.init_schema().expect("memory schema");
        let mem = Arc::new(mem_db);

        let session_conn = Connection::open_in_memory().expect("session conn");
        let sessions = Arc::new(SessionStore::new(session_conn));
        sessions.init_schema().expect("session schema");

        let cfg = crate::config::Config {
            providers: crate::config::ProvidersConfig {
                openai: None,
                anthropic: None,
                nvidia: None,
                gemini: None,
                glm: None,
                generic: vec![],
                auto_route: Default::default(),
                multi_agent: Default::default(),
            },
            agent: Default::default(),
            tools: Default::default(),
            memory: Default::default(),
            stt: Default::default(),
            tts: Default::default(),
            mnemonics: Default::default(),
            mcp: Default::default(),
            channels: Default::default(),
            http: Default::default(),
        };

        let state = HttpState {
            agent: Arc::new(Mutex::new(None)),
            agents: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            config: Arc::new(std::sync::RwLock::new(cfg)),
            memory: mem,
            session_id: Arc::new(StdMutex::new("smoke-session".to_string())),
            session_store: sessions,
        };

        let token = "smoke-token-xyz".to_string();
        let app = build_app(state, token.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);

        // 1. no auth → 401
        let r = client.get(format!("{}/health", base)).send().await.expect("send 1");
        assert_eq!(r.status(), 401, "no-auth health must be 401");

        // 2. wrong token → 401
        let r = client
            .get(format!("{}/health", base))
            .bearer_auth("wrong")
            .send()
            .await
            .expect("send 2");
        assert_eq!(r.status(), 401, "wrong-token health must be 401");

        // 3. correct token → 200 + {"status":"ok"}
        let r = client
            .get(format!("{}/health", base))
            .bearer_auth(&token)
            .send()
            .await
            .expect("send 3");
        assert_eq!(r.status(), 200);
        let body: serde_json::Value = r.json().await.expect("health json");
        assert_eq!(body["status"], "ok");

        // 4. /message with agent=None → 503 (proves routing + handler reach,
        //    and that we fail closed instead of panicking when no provider).
        let r = client
            .post(format!("{}/message", base))
            .bearer_auth(&token)
            .json(&serde_json::json!({"text": "hi"}))
            .send()
            .await
            .expect("send 4");
        assert_eq!(r.status(), 503, "agent=None must yield 503");

        // 5. /sessions → 200 + {"sessions": []} on a fresh in-memory store.
        let r = client
            .get(format!("{}/sessions", base))
            .bearer_auth(&token)
            .send()
            .await
            .expect("send 5");
        assert_eq!(r.status(), 200);
        let body: serde_json::Value = r.json().await.expect("sessions json");
        assert!(body["sessions"].is_array(), "sessions must be an array");

        // 6. /memory?q=... → 200 + {"memories": []} on an empty db.
        //    Proves Query<MemoryQuery> deserialization and the search path.
        let r = client
            .get(format!("{}/memory?q=anything", base))
            .bearer_auth(&token)
            .send()
            .await
            .expect("send 6");
        assert_eq!(r.status(), 200);
        let body: serde_json::Value = r.json().await.expect("memory json");
        assert!(body["memories"].is_array(), "memories must be an array");

        // 7. /memory without required `q` query param → 400 (axum's
        //    Query<T> extractor rejects missing fields before our handler).
        let r = client
            .get(format!("{}/memory", base))
            .bearer_auth(&token)
            .send()
            .await
            .expect("send 7");
        assert_eq!(r.status(), 400, "missing q must be 400");

        server.abort();
        let _ = std::fs::remove_file(&tmp);
    }
}
