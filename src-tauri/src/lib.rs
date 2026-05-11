mod agent;
mod config;
mod cron;
mod memory;
mod provider;
mod session;
mod tools;

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
use tools::ToolRegistry;
use tokio::sync::Mutex;
use std::sync::Mutex as StdMutex;
use tauri::State;
use tauri::Manager;

struct AppState {
    agent: Mutex<Option<AgentLoop>>,
    config: Config,
    memory: MemoryDb,
    session_id: StdMutex<String>,
    session_store: SessionStore,
    cron_store: CronStore,
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

    let selected_model = if model.is_none() {
        let auto = state.config.auto_route_model(&message, false);
        Some(auto.to_string())
    } else {
        model
    };

    let response = agent
        .send_message(&message, None, &memories, &learned, selected_model.as_deref())
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

    let result = if job.mode == "script" {
        execute_script_job(&job.prompt)
    } else {
        let mut agent_guard = state.agent.lock().await;
        let agent = agent_guard
            .as_mut()
            .ok_or_else(|| "Agent not initialized".to_string())?;

        let response = agent
            .send_message(&job.prompt, None, &[], &[], None)
            .await
            .map_err(|e| format!("Agent error: {}", e))?;
        Ok(response.content)
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
                        agent
                            .send_message(&job.prompt, None, &[], &[], None)
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

fn init_agent(config: &Config, tool_registry: ToolRegistry) -> Option<AgentLoop> {
    let provider: Box<dyn provider::Provider> = if let Some(openai_cfg) = &config.providers.openai {
        Box::new(OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
        })
    } else if let Some(anthro_cfg) = &config.providers.anthropic {
        Box::new(AnthropicProvider {
            api_key: anthro_cfg.api_key.clone(),
            base_url: anthro_cfg.base_url.clone(),
        })
    } else if let Some(nvidia_cfg) = &config.providers.nvidia {
        Box::new(NvidiaProvider {
            api_key: nvidia_cfg.api_key.clone(),
            base_url: nvidia_cfg.base_url.clone(),
        })
    } else if let Some(gemini_cfg) = &config.providers.gemini {
        Box::new(GeminiProvider {
            api_key: gemini_cfg.api_key.clone(),
            base_url: gemini_cfg.base_url.clone(),
        })
    } else if let Some(glm_cfg) = &config.providers.glm {
        Box::new(GlmProvider {
            api_key: glm_cfg.api_key.clone(),
            base_url: glm_cfg.base_url.clone(),
        })
    } else if let Some(generic) = config.providers.generic.first() {
        if generic.provider_type == "anthropic" {
            Box::new(AnthropicProvider {
                api_key: generic.api_key.clone(),
                base_url: generic.base_url.clone(),
            })
        } else {
            Box::new(OpenAIProvider {
                api_key: generic.api_key.clone(),
                base_url: generic.base_url.clone(),
            })
        }
    } else {
        return None;
    };

    Some(AgentLoop::new(config.clone(), provider, tool_registry))
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
                gemini: None,
                glm: None,
                generic: vec![],
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
        .setup(|app| {
            let handle = app.handle().clone();
            tokio::spawn(async move {
                cron_scheduler_loop(handle).await;
            });
            Ok(())
        })
        .manage(AppState {
            agent: Mutex::new(agent),
            config,
            memory,
            session_id: StdMutex::new(session_id),
            session_store,
            cron_store,
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
            cron_add,
            cron_list,
            cron_get,
            cron_delete,
            cron_toggle,
            cron_run_now,
            session_export,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
            Message { role: "system".into(), content: "sys prompt".into(), tool_calls: None, tool_call_id: None },
            Message { role: "user".into(), content: "hello".into(), tool_calls: None, tool_call_id: None },
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
}
