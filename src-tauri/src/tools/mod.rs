pub mod file_ops;
pub mod search;
pub mod shell;
pub mod web;
pub mod browser;
pub mod git;
pub mod media;
pub mod meta;
pub mod vault;
pub mod mcp;
pub mod mcp_server;
pub mod skills;
pub mod peer;
pub mod compactor;
pub mod sandbox;

use crate::config::SttConfig;
use crate::config::TtsConfig;
use crate::task::TaskStore;

use crate::provider::ToolDefinition;
use std::collections::HashMap;
use std::pin::Pin;
use std::future::Future;

type AsyncToolResult = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

type ToolHandler = Box<dyn Fn(serde_json::Value) -> AsyncToolResult + Send + Sync>;

pub struct ToolRegistry {
    definitions: Vec<ToolDefinition>,
    handlers: HashMap<String, ToolHandler>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            definitions: Vec::new(),
            handlers: HashMap::new(),
        }
    }

    pub fn register<F, Fut>(&mut self, def: ToolDefinition, handler: F)
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        let name = def.function.name.clone();
        self.definitions.push(def);
        self.handlers.insert(
            name,
            Box::new(move |args| Box::pin(handler(args))),
        );
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        let handler = self
            .handlers
            .get(name)
            .ok_or_else(|| format!("Unknown tool: {}", name))?;
        handler(args).await
    }

    #[allow(dead_code)]
    pub fn names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }
}

pub fn create_tool_registry(stt: SttConfig, tts: TtsConfig, task_store: TaskStore) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    let stt_api_key = stt.api_key.clone();
    let stt_base_url = if stt.provider == "none" || stt.provider.is_empty() {
        None
    } else {
        Some(stt.base_url.clone())
    };

    let tts_provider = tts.provider.clone();
    let tts_api_key = tts.api_key.clone();
    let tts_base_url = tts.base_url.clone();
    let tts_model = tts.model.clone();
    let tts_voice = tts.voice.clone();

    registry.register(file_ops::read_file_def(), file_ops::handle_read_file);
    registry.register(file_ops::write_file_def(), file_ops::handle_write_file);
    registry.register(file_ops::edit_file_def(), file_ops::handle_edit_file);
    registry.register(file_ops::multi_edit_def(), file_ops::handle_multi_edit);
    registry.register(search::grep_def(), search::handle_grep);
    registry.register(search::glob_def(), search::handle_glob);
    registry.register(shell::bash_def(), shell::handle_bash);
    registry.register(shell::bash_background_def(), shell::handle_bash_background);
    registry.register(shell::bash_background_check_def(), shell::handle_bash_background_check);
    registry.register(shell::bash_background_kill_def(), shell::handle_bash_background_kill);
    registry.register(web::web_fetch_def(), web::handle_web_fetch);
    registry.register(web::web_search_def(), web::handle_web_search);
    registry.register(browser::browser_navigate_def(), browser::handle_browser_navigate);
    registry.register(browser::browser_click_def(), browser::handle_browser_click);
    registry.register(browser::browser_type_def(), browser::handle_browser_type);
    registry.register(browser::browser_scroll_def(), browser::handle_browser_scroll);
    registry.register(browser::browser_snapshot_def(), browser::handle_browser_snapshot);
    registry.register(browser::browser_press_def(), browser::handle_browser_press);
    registry.register(browser::browser_vision_def(), browser::handle_browser_vision);
    registry.register(browser::browser_console_def(), browser::handle_browser_console);

    // Faz 9: Git tools
    registry.register(git::git_status_def(), git::handle_git_status);
    registry.register(git::git_diff_def(), git::handle_git_diff);
    registry.register(git::git_commit_def(), git::handle_git_commit);
    registry.register(git::git_log_def(), git::handle_git_log);
    registry.register(git::git_pr_create_def(), git::handle_git_pr_create);

    // Faz 9: Media tools
    registry.register(media::vision_analyze_def(), media::handle_vision_analyze);

    // TTS tool with multi-provider config
    {
        let provider = tts_provider.clone();
        let api_key = tts_api_key.clone();
        let base_url = tts_base_url.clone();
        let model = tts_model.clone();
        let voice = tts_voice.clone();
        registry.register(media::text_to_speech_def(), move |args| {
            let provider = provider.clone();
            let api_key = api_key.clone();
            let base_url = base_url.clone();
            let model = model.clone();
            let voice = voice.clone();
            Box::pin(async move {
                media::handle_text_to_speech(args, &provider, api_key.as_deref(), &base_url, &model, &voice).await
            })
        });
    }

    // STT tool with config
    {
        let key = stt_api_key.clone();
        let url = stt_base_url.clone();
        registry.register(media::voice_record_def(), move |args| {
            let key = key.clone();
            let url = url.clone();
            Box::pin(async move { media::handle_voice_record(args, key, url).await })
        });
    }

    // Faz 9: Meta tools
    {
        let ts = task_store.clone();
        registry.register(meta::delegate_task_def(), move |args| {
            let ts = ts.clone();
            Box::pin(async move { meta::handle_delegate_task(args, &ts).await })
        });
    }
    registry.register(meta::premortem_def(), meta::handle_premortem);
    registry.register(meta::eisenhower_def(), meta::handle_eisenhower);

    // Faz 9: Vault tools
    registry.register(vault::obsidian_read_def(), vault::handle_obsidian_read);
    registry.register(vault::obsidian_write_def(), vault::handle_obsidian_write);
    registry.register(vault::obsidian_search_def(), vault::handle_obsidian_search);
    registry.register(vault::vault_stats_def(), vault::handle_vault_stats);

    // Faz 9: MCP tools
    registry.register(mcp::mcp_connect_def(), mcp::handle_mcp_connect);
    registry.register(mcp::mcp_list_tools_def(), mcp::handle_mcp_list_tools);
    registry.register(mcp::mcp_call_tool_def(), mcp::handle_mcp_call_tool);
    registry.register(mcp::mcp_install_def(), mcp::handle_mcp_install);

    // Faz 9: Skills tools
    registry.register(skills::skill_list_def(), skills::handle_skill_list);
    registry.register(skills::skill_view_def(), skills::handle_skill_view);
    registry.register(skills::skill_manage_def(), skills::handle_skill_manage);
    registry.register(skills::skill_search_def(), skills::handle_skill_search);

    // Faz 9: Peer tools
    registry.register(peer::peer_send_def(), peer::handle_peer_send);
    registry.register(peer::peer_broadcast_def(), peer::handle_peer_broadcast);
    registry.register(peer::peer_status_def(), peer::handle_peer_status);
    registry.register(peer::peer_coordinate_def(), peer::handle_peer_coordinate);

    // Sandbox tools (Docker isolation)
    registry.register(sandbox::sandbox_exec_def(), sandbox::handle_sandbox_exec);
    registry.register(sandbox::sandbox_list_def(), sandbox::handle_sandbox_list);

    registry
}
