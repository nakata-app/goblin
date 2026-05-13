mod agent;
mod channel;
mod config;
pub mod headless;
mod http;
mod cron;
mod daemon;
mod memory;
mod provider;
mod session;
mod task;
mod tools;
mod whatsapp;
mod mnemonics;
mod plugin;
mod mcp;

use agent::r#loop::AgentLoop;
use crate::config::Config;
use crate::cron::{CronStore, execute_script_job, CronJob};
use provider::openai::OpenAIProvider;
use provider::anthropic::AnthropicProvider;
use provider::nvidia::NvidiaProvider;
use provider::gemini::GeminiProvider;
use provider::glm::GlmProvider;
use memory::{MemoryDb, inject, compact, observe, reinforcement};
use session::SessionStore;
use task::TaskStore;
use tools::ToolRegistry;
use whatsapp::WhatsappBridge;
use mnemonics::MnemonicsClient;
use plugin::PluginRegistry;
use mcp::McpHub;
use tokio::sync::Mutex;
use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use tauri::Emitter;
use tauri::State;
use tauri::Manager;
use tauri::RunEvent;

struct AppState {
    // The active session's agent, kept on `agent` for backwards
    // compatibility with single-session callers. `agents` is the full
    // map keyed by session id — new sessions land here and the desktop
    // window swaps `agent` to point at the current one. The map is
    // declared but not yet read by send_message; it is the seam a
    // future multi-window UI will plug into without touching the
    // single-window codepath.
    agent: Arc<Mutex<Option<AgentLoop>>>,
    #[allow(dead_code)]
    agents: Arc<std::sync::RwLock<std::collections::HashMap<String, Arc<Mutex<Option<AgentLoop>>>>>>,
    // Shared tool registry — every AgentLoop in `agents` holds an Arc
    // clone of this single registry, so save_config can swap it
    // atomically without rebuilding each session.
    tool_registry: Arc<std::sync::RwLock<Arc<ToolRegistry>>>,
    config: Arc<std::sync::RwLock<Config>>,
    memory: Arc<MemoryDb>,
    session_id: Arc<StdMutex<String>>,
    session_store: Arc<SessionStore>,
    cron_store: CronStore,
    task_store: TaskStore,
    whatsapp: Arc<WhatsappBridge>,
    mnemonics: Option<Arc<MnemonicsClient>>,
    plugins: Arc<PluginRegistry>,
    mcp: Arc<McpHub>,
}

fn calculate_cost(tokens_in: u32, tokens_out: u32, model: &str) -> f64 {
    if model.contains("deepseek") {
        (tokens_in as f64 / 1_000_000.0) * 0.28 + (tokens_out as f64 / 1_000_000.0) * 1.10
    } else if model.contains("gpt-4") || model.contains("claude") {
        (tokens_in as f64 / 1_000_000.0) * 3.0 + (tokens_out as f64 / 1_000_000.0) * 15.0
    } else {
        (tokens_in as f64 / 1_000_000.0) * 0.28 + (tokens_out as f64 / 1_000_000.0) * 1.10
    }
}

fn serialize_conversation(messages: &[provider::Message]) -> String {
    messages
        .iter()
        .filter_map(|m| serde_json::to_string(m).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn deserialize_conversation(jsonl: &str) -> Vec<provider::Message> {
    jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<provider::Message>(line).ok())
        .collect()
}

#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    message: String,
    model: Option<String>,
) -> Result<serde_json::Value, String> {
    let mut agent_guard = state.agent.lock().await;

    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    let ns = format!("session:{}", session_id);
    let memories = inject::inject_memories(&state.memory, &ns, 5);
    let learned = inject::inject_learned(&state.memory, 5);

    let selected_model = if model.is_none() {
        let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
        let auto = cfg.auto_route_model(&message, false);
        Some(auto.to_string())
    } else {
        model
    };

    let agent = agent_guard
        .as_mut()
        .ok_or_else(|| "Agent not initialized. Configure a provider in ~/.goblin/config.toml".to_string())?;

    // Emit progress: thinking started
    let _ = app.emit("agent-progress", serde_json::json!({
        "type": "thinking",
        "model": selected_model.as_deref().unwrap_or("auto"),
    }));

    // Channel for real-time tool progress events from the agent loop
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let progress_app = app.clone();

    // Spawn task that bridges progress events to Tauri events
    let progress_task = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            let _ = progress_app.emit("agent-progress", event);
        }
    });

    let soul = agent::soul::load_soul();
    let response = agent
        .send_message(&message, None, &memories, &learned, selected_model.as_deref(), Some(progress_tx), soul.as_deref())
        .await;

    // Ensure progress task completes
    progress_task.abort();

    let response = response.map_err(|e| {
        let _ = app.emit("agent-progress", serde_json::json!({
            "type": "error",
            "error": e,
        }));
        // Push to outbound channels too — the desktop user might not be
        // watching the window when an overnight cron task blows up.
        channel::publish("error", &e);
        e
    })?;

    let cost = calculate_cost(response.tokens_in, response.tokens_out, &response.model);
    state.session_store.update_stats(&session_id, response.tokens_in as i64, response.tokens_out as i64, cost, &response.model).ok();

    for obs in &response.observations {
        observe::observe_tool_call(
            &state.memory,
            &session_id,
            &obs.tool_name,
            Some(&obs.args_summary),
            Some(&obs.result_summary),
            obs.success,
        );
    }

    Ok(serde_json::json!({
        "content": response.content,
        "tool_calls": response.tool_calls,
        "tokens_in": response.tokens_in,
        "tokens_out": response.tokens_out,
        "model": response.model,
        "reasoning": response.reasoning,
        "decisions": response.decisions,
    }))
}

/// Look up the AgentLoop slot for `session_id`. If the session has its
/// own entry in the `agents` map, return that; otherwise return the
/// shared default slot (`state.agent`). This means callers that don't
/// know about the multi-agent map keep talking to the single session
/// agent — backwards compatible.
fn agent_for_session(state: &AppState, session_id: &str) -> Arc<Mutex<Option<AgentLoop>>> {
    if let Ok(g) = state.agents.read() {
        if let Some(slot) = g.get(session_id) {
            return slot.clone();
        }
    }
    state.agent.clone()
}

/// Spawn a fresh AgentLoop for `session_id` and park it in the
/// `agents` map. Used by session_create so a second window (or HTTP
/// caller) running in parallel doesn't share conversation history
/// with the desktop window's session.
fn spawn_session_agent(state: &AppState, session_id: &str) -> Result<(), String> {
    let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?.clone();
    let registry = state.tool_registry.read().map_err(|e| format!("Registry lock: {}", e))?.clone();
    let new_agent = init_agent(&cfg, registry);
    let slot = Arc::new(Mutex::new(new_agent));
    state
        .agents
        .write()
        .map_err(|e| format!("Agents lock: {}", e))?
        .insert(session_id.to_string(), slot);
    Ok(())
}

#[tauri::command]
async fn send_message_in_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    message: String,
    model: Option<String>,
) -> Result<serde_json::Value, String> {
    // The session-scoped send_message: look up (or fail closed for) a
    // specific session's agent. Two windows hitting two different
    // sessions can now run their tool rounds in parallel because each
    // takes its own inner Mutex<Option<AgentLoop>>.
    let slot = agent_for_session(&state, &session_id);
    let mut agent_guard = slot.lock().await;
    let agent = agent_guard
        .as_mut()
        .ok_or_else(|| format!("Session {} has no live agent. Call session_create first.", session_id))?;

    let ns = format!("session:{}", session_id);
    let memories = inject::inject_memories(&state.memory, &ns, 5);
    let learned = inject::inject_learned(&state.memory, 5);

    let selected_model = if model.is_none() {
        let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
        Some(cfg.auto_route_model(&message, false).to_string())
    } else {
        model
    };

    let _ = app.emit("agent-progress", serde_json::json!({
        "type": "thinking",
        "session_id": session_id,
        "model": selected_model.as_deref().unwrap_or("auto"),
    }));

    let soul = agent::soul::load_soul();
    let response = agent
        .send_message(&message, None, &memories, &learned, selected_model.as_deref(), None, soul.as_deref())
        .await
        .map_err(|e| {
            channel::publish("error", &e);
            e
        })?;

    let cost = calculate_cost(response.tokens_in, response.tokens_out, &response.model);
    state.session_store.update_stats(&session_id, response.tokens_in as i64, response.tokens_out as i64, cost, &response.model).ok();

    for obs in &response.observations {
        observe::observe_tool_call(
            &state.memory,
            &session_id,
            &obs.tool_name,
            Some(&obs.args_summary),
            Some(&obs.result_summary),
            obs.success,
        );
    }

    Ok(serde_json::json!({
        "content": response.content,
        "tool_calls": response.tool_calls,
        "tokens_in": response.tokens_in,
        "tokens_out": response.tokens_out,
        "model": response.model,
        "session_id": session_id,
    }))
}

fn resolve_key_from_config<'a>(config: &'a Config, provider_type: &str) -> Option<&'a str> {
    match provider_type {
        "openai" => config.providers.openai.as_ref().map(|c| c.api_key.as_str()),
        "anthropic" => config.providers.anthropic.as_ref().map(|c| c.api_key.as_str()),
        "nvidia" => config.providers.nvidia.as_ref().map(|c| c.api_key.as_str()),
        "gemini" => config.providers.gemini.as_ref().map(|c| c.api_key.as_str()),
        "glm" => config.providers.glm.as_ref().map(|c| c.api_key.as_str()),
        _ => {
            // Check generic providers
            config.providers.generic.iter()
                .find(|g| g.provider_type == provider_type || g.name == provider_type)
                .map(|g| g.api_key.as_str())
        }
    }
}

fn mask_api_key(key: &mut serde_json::Value) {
    if let Some(s) = key.as_str() {
        // Real API keys are ASCII, but a hand-edited config could feed us a
        // string with multi-byte chars at the cut points; slicing by byte
        // index would panic. Walk by char so the prefix/suffix are always
        // valid UTF-8.
        let char_count = s.chars().count();
        if char_count > 8 {
            let prefix: String = s.chars().take(3).collect();
            let suffix: String = s.chars().rev().take(4).collect::<Vec<_>>()
                .into_iter().rev().collect();
            *key = serde_json::Value::String(format!("{}...{}", prefix, suffix));
        } else if !s.is_empty() {
            *key = serde_json::Value::String("...".to_string());
        }
    }
}

fn mask_api_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "api_key" {
                    mask_api_key(v);
                } else if k == "key_pool" {
                    if let serde_json::Value::Array(arr) = v {
                        for item in arr.iter_mut() {
                            mask_api_key(item);
                        }
                    }
                } else {
                    mask_api_keys(v);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                mask_api_keys(item);
            }
        }
        _ => {}
    }
}

fn is_masked(s: &str) -> bool {
    s.contains("...")
}

fn preserve_masked_keys(incoming: &mut serde_json::Value, existing: &serde_json::Value) {
    match (incoming, existing) {
        (serde_json::Value::Object(in_map), serde_json::Value::Object(ex_map)) => {
            for (k, inv) in in_map.iter_mut() {
                if k == "api_key" {
                    if let Some(s) = inv.as_str() {
                        if is_masked(s) {
                            if let Some(ex_v) = ex_map.get(k) {
                                *inv = ex_v.clone();
                            }
                        }
                    }
                } else if k == "key_pool" {
                    if let (serde_json::Value::Array(in_arr), Some(serde_json::Value::Array(ex_arr))) = (inv, ex_map.get(k)) {
                        for (i, item) in in_arr.iter_mut().enumerate() {
                            if let Some(s) = item.as_str() {
                                if is_masked(s) {
                                    if let Some(ex_item) = ex_arr.get(i) {
                                        *item = ex_item.clone();
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(ex_v) = ex_map.get(k) {
                    preserve_masked_keys(inv, ex_v);
                }
            }
        }
        (serde_json::Value::Array(in_arr), serde_json::Value::Array(ex_arr)) => {
            for (i, item) in in_arr.iter_mut().enumerate() {
                if let Some(ex_item) = ex_arr.get(i) {
                    preserve_masked_keys(item, ex_item);
                }
            }
        }
        _ => {}
    }
}

#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
    let mut value = serde_json::to_value(&*cfg).map_err(|e| format!("Serialization error: {}", e))?;
    drop(cfg);
    mask_api_keys(&mut value);
    Ok(value)
}

#[tauri::command]
async fn clear_conversation(state: State<'_, AppState>) -> Result<(), String> {
    let mut agent_guard = state.agent.lock().await;
    if let Some(agent) = agent_guard.as_mut() {
        agent.clear();
    }
    Ok(())
}

#[tauri::command]
async fn memory_add(
    state: State<'_, AppState>,
    ns: String,
    text: String,
    tier: Option<i32>,
) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().to_string();
    state.memory.add_memory(&id, &ns, tier.unwrap_or(1), &text, None)
}

#[tauri::command]
async fn memory_search(
    state: State<'_, AppState>,
    query: String,
    ns: Option<String>,
) -> Result<Vec<memory::db::MemoryRecord>, String> {
    state.memory.search_memories(ns.as_deref(), &query, 20)
}

#[tauri::command]
async fn memory_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state.memory.remove_memory(&id)
}

#[tauri::command]
async fn memory_stats(state: State<'_, AppState>) -> Result<memory::db::MemoryStats, String> {
    state.memory.memory_stats()
}

#[tauri::command]
async fn session_list(state: State<'_, AppState>) -> Result<Vec<session::store::SessionSummary>, String> {
    state.session_store.list(50)
}

#[tauri::command]
async fn session_create(state: State<'_, AppState>) -> Result<session::store::SessionSummary, String> {
    let old_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();

    // End current session
    let mut agent_guard = state.agent.lock().await;
    if let Some(agent) = agent_guard.as_mut() {
        if !agent.conversation.is_empty() {
            let messages_jsonl = serialize_conversation(&agent.conversation);
            state.session_store.end(&old_id, &messages_jsonl).ok();
        }
        agent.clear();
    }
    drop(agent_guard);

    let new_id = uuid::Uuid::new_v4().to_string();
    let (provider_name, default_model) = {
        let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
        (cfg.provider_name().to_string(), cfg.default_model().to_string())
    };
    state.session_store.create(&new_id, Some(&default_model), Some(&provider_name))?;

    // Also park a dedicated AgentLoop for the new session in the
    // multi-agent map. The desktop window itself still talks through
    // state.agent, but a parallel caller (HTTP API, a second window)
    // can address this session id directly without colliding on the
    // shared mutex.
    let _ = spawn_session_agent(&state, &new_id);

    {
        let mut sid = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?;
        *sid = new_id.clone();
    }

    state.session_store.list(1).map(|mut list| {
        list.pop().unwrap_or(session::store::SessionSummary {
            id: new_id,
            title: None,
            started_at: chrono::Utc::now().timestamp(),
            ended_at: None,
            model: Some(default_model),
            message_count: 0,
            cost: 0.0,
        })
    })
}

#[tauri::command]
async fn session_get(state: State<'_, AppState>, id: String) -> Result<Option<session::store::SessionRecord>, String> {
    state.session_store.get(&id)
}

#[tauri::command]
async fn session_search(state: State<'_, AppState>, query: String) -> Result<Vec<session::store::SessionSummary>, String> {
    state.session_store.search(&query, 20)
}

#[tauri::command]
async fn session_delete(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let current_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    if id == current_id {
        return Err("Cannot delete the active session".to_string());
    }
    state.session_store.delete(&id)
}

#[tauri::command]
async fn session_switch(state: State<'_, AppState>, id: String) -> Result<serde_json::Value, String> {
    // End current session
    let old_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();

    let mut agent_guard = state.agent.lock().await;
    let agent = agent_guard.as_mut()
        .ok_or_else(|| "Agent not initialized".to_string())?;

    if !agent.conversation.is_empty() && old_id != id {
        let messages_jsonl = serialize_conversation(&agent.conversation);
        state.session_store.end(&old_id, &messages_jsonl).ok();
    }

    // Load target session
    let session = state.session_store.get(&id)?
        .ok_or_else(|| format!("Session not found: {}", id))?;

    let messages = session.messages.as_deref()
        .map(|m| deserialize_conversation(m))
        .unwrap_or_default();

    let tokens_in = session.tokens_in as u32;
    let tokens_out = session.tokens_out as u32;
    let cost = session.cost;

    agent.set_conversation(messages.clone());
    drop(agent_guard);

    // Update session_id
    {
        let mut sid_guard = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?;
        *sid_guard = id.clone();
    }

    let messages_json: Vec<serde_json::Value> = messages.iter().map(|m| {
        serde_json::json!({
            "role": m.role,
            "content": m.content,
            "toolCalls": m.tool_calls,
            "toolCallId": m.tool_call_id,
        })
    }).collect();

    Ok(serde_json::json!({
        "id": session.id,
        "title": session.title,
        "startedAt": session.started_at,
        "endedAt": session.ended_at,
        "model": session.model,
        "tokensIn": tokens_in,
        "tokensOut": tokens_out,
        "cost": cost,
        "messages": messages_json,
    }))
}

#[tauri::command]
async fn reinforce(state: State<'_, AppState>, preference: String) -> Result<(), String> {
    reinforcement::reinforce_preference(&state.memory, &preference);
    Ok(())
}

#[tauri::command]
async fn cron_add(state: State<'_, AppState>, schedule: String, prompt: String, mode: String) -> Result<CronJob, String> {
    let job = CronJob {
        id: uuid::Uuid::new_v4().to_string(),
        schedule,
        prompt,
        mode,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
        last_run: None,
        run_count: 0,
        last_error: None,
        last_output: None,
    };
    state.cron_store.add(&job)?;
    Ok(job)
}

#[tauri::command]
async fn cron_list(state: State<'_, AppState>) -> Result<Vec<CronJob>, String> {
    state.cron_store.list()
}

#[tauri::command]
async fn cron_get(state: State<'_, AppState>, id: String) -> Result<Option<CronJob>, String> {
    state.cron_store.get(&id)
}

#[tauri::command]
async fn cron_delete(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state.cron_store.delete(&id)
}

#[tauri::command]
async fn cron_toggle(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state.cron_store.toggle(&id)
}

#[tauri::command]
async fn cron_run_now(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let job = state.cron_store.get(&id)?
        .ok_or_else(|| format!("Job not found: {}", id))?;

    let now = chrono::Utc::now().timestamp();

    // Inline async block so a failure (agent missing, send_message error,
    // script non-zero exit) becomes an Err that mark_run still records,
    // instead of `?`-returning before we touch the cron_jobs row.
    let result: Result<String, String> = if job.mode == "script" {
        execute_script_job(&job.prompt)
    } else {
        let mut agent_guard = state.agent.lock().await;
        match agent_guard.as_mut() {
            None => Err("Agent not initialized".to_string()),
            Some(agent) => {
                let soul = agent::soul::load_soul();
                match agent
                    .send_message(&job.prompt, None, &[], &[], None, None, soul.as_deref())
                    .await
                {
                    Ok(response) => Ok(response.content),
                    Err(e) => Err(format!("Agent error: {}", e)),
                }
            }
        }
    };

    match &result {
        Ok(output) => {
            state.cron_store.mark_run(&id, now, Some(output), None).ok();
        }
        Err(e) => {
            state.cron_store.mark_run(&id, now, None, Some(e)).ok();
        }
    }

    result
}

#[tauri::command]
async fn session_export(
    state: State<'_, AppState>,
    id: Option<String>,
    output_path: Option<String>,
) -> Result<String, String> {
    let session_id = if let Some(sid) = id {
        sid
    } else {
        state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone()
    };

    let session = state.session_store.get(&session_id)?
        .ok_or_else(|| format!("Session not found: {}", session_id))?;

    let messages = session.messages.unwrap_or_default();
    let output = format!(
        "# Goblin Session Export\n\
         # ID: {id}\n\
         # Title: {title}\n\
         # Model: {model:?}\n\
         # Provider: {provider:?}\n\
         # Started: {started}\n\
         # Ended: {ended:?}\n\
         # Cost: ${cost:.4}\n\
         # Tokens: {tokens_in} in / {tokens_out} out\n\n\
         {messages}",
        id = session.id,
        title = session.title.as_deref().unwrap_or("untitled"),
        model = session.model,
        provider = session.provider,
        started = session.started_at,
        ended = session.ended_at,
        cost = session.cost,
        tokens_in = session.tokens_in,
        tokens_out = session.tokens_out,
        messages = messages,
    );

    let path = output_path.unwrap_or_else(|| {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        format!("goblin_session_{}_{}.jsonl", ts, &session_id[..8])
    });

    std::fs::write(&path, &output)
        .map_err(|e| format!("Failed to write export file {}: {}", path, e))?;

    Ok(format!("Session exported to {} ({:.1} KB)", path, output.len() as f64 / 1024.0))
}

#[tauri::command]
async fn task_list(state: State<'_, AppState>) -> Result<Vec<task::TaskRecord>, String> {
    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    state.task_store.list(&session_id)
}

#[tauri::command]
async fn task_tree(state: State<'_, AppState>) -> Result<Vec<task::TaskTree>, String> {
    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    state.task_store.task_tree(&session_id)
}

#[tauri::command]
async fn task_upsert(
    state: State<'_, AppState>,
    id: String,
    name: String,
    status: String,
    result: Option<String>,
) -> Result<(), String> {
    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    state.task_store.upsert(&session_id, &id, &name, &status, result.as_deref())
}

#[tauri::command]
async fn task_clear(state: State<'_, AppState>) -> Result<usize, String> {
    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    state.task_store.clear_session(&session_id)
}

#[tauri::command]
async fn mcp_server_start(_state: State<'_, AppState>) -> Result<String, String> {
    // The Tauri desktop process owns stdin/stdout (webview IPC on some
    // platforms uses them; on a packaged macOS .app they are not even
    // attached to a tty). Trying to drive an MCP stdio server inside the
    // same process either fights the webview for those FDs or writes
    // JSON-RPC frames into the void. The correct shape is a separate
    // headless `goblin-mcp` binary that links the same tool registry,
    // but that binary doesn't exist yet, so refuse instead of pretending
    // to start.
    Err("MCP stdio server cannot run inside the desktop app — Tauri owns stdin/stdout. Build a headless goblin-mcp binary that links the same tool registry and run it standalone.".to_string())
}

#[tauri::command]
async fn save_config(
    state: State<'_, AppState>,
    config_json: serde_json::Value,
) -> Result<(), String> {
    // Masked api_key fields ("sk-...abcd") coming from the settings UI
    // must be rehydrated from the on-disk value, otherwise we'd persist
    // the mask string back to disk.
    let mut incoming = config_json;
    {
        let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
        let existing = serde_json::to_value(&*cfg).map_err(|e| format!("Serialization error: {}", e))?;
        preserve_masked_keys(&mut incoming, &existing);
    }

    let new_config: Config = serde_json::from_value(incoming)
        .map_err(|e| format!("Invalid config JSON: {}", e))?;

    // Persist to ~/.goblin/config.toml first — if disk write fails we
    // surface the error instead of mutating in-memory state.
    let config_path = Config::config_path();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir: {}", e))?;
    }
    let toml_str = toml::to_string_pretty(&new_config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    std::fs::write(&config_path, &toml_str)
        .map_err(|e| format!("Failed to write config to {:?}: {}", config_path, e))?;

    // Rebuild the agent (provider, tool registry, system prompt) against
    // the new config and swap it in atomically. Both the agent and the
    // stored config are now live without needing a restart.
    let tool_registry = Arc::new(tools::create_tool_registry(
        new_config.stt.clone(),
        new_config.tts.clone(),
        new_config.tools.clone(),
        state.task_store.clone(),
        state.whatsapp.clone(),
        state.mnemonics.clone(),
        state.plugins.clone(),
        state.mcp.clone(),
    ));
    let new_agent = init_agent(&new_config, tool_registry.clone());

    // Refresh the registry shared by every parallel agent.
    {
        let mut reg_w = state.tool_registry.write().map_err(|e| format!("Registry lock: {}", e))?;
        *reg_w = tool_registry;
    }

    let mut agent_guard = state.agent.lock().await;
    *agent_guard = new_agent;
    drop(agent_guard);

    // Refresh the live notification feed against the new config before
    // we swap the in-memory Config — otherwise an event emitted in this
    // tick would still hit the old Telegram destination.
    channel::init(new_config.channels.clone());

    let mut cfg_w = state.config.write().map_err(|e| format!("Config lock: {}", e))?;
    *cfg_w = new_config;

    Ok(())
}

#[tauri::command]
async fn test_connection(
    state: State<'_, AppState>,
    api_key: String,
    base_url: String,
    provider_type: String,
) -> Result<serde_json::Value, String> {
    // If the frontend sent a masked key (contains "..."), resolve from stored config
    let resolved_key = if api_key.contains("...") {
        let cfg = state.config.read().map_err(|e| format!("Config lock: {}", e))?;
        resolve_key_from_config(&cfg, &provider_type).unwrap_or(&api_key).to_string()
    } else {
        api_key
    };

    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let models_url = base_url.trim_end_matches('/').to_string() + "/models";
    let result = client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", resolved_key))
        .send()
        .await;

    let latency_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 200 || status == 401 {
                // 401 = key invalid but endpoint reachable. We don't need
                // the body for either branch — only the status decides
                // success vs. unauthorized.
                let ok = status == 200;
                Ok(serde_json::json!({
                    "success": ok,
                    "latencyMs": latency_ms,
                    "statusCode": status,
                    "endpointReachable": true,
                    "message": if ok {
                        "Connection successful".to_string()
                    } else {
                        "API key rejected — endpoint is reachable but key is invalid".to_string()
                    }
                }))
            } else {
                let body = resp.text().await.unwrap_or_default();
                Ok(serde_json::json!({
                    "success": false,
                    "latencyMs": latency_ms,
                    "statusCode": status,
                    "endpointReachable": true,
                    "message": format!("Unexpected status {}: {}", status, body.chars().take(200).collect::<String>())
                }))
            }
        }
        Err(e) => {
            Ok(serde_json::json!({
                "success": false,
                "latencyMs": latency_ms,
                "statusCode": 0,
                "endpointReachable": false,
                "message": format!("Connection failed: {}", e)
            }))
        }
    }
}

// ── WhatsApp Bridge Commands ──

#[tauri::command]
async fn whatsapp_start(state: State<'_, AppState>) -> Result<String, String> {
    // CARGO_MANIFEST_DIR is /Users/.../goblin/src-tauri at compile time.
    // Parent is the project root, which contains src-tauri/whatsapp-bridge.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let app_dir = manifest_dir
        .parent()
        .ok_or("Cannot resolve project root")?
        .to_str()
        .ok_or("Invalid path")?
        .to_string();
    state.whatsapp.start(&app_dir).await?;
    Ok("WhatsApp bridge started".to_string())
}

#[tauri::command]
async fn whatsapp_stop(state: State<'_, AppState>) -> Result<(), String> {
    state.whatsapp.stop().await;
    Ok(())
}

#[tauri::command]
async fn whatsapp_status(state: State<'_, AppState>) -> Result<whatsapp::BridgeStatus, String> {
    state.whatsapp.get_status().await
}

#[tauri::command]
async fn whatsapp_send(
    state: State<'_, AppState>,
    jid: String,
    text: String,
) -> Result<whatsapp::SendResult, String> {
    state.whatsapp.send_message(&jid, &text).await
}

#[tauri::command]
async fn whatsapp_poll(state: State<'_, AppState>) -> Result<Vec<whatsapp::WaMessage>, String> {
    state.whatsapp.poll_messages().await
}

// ── Wasm Plugin Commands ──

#[tauri::command]
async fn plugin_list(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.plugins.list())
}

#[tauri::command]
async fn plugin_run(
    state: State<'_, AppState>,
    name: String,
    input: String,
) -> Result<String, String> {
    let plugins = state.plugins.clone();
    tokio::task::spawn_blocking(move || plugins.run(&name, &input))
        .await
        .map_err(|e| format!("Plugin task panicked: {}", e))?
}

#[tauri::command]
async fn plugin_install(
    state: State<'_, AppState>,
    name: String,
    wasm_bytes: Vec<u8>,
) -> Result<(), String> {
    // Persist to ~/.goblin/plugins/<name>.wasm so the plugin survives a
    // restart, then register it in the live registry.
    let plugins_dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".goblin").join("plugins"))
        .ok_or("HOME not set")?;
    std::fs::create_dir_all(&plugins_dir)
        .map_err(|e| format!("Failed to create plugins dir: {}", e))?;
    let path = plugins_dir.join(format!("{}.wasm", name));
    std::fs::write(&path, &wasm_bytes)
        .map_err(|e| format!("Failed to write plugin: {}", e))?;
    state.plugins.load_bytes(name, &wasm_bytes)
}

#[tauri::command]
async fn plugin_uninstall(state: State<'_, AppState>, name: String) -> Result<bool, String> {
    let plugins_dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".goblin").join("plugins"))
        .ok_or("HOME not set")?;
    let path = plugins_dir.join(format!("{}.wasm", name));
    let removed_file = std::fs::remove_file(&path).is_ok();
    let removed_mem = state.plugins.unload(&name);
    Ok(removed_file || removed_mem)
}

async fn cron_scheduler_loop(app: tauri::AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        let state = app.state::<AppState>();
        let now = chrono::Utc::now();
        let due = state.cron_store.due_jobs(&now).unwrap_or_default();

        for job in due {
            let now_ts = now.timestamp();

            let result = if job.mode == "script" {
                execute_script_job(&job.prompt)
            } else {
                let mut agent_guard = state.agent.lock().await;
                match agent_guard.as_mut() {
                    Some(agent) => {
                        let soul = agent::soul::load_soul();
                        agent
                            .send_message(&job.prompt, None, &[], &[], None, None, soul.as_deref())
                            .await
                            .map(|r| r.content)
                            .map_err(|e| format!("Agent error: {}", e))
                    }
                    None => Err("Agent not initialized".to_string()),
                }
            };

            match &result {
                Ok(output) => {
                    state.cron_store.mark_run(&job.id, now_ts, Some(output), None).ok();
                }
                Err(e) => {
                    eprintln!("Cron job {} failed: {}", job.id, e);
                    state.cron_store.mark_run(&job.id, now_ts, None, Some(e)).ok();
                }
            }
        }
    }
}

async fn subagent_runner_loop(app: tauri::AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let state = app.state::<AppState>();
        let pending = state.task_store.list_pending().unwrap_or_default();

        for task in pending {
            eprintln!("[subagent] Picking up task {}: {}", task.id, task.name);

            // Mark as running
            state.task_store.upsert(
                &task.session_id, &task.id, &task.name, "running", None,
            ).ok();

            let prompt = task.prompt.clone().unwrap_or_else(|| task.name.clone());
            // Snapshot the config under a short read lock so the subagent
            // sees a consistent view even if save_config swaps it mid-loop.
            let cfg_snapshot = match state.config.read() {
                Ok(g) => g.clone(),
                Err(e) => {
                    eprintln!("[subagent] Config lock poisoned: {}", e);
                    continue;
                }
            };
            // Subagents reuse the shared registry instead of building
            // their own — saves dozens of Arc clones per task and means
            // shell_allowlist swaps land in subagents too.
            let tool_registry = state.tool_registry.read()
                .map(|g| g.clone())
                .unwrap_or_else(|_| Arc::new(tools::create_tool_registry(
                    cfg_snapshot.stt.clone(),
                    cfg_snapshot.tts.clone(),
                    cfg_snapshot.tools.clone(),
                    state.task_store.clone(),
                    state.whatsapp.clone(),
                    state.mnemonics.clone(),
                    state.plugins.clone(),
                    state.mcp.clone(),
                )));

            if let Some(mut sub_agent) = init_agent(&cfg_snapshot, tool_registry) {
                let soul = agent::soul::load_soul();
                match sub_agent.send_message(&prompt, None, &[], &[], None, None, soul.as_deref()).await {
                    Ok(response) => {
                        state.task_store.upsert(
                            &task.session_id, &task.id, &task.name, "done",
                            Some(&response.content),
                        ).ok();
                        eprintln!("[subagent] Task {} completed", task.id);
                    }
                    Err(e) => {
                        state.task_store.upsert(
                            &task.session_id, &task.id, &task.name, "error",
                            Some(&e),
                        ).ok();
                        eprintln!("[subagent] Task {} failed: {}", task.id, e);
                    }
                }
            } else {
                state.task_store.upsert(
                    &task.session_id, &task.id, &task.name, "error",
                    Some("No provider configured"),
                ).ok();
            }
        }
    }
}

fn init_agent(config: &Config, tool_registry: Arc<ToolRegistry>) -> Option<AgentLoop> {
    let max_tokens = config.agent.max_tokens;
    let provider: Box<dyn provider::Provider> = if let Some(openai_cfg) = &config.providers.openai {
        Box::new(OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
            max_tokens,
        })
    } else if let Some(anthro_cfg) = &config.providers.anthropic {
        Box::new(AnthropicProvider {
            api_key: anthro_cfg.api_key.clone(),
            base_url: anthro_cfg.base_url.clone(),
            max_tokens,
        })
    } else if let Some(nvidia_cfg) = &config.providers.nvidia {
        Box::new(NvidiaProvider {
            api_key: nvidia_cfg.api_key.clone(),
            base_url: nvidia_cfg.base_url.clone(),
            max_tokens,
        })
    } else if let Some(gemini_cfg) = &config.providers.gemini {
        Box::new(GeminiProvider {
            api_key: gemini_cfg.api_key.clone(),
            base_url: gemini_cfg.base_url.clone(),
            max_tokens,
        })
    } else if let Some(glm_cfg) = &config.providers.glm {
        Box::new(GlmProvider {
            api_key: glm_cfg.api_key.clone(),
            base_url: glm_cfg.base_url.clone(),
            max_tokens,
        })
    } else if let Some(generic) = config.providers.generic.first() {
        if generic.provider_type == "anthropic" {
            Box::new(AnthropicProvider {
                api_key: generic.api_key.clone(),
                base_url: generic.base_url.clone(),
                max_tokens,
            })
        } else {
            Box::new(OpenAIProvider {
                api_key: generic.api_key.clone(),
                base_url: generic.base_url.clone(),
                max_tokens,
            })
        }
    } else {
        return None;
    };

    Some(AgentLoop::new(config.clone(), provider, tool_registry))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Global panic hook — log to file before crash
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("[GOBLIN PANIC] {}\n{:?}", info, std::backtrace::Backtrace::force_capture());
        eprintln!("{}", msg);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/goblin-panic.log") {
            use std::io::Write;
            let _ = writeln!(f, "{}", msg);
        }
    }));

    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("Config load warning: {}", e);
        Config {
            providers: crate::config::ProvidersConfig {
                openai: None,
                anthropic: None,
                nvidia: None,
                gemini: None,
                glm: None,
                generic: vec![],
                auto_route: crate::config::AutoRouteConfig::default(),
                multi_agent: crate::config::MultiAgentConfig::default(),
            },
            agent: crate::config::AgentConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            memory: crate::config::MemoryConfig::default(),
            stt: crate::config::SttConfig::default(),
            tts: crate::config::TtsConfig::default(),
            mnemonics: crate::config::MnemonicsConfig::default(),
            mcp: crate::config::McpConfig::default(),
            channels: crate::config::ChannelsConfig::default(),
            http: crate::config::HttpConfig::default(),
        }
    });

    let db_path = MemoryDb::default_path();
    let mut memory = MemoryDb::open(db_path.to_str().unwrap_or("memory.db"))
        .unwrap_or_else(|e| {
            eprintln!("Failed to open memory db: {}", e);
            std::process::exit(1);
        });

    if let Err(e) = memory.init_schema() {
        eprintln!("Failed to init memory schema: {}", e);
        std::process::exit(1);
    }

    // Configure embedding client if enabled
    if config.memory.embedding.enabled {
        let emb_api_key = config.memory.embedding.api_key.as_deref()
            .or_else(|| config.providers.openai.as_ref().map(|c| c.api_key.as_str()))
            .unwrap_or("");

        if !emb_api_key.is_empty() {
            let emb_client = memory::embed::EmbeddingClient {
                api_key: emb_api_key.to_string(),
                base_url: config.memory.embedding.base_url.clone(),
                model: config.memory.embedding.model.clone(),
            };
            memory.set_embedding(emb_client);
            println!("[memory] Embedding enabled: {} @ {}", config.memory.embedding.model, config.memory.embedding.base_url);
        } else {
            eprintln!("[memory] Embedding enabled but no API key found. Semantic search disabled.");
        }
    }

    let session_store = Arc::new({
        let conn = rusqlite::Connection::open(db_path.to_str().unwrap_or("memory.db"))
            .unwrap_or_else(|e| {
                eprintln!("Failed to open session db: {}", e);
                std::process::exit(1);
            });
        let store = SessionStore::new(conn);
        if let Err(e) = store.init_schema() {
            eprintln!("Failed to init session schema: {}", e);
            std::process::exit(1);
        }
        store
    });

    let cron_store = {
        let conn = rusqlite::Connection::open(db_path.to_str().unwrap_or("memory.db"))
            .unwrap_or_else(|e| {
                eprintln!("Failed to open cron db: {}", e);
                std::process::exit(1);
            });
        let store = CronStore::new(conn);
        if let Err(e) = store.init_schema() {
            eprintln!("Failed to init cron schema: {}", e);
            std::process::exit(1);
        }
        store
    };

    let task_store = {
        let conn = rusqlite::Connection::open(db_path.to_str().unwrap_or("memory.db"))
            .unwrap_or_else(|e| {
                eprintln!("Failed to open task db: {}", e);
                std::process::exit(1);
            });
        let store = TaskStore::new(conn);
        if let Err(e) = store.init_schema() {
            eprintln!("Failed to init task schema: {}", e);
            std::process::exit(1);
        }
        store
    };

    let whatsapp_bridge = Arc::new(WhatsappBridge::new());

    // mnemonics binary may or may not be installed; probe once at boot so we
    // can decide whether to expose the tools.
    let mnemonics_client: Option<Arc<MnemonicsClient>> = if config.mnemonics.enabled {
        let client = MnemonicsClient::new(
            config.mnemonics.binary.clone(),
            config.mnemonics.default_ns.clone(),
        );
        if client.is_available() {
            println!("[mnemonics] Enabled via '{}' (ns='{}').", config.mnemonics.binary, config.mnemonics.default_ns);
            Some(Arc::new(client))
        } else {
            eprintln!("[mnemonics] Configured but '{}' is not runnable; skipping.", config.mnemonics.binary);
            None
        }
    } else {
        None
    };

    // Wasm plugin registry: load anything in ~/.goblin/plugins/ on boot.
    // Failures are logged inside load_dir; one broken plugin does not
    // disable the rest.
    let plugin_registry = Arc::new(
        PluginRegistry::new().unwrap_or_else(|e| {
            eprintln!("[plugin] init failed: {} — plugins disabled.", e);
            // Construction failure should never happen short of an OOM; if
            // it does we still want a no-op registry rather than crashing
            // the whole app.
            PluginRegistry::new().expect("plugin registry init failed twice")
        })
    );
    {
        let plugins_dir = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".goblin").join("plugins"))
            .unwrap_or_else(|| std::path::PathBuf::from(".goblin/plugins"));
        let loaded = plugin_registry.load_dir(&plugins_dir);
        if !loaded.is_empty() {
            println!("[plugin] Loaded {} plugin(s) from {:?}: {}", loaded.len(), plugins_dir, loaded.join(", "));
        }
    }

    // Auto-boot every server in [mcp.servers.*]. Failures inside
    // boot_from_config are logged and skipped so a single broken server
    // does not kill the agent.
    let mcp_hub = Arc::new(McpHub::boot_from_config(config.mcp.servers.clone().into_iter()));

    let tool_registry = tools::create_tool_registry(
        config.stt.clone(),
        config.tts.clone(),
        config.tools.clone(),
        task_store.clone(),
        whatsapp_bridge.clone(),
        mnemonics_client.clone(),
        plugin_registry.clone(),
        mcp_hub.clone(),
    );
    let tool_registry = Arc::new(tool_registry);
    let agent = init_agent(&config, tool_registry.clone());

    // Wire the outbound notification feed (Telegram, future channels).
    // Idempotent — save_config calls init() again with the new config.
    channel::init(config.channels.clone());

    let session_id = uuid::Uuid::new_v4().to_string();
    let provider_name = config.provider_name().to_string();
    let default_model = config.default_model().to_string();
    if let Err(e) = session_store.create(&session_id, Some(&default_model), Some(&provider_name)) {
        eprintln!("Failed to create initial session: {}", e);
    }

    compact::compact_if_needed(&memory, config.memory.auto_compact_days as i32);

    let agent_was_none = agent.is_none();

    // Build the shared Arcs that both Tauri AppState and the HTTP API
    // (if enabled) hold a reference to. Storing them as Arc means a
    // call coming through `POST /message` and one coming through the
    // desktop send_message command operate on the exact same agent
    // and session id — no shadow state, no drift.
    let agent_arc = Arc::new(Mutex::new(agent));
    let config_arc = Arc::new(std::sync::RwLock::new(config));
    let session_id_arc = Arc::new(StdMutex::new(session_id));
    let memory_arc = Arc::new(memory);
    let tool_registry_arc = Arc::new(std::sync::RwLock::new(tool_registry));

    // Seed the multi-agent map with the bootstrap session so parallel
    // sessions added later don't accidentally shadow it.
    let agents_map: std::collections::HashMap<String, Arc<Mutex<Option<AgentLoop>>>> = {
        let mut m = std::collections::HashMap::new();
        let sid = session_id_arc.lock().map(|g| g.clone()).unwrap_or_default();
        m.insert(sid, agent_arc.clone());
        m
    };
    let agents_arc = Arc::new(std::sync::RwLock::new(agents_map));

    // Spawn HTTP API server if enabled in config.
    {
        let http_cfg = config_arc.read().map(|c| c.http.clone()).unwrap_or_default();
        if http_cfg.enabled {
            let http_state = http::HttpState {
                agent: agent_arc.clone(),
                config: config_arc.clone(),
                memory: memory_arc.clone(),
                session_id: session_id_arc.clone(),
                session_store: session_store.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = http::serve(http_state, http_cfg).await {
                    eprintln!("[http] server error: {}", e);
                }
            });
        }
    }

    if agent_was_none {
        eprintln!(
            "No provider configured. Create ~/.goblin/config.toml with your API keys."
        );
        eprintln!("Example:");
        eprintln!("[providers.openai]");
        eprintln!("api_key = \"sk-your-deepseek-key\"");
        eprintln!("base_url = \"https://api.deepseek.com/v1\"");
        eprintln!("models = [\"deepseek-v4-flash\", \"deepseek-v4-pro\"]");
    } else {
        println!("Agent initialized with provider: {}", provider_name);
        let sid_for_log = session_id_arc.lock().map(|g| g.clone()).unwrap_or_default();
        println!("Session: {}", sid_for_log);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            agent: agent_arc,
            agents: agents_arc,
            tool_registry: tool_registry_arc,
            config: config_arc,
            memory: memory_arc,
            session_id: session_id_arc,
            session_store,
            cron_store,
            task_store,
            whatsapp: whatsapp_bridge,
            mnemonics: mnemonics_client,
            plugins: plugin_registry,
            mcp: mcp_hub,
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            send_message_in_session,
            get_config,
            clear_conversation,
            memory_add,
            memory_search,
            memory_remove,
            memory_stats,
            session_list,
            session_create,
            session_get,
            session_search,
            session_delete,
            session_switch,
            reinforce,
            cron_add,
            cron_list,
            cron_get,
            cron_delete,
            cron_toggle,
            cron_run_now,
            session_export,
            task_list,
            task_tree,
            task_upsert,
            task_clear,
            mcp_server_start,
            save_config,
            test_connection,
            whatsapp_start,
            whatsapp_stop,
            whatsapp_status,
            whatsapp_send,
            whatsapp_poll,
            plugin_list,
            plugin_run,
            plugin_install,
            plugin_uninstall,
        ])
        .setup(|app| {
            // ── System tray daemon ──
            daemon::create_tray_icon(app.handle())?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                cron_scheduler_loop(handle).await;
            });
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                subagent_runner_loop(handle2).await;
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::WindowEvent { label, event: window_event, .. } = event {
                if label == "main" {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = window_event {
                        // Minimize to system tray instead of quitting
                        api.prevent_close();
                        let _ = app_handle.get_webview_window("main").map(|w| w.hide());
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::Message;

    #[test]
    fn calculate_cost_deepseek() {
        let cost = calculate_cost(1_000_000, 1_000_000, "deepseek-v4-pro");
        // in: 0.28, out: 1.10
        assert!((cost - 1.38).abs() < 0.01);
    }

    #[test]
    fn calculate_cost_gpt4() {
        let cost = calculate_cost(1_000_000, 1_000_000, "gpt-4-turbo");
        // in: 3.0, out: 15.0
        assert!((cost - 18.0).abs() < 0.01);
    }

    #[test]
    fn calculate_cost_claude() {
        let cost = calculate_cost(1_000_000, 1_000_000, "claude-3-opus");
        assert!((cost - 18.0).abs() < 0.01);
    }

    #[test]
    fn calculate_cost_unknown_falls_to_deepseek() {
        let cost = calculate_cost(1_000_000, 1_000_000, "unknown-model");
        assert!((cost - 1.38).abs() < 0.01);
    }

    #[test]
    fn calculate_cost_zero_tokens() {
        let cost = calculate_cost(0, 0, "deepseek-v4-flash");
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let msgs = vec![
            Message { role: "system".into(), content: "sys prompt".into(), tool_calls: None, tool_call_id: None, reasoning: None },
            Message { role: "user".into(), content: "hello".into(), tool_calls: None, tool_call_id: None, reasoning: None },
        ];
        let jsonl = serialize_conversation(&msgs);
        assert!(jsonl.contains("sys prompt"));
        assert!(jsonl.contains("hello"));

        let restored = deserialize_conversation(&jsonl);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].role, "system");
        assert_eq!(restored[1].content, "hello");
    }

    #[test]
    fn deserialize_empty_yields_empty() {
        let result = deserialize_conversation("");
        assert!(result.is_empty());
    }

    #[test]
    fn deserialize_bad_lines_are_filtered() {
        let result = deserialize_conversation("not valid json\n{\"role\":\"user\",\"content\":\"ok\"}");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "ok");
    }

    // ── E2E: Task Persistence ──

    #[test]
    fn task_persistence_across_store_lifetime() {
        let db_path = std::path::PathBuf::from("/tmp/goblin-e2e-task-persist.db");
        let _ = std::fs::remove_file(&db_path);

        // Create tasks
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let store = TaskStore::new(conn);
            store.init_schema().unwrap();

            store.upsert("sid-abc", "t1", "read file", "done", Some("content")).unwrap();
            store.upsert("sid-abc", "t2", "write file", "running", None).unwrap();
            store.upsert("sid-xyz", "t3", "search", "pending", None).unwrap();
        }

        // Reopen and verify
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let store = TaskStore::new(conn);

            let list = store.list("sid-abc").unwrap();
            assert_eq!(list.len(), 2);
            assert_eq!(list[0].name, "read file");
            assert_eq!(list[0].status, "done");
            assert_eq!(list[0].result.as_deref(), Some("content"));
            assert_eq!(list[1].name, "write file");
            assert_eq!(list[1].status, "running");

            let other = store.list("sid-xyz").unwrap();
            assert_eq!(other.len(), 1);
            assert_eq!(other[0].status, "pending");
        }

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn task_clear_session_removes_only_that_session() {
        let store = TaskStore::new_in_memory().unwrap();

        store.upsert("s1", "a", "task a", "done", None).unwrap();
        store.upsert("s1", "b", "task b", "pending", None).unwrap();
        store.upsert("s2", "c", "task c", "pending", None).unwrap();

        let cleared = store.clear_session("s1").unwrap();
        assert_eq!(cleared, 2);
        assert_eq!(store.list("s1").unwrap().len(), 0);
        assert_eq!(store.list("s2").unwrap().len(), 1);
    }

    // ── E2E: Sub-agent with Mock Provider ──

    struct MockProvider {
        canned_response: String,
    }

    #[async_trait::async_trait]
    impl provider::Provider for MockProvider {
        async fn chat(
            &self,
            _messages: &[provider::Message],
            _tools: &[provider::ToolDefinition],
            _model: &str,
        ) -> Result<provider::ProviderResponse, String> {
            Ok(provider::ProviderResponse {
                content: Some(self.canned_response.clone()),
                tool_calls: None,
                tokens_in: 5,
                tokens_out: 3,
                model: "mock".to_string(),
                reasoning: None,
            })
        }
    }

    #[tokio::test]
    async fn subagent_executes_pending_task_with_mock_provider() {
        let store = TaskStore::new_in_memory().unwrap();

        // Create a pending delegated task
        store.upsert_with_prompt(
            "sid-test",
            "task-sub-1",
            "Analyze code",
            "pending",
            Some("Analyze this code for bugs"),
            None,
        ).unwrap();

        // Verify it's pending
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "task-sub-1");
        assert_eq!(pending[0].status, "pending");

        // Simulate sub-agent picking it up: mark running, execute, store result
        store.upsert("sid-test", "task-sub-1", "Analyze code", "running", None).unwrap();

        let mock = MockProvider {
            canned_response: "No bugs found. Code is clean.".to_string(),
        };

        let config = Config {
            providers: crate::config::ProvidersConfig {
                openai: None, anthropic: None, nvidia: None, gemini: None, glm: None,
                generic: vec![],
                auto_route: crate::config::AutoRouteConfig::default(),
                multi_agent: crate::config::MultiAgentConfig::default(),
            },
            agent: crate::config::AgentConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            memory: crate::config::MemoryConfig::default(),
            stt: crate::config::SttConfig::default(),
            tts: crate::config::TtsConfig::default(),
            mnemonics: crate::config::MnemonicsConfig::default(),
            mcp: crate::config::McpConfig::default(),
            channels: crate::config::ChannelsConfig::default(),
            http: crate::config::HttpConfig::default(),
        };

        let tool_registry = tools::create_tool_registry(
            config.stt.clone(), config.tts.clone(), config.tools.clone(), store.clone(), Arc::new(WhatsappBridge::new()), None, Arc::new(plugin::PluginRegistry::new().unwrap()), Arc::new(mcp::McpHub::new()),
        );

        let mut agent = AgentLoop::new(config, Box::new(mock), Arc::new(tool_registry));
        let result = agent
            .send_message("Analyze this code for bugs", None, &[], &[], None, None, None)
            .await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.content.contains("No bugs found"));

        // Store result
        store.upsert("sid-test", "task-sub-1", "Analyze code", "done", Some(&response.content)).unwrap();

        // Verify task completed
        let tasks = store.list("sid-test").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, "done");
        assert!(tasks[0].result.as_ref().unwrap().contains("No bugs found"));
    }

    #[tokio::test]
    async fn subagent_handles_failure_gracefully() {
        let store = TaskStore::new_in_memory().unwrap();

        store.upsert_with_prompt(
            "sid-test",
            "task-fail-1",
            "Failing task",
            "pending",
            Some("This will fail"),
            None,
        ).unwrap();

        store.upsert("sid-test", "task-fail-1", "Failing task", "running", None).unwrap();

        // Store error
        store.upsert(
            "sid-test", "task-fail-1", "Failing task", "error",
            Some("LLM connection timeout"),
        ).unwrap();

        let tasks = store.list("sid-test").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, "error");
        assert_eq!(tasks[0].result.as_deref(), Some("LLM connection timeout"));
    }

    #[tokio::test]
    async fn subagent_multiple_pending_tasks_executed_in_order() {
        let store = TaskStore::new_in_memory().unwrap();

        store.upsert_with_prompt("sid", "a", "Task A", "pending", Some("Do A"), None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.upsert_with_prompt("sid", "b", "Task B", "pending", Some("Do B"), None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.upsert_with_prompt("sid", "c", "Task C", "pending", Some("Do C"), None).unwrap();

        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 3);

        // Execute each in order
        for (i, task) in pending.iter().enumerate() {
            store.upsert(&task.session_id, &task.id, &task.name, "running", None).unwrap();

            let mock = MockProvider {
                canned_response: format!("Completed {}", task.name),
            };

            let config = Config {
                providers: crate::config::ProvidersConfig {
                    openai: None, anthropic: None, nvidia: None, gemini: None, glm: None,
                    generic: vec![],
                    auto_route: crate::config::AutoRouteConfig::default(),
                    multi_agent: crate::config::MultiAgentConfig::default(),
                },
                agent: crate::config::AgentConfig::default(),
                tools: crate::config::ToolsConfig::default(),
                memory: crate::config::MemoryConfig::default(),
                stt: crate::config::SttConfig::default(),
                tts: crate::config::TtsConfig::default(),
                mnemonics: crate::config::MnemonicsConfig::default(),
                mcp: crate::config::McpConfig::default(),
                channels: crate::config::ChannelsConfig::default(),
            http: crate::config::HttpConfig::default(),
            };

            let tool_registry = tools::create_tool_registry(
                config.stt.clone(), config.tts.clone(), config.tools.clone(), store.clone(), Arc::new(WhatsappBridge::new()), None, Arc::new(plugin::PluginRegistry::new().unwrap()), Arc::new(mcp::McpHub::new()),
            );

            let mut agent = AgentLoop::new(config, Box::new(mock), Arc::new(tool_registry));
            let prompt = task.prompt.clone().unwrap_or_default();
            let result = agent.send_message(&prompt, None, &[], &[], None, None, None).await.unwrap();

            store.upsert(&task.session_id, &task.id, &task.name, "done", Some(&result.content)).unwrap();

            eprintln!("[E2E test] Task {}/{} done: {}", i + 1, pending.len(), task.name);
        }

        let all = store.list("sid").unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|t| t.status == "done"));
    }

    // ── E2E: delegate_task tool integration ──

    #[tokio::test]
    async fn delegate_task_tool_creates_persisted_task() {
        let store = TaskStore::new_in_memory().unwrap();

        let result = tools::meta::handle_delegate_task(
            serde_json::json!({
                "description": "Fix bug #42",
                "prompt": "Find and fix the null pointer in user.rs:142",
                "agentType": "explore",
                "sessionId": "fixed-session",
            }),
            &store,
        ).await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("QUEUED"));
        assert!(output.contains("Fix bug #42"));
        assert!(output.contains("user.rs:142"));

        let tasks = store.list("fixed-session").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "Fix bug #42");
        assert_eq!(tasks[0].status, "pending");
        assert_eq!(tasks[0].prompt.as_deref(), Some("Find and fix the null pointer in user.rs:142"));
    }
}
