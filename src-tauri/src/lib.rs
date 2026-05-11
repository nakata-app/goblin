use agent::r#loop::AgentLoop;
use config::Config;
use provider::openai::OpenAIProvider;
use tokio::sync::Mutex;
use tauri::State;

struct AppState {
    agent: Mutex<Option<AgentLoop>>,
    config: Config,
}

#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    message: String,
) -> Result<serde_json::Value, String> {
    let mut agent_guard = state.agent.lock().await;

    let agent = agent_guard
        .as_mut()
        .ok_or_else(|| "Agent not initialized. Configure a provider in ~/.goblin/config.toml".to_string())?;

    let memories: Vec<String> = Vec::new();
    let learned: Vec<String> = Vec::new();

    let response = agent
        .send_message(&message, None, &memories, &learned)
        .await
        .map_err(|e| format!("Agent error: {}", e))?;

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

fn init_agent(config: &Config) -> Option<AgentLoop> {
    if let Some(openai_cfg) = &config.providers.openai {
        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
        };
        let mut agent = AgentLoop::new(config.clone(), Box::new(provider));
        agent.set_tools(tools::get_tools());
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
            providers: config::ProvidersConfig {
                openai: None,
                anthropic: None,
                nvidia: None,
                auto_route: config::AutoRouteConfig::default(),
            },
            agent: config::AgentConfig::default(),
            tools: config::ToolsConfig::default(),
            memory: config::MemoryConfig::default(),
        }
    });

    let provider_name = config.provider_name().to_string();
    let agent = init_agent(&config);

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
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            agent: Mutex::new(agent),
            config,
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            get_config,
            clear_conversation,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
