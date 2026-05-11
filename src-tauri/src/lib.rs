mod agent;
mod config;
mod cron;
mod memory;
mod provider;
mod session;
mod tools;

use agent::r#loop::AgentLoop;
use crate::config::Config;
use provider::openai::OpenAIProvider;
use memory::{MemoryDb, inject, compact, observe, reinforcement};
use session::SessionStore;
use tools::ToolRegistry;
use tokio::sync::Mutex;
use std::sync::Mutex as StdMutex;
use tauri::State;

struct AppState {
    agent: Mutex<Option<AgentLoop>>,
    config: Config,
    memory: MemoryDb,
    session_id: StdMutex<String>,
    session_store: SessionStore,
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
    state: State<'_, AppState>,
    message: String,
    model: Option<String>,
) -> Result<serde_json::Value, String> {
    let mut agent_guard = state.agent.lock().await;

    let agent = agent_guard
        .as_mut()
        .ok_or_else(|| "Agent not initialized. Configure a provider in ~/.goblin/config.toml".to_string())?;

    let session_id = state.session_id.lock().map_err(|e| format!("Lock error: {}", e))?.clone();
    let ns = format!("session:{}", session_id);
    let memories = inject::inject_memories(&state.memory, &ns, 5);
    let learned = inject::inject_learned(&state.memory, 5);

    let response = agent
        .send_message(&message, None, &memories, &learned, model.as_deref())
        .await
        .map_err(|e| format!("Agent error: {}", e))?;

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
    }))
}

#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    serde_json::to_value(&state.config).map_err(|e| format!("Serialization error: {}", e))
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
    let provider_name = state.config.provider_name().to_string();
    let default_model = state.config.default_model().to_string();
    state.session_store.create(&new_id, Some(&default_model), Some(&provider_name))?;

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

fn init_agent(config: &Config, tool_registry: ToolRegistry) -> Option<AgentLoop> {
    if let Some(openai_cfg) = &config.providers.openai {
        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
        };
        let agent = AgentLoop::new(config.clone(), Box::new(provider), tool_registry);
        Some(agent)
    } else if config.providers.anthropic.is_some() {
        eprintln!("Anthropic provider not yet implemented");
        None
    } else if config.providers.nvidia.is_some() {
        eprintln!("NVIDIA provider not yet implemented");
        None
    } else {
        None
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("Config load warning: {}", e);
        Config {
            providers: crate::config::ProvidersConfig {
                openai: None,
                anthropic: None,
                nvidia: None,
                auto_route: crate::config::AutoRouteConfig::default(),
            },
            agent: crate::config::AgentConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            memory: crate::config::MemoryConfig::default(),
        }
    });

    let tool_registry = tools::create_tool_registry();
    let agent = init_agent(&config, tool_registry);

    let db_path = MemoryDb::default_path();
    let memory = MemoryDb::open(db_path.to_str().unwrap_or("memory.db"))
        .unwrap_or_else(|e| {
            eprintln!("Failed to open memory db: {}", e);
            std::process::exit(1);
        });

    if let Err(e) = memory.init_schema() {
        eprintln!("Failed to init memory schema: {}", e);
        std::process::exit(1);
    }

    let session_store = {
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
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let provider_name = config.provider_name().to_string();
    let default_model = config.default_model().to_string();
    if let Err(e) = session_store.create(&session_id, Some(&default_model), Some(&provider_name)) {
        eprintln!("Failed to create initial session: {}", e);
    }

    compact::compact_if_needed(&memory, config.memory.auto_compact_days as i32);

    if agent.is_none() {
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
        println!("Session: {}", session_id);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            agent: Mutex::new(agent),
            config,
            memory,
            session_id: StdMutex::new(session_id),
            session_store,
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
