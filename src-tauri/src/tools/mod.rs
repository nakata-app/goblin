pub mod file_ops;
pub mod search;
pub mod shell;
pub mod web;
pub mod browser;
pub mod git;
pub mod media;
pub mod meta;
pub mod vault;
// `mcp` is trimmed down to just `mcp_install` (a discovery helper that
// prints install instructions). The old connect/list/call stubs were
// superseded by the auto-booting McpHub in src/mcp/mod.rs.
// `mcp_server` is the stdio JSON-RPC handle a future headless
// `goblin-mcp` binary will link against; unused in the desktop build
// but kept so the headless target can re-enable it without a rewrite.
pub mod mcp;
#[allow(dead_code)]
pub mod mcp_server;
pub mod skills;
pub mod peer;
pub mod compactor;
pub mod sandbox;

use crate::config::SttConfig;
use crate::config::ToolsConfig;
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

pub fn create_tool_registry(
    stt: SttConfig,
    tts: TtsConfig,
    tools_cfg: ToolsConfig,
    task_store: TaskStore,
    whatsapp: std::sync::Arc<crate::whatsapp::WhatsappBridge>,
    mnemonics: Option<std::sync::Arc<crate::mnemonics::MnemonicsClient>>,
    plugins: std::sync::Arc<crate::plugin::PluginRegistry>,
    mcp_hub: std::sync::Arc<crate::mcp::McpHub>,
) -> ToolRegistry {
    // Install (or refresh) the live shell guardrails. create_tool_registry
    // is called once at startup and again by save_config when the user
    // edits ~/.goblin/config.toml from the settings panel.
    shell::apply_shell_guards(&tools_cfg.shell_allowlist, &tools_cfg.shell_blocklist);

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

    // Faz 9 MCP tools: mcp_connect / mcp_list_tools / mcp_call_tool were
    // stub placeholders that returned help strings; they are replaced by
    // the auto-boot McpHub plus the `mcp_servers` / `mcp_tools` /
    // `mcp_call` tools registered further down. `mcp_install` (npm-based
    // package installation helper) survives because it complements the
    // new dispatcher rather than duplicating it.
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

    // WhatsApp tools
    {
        let wa = whatsapp.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "whatsapp_send".to_string(),
                    description: "Send a WhatsApp message to a phone number (international format without +, e.g. 905551234567) or JID".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "to": {
                                "type": "string",
                                "description": "Phone number in international format without + (e.g. 905551234567) or WhatsApp JID"
                            },
                            "text": {
                                "type": "string",
                                "description": "Message text to send"
                            }
                        },
                        "required": ["to", "text"]
                    }),
                },
            },
            move |args| {
                let wa = wa.clone();
                Box::pin(async move {
                    let to = args["to"].as_str().ok_or("Missing 'to' parameter")?.to_string();
                    let text = args["text"].as_str().ok_or("Missing 'text' parameter")?.to_string();
                    let result = wa.send_message(&to, &text).await?;
                    if result.success {
                        Ok(format!("Message sent successfully. ID: {}", result.id.unwrap_or_default()))
                    } else {
                        Err(format!("Failed to send: {}", result.error.unwrap_or_default()))
                    }
                })
            },
        );
    }
    {
        let wa = whatsapp.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "whatsapp_check".to_string(),
                    description: "Check WhatsApp connection status and see pending messages".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    }),
                },
            },
            move |_args| {
                let wa = wa.clone();
                Box::pin(async move {
                    let status = wa.get_status().await?;
                    let messages = wa.poll_messages().await.unwrap_or_default();
                    let msg_list: Vec<String> = messages.iter()
                        .map(|m| format!("[{}] {}: {}", m.from, m.id, m.text))
                        .collect();
                    Ok(format!(
                        "Status: {}\nUser: {}\nMessages ({}):\n{}",
                        status.status,
                        status.user.map(|u| u.name).unwrap_or_default(),
                        messages.len(),
                        if msg_list.is_empty() { "  (none)".to_string() } else { msg_list.join("\n") }
                    ))
                })
            },
        );
    }

    // Cross-project semantic memory via Atakan's `mnemonics` binary.
    // Only exposed when the binary actually responded to --help, so the
    // agent doesn't get phantom tools that always error.
    if let Some(client) = mnemonics {
        {
            let mn = client.clone();
            registry.register(
                crate::provider::ToolDefinition {
                    def_type: "function".to_string(),
                    function: crate::provider::FunctionDef {
                        name: "mnemonics_retrieve".to_string(),
                        description: "Search Atakan's cross-project semantic memory. Use this when the answer might live in a previous session or other project, not just this codebase. Returns top-k hits ordered by tier-aware decay score.".to_string(),
                        parameters: serde_json::json!({
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "description": "Free-text query." },
                                "ns": { "type": "string", "description": "Optional namespace filter (e.g. 'proj:goblin', 'feedback', 'global')." },
                                "top_k": { "type": "integer", "description": "Max hits to return (default 5).", "default": 5 },
                                "decay": { "type": "boolean", "description": "Apply tier-aware decay scoring (default true).", "default": true }
                            },
                            "required": ["query"]
                        }),
                    },
                },
                move |args| {
                    let mn = mn.clone();
                    Box::pin(async move {
                        let query = args["query"].as_str().ok_or("Missing 'query' parameter")?.to_string();
                        let ns = args["ns"].as_str().map(|s| s.to_string());
                        let top_k = args["top_k"].as_u64().unwrap_or(5) as u32;
                        let decay = args["decay"].as_bool().unwrap_or(true);
                        tokio::task::spawn_blocking(move || {
                            mn.retrieve(&query, ns.as_deref(), top_k, decay)
                        })
                        .await
                        .map_err(|e| format!("Mnemonics task panicked: {}", e))?
                    })
                },
            );
        }
        {
            let mn = client.clone();
            registry.register(
                crate::provider::ToolDefinition {
                    def_type: "function".to_string(),
                    function: crate::provider::FunctionDef {
                        name: "mnemonics_ingest".to_string(),
                        description: "Append a memory to Atakan's cross-project store. Use sparingly: only for decisions, non-obvious bug root causes, or facts the user explicitly asked to remember. Avoid routine session noise.".to_string(),
                        parameters: serde_json::json!({
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "description": "The memory text. Prefix with '[YYYY-MM-DD] [project]' when context matters." },
                                "ns": { "type": "string", "description": "Optional namespace (defaults to 'proj:goblin')." }
                            },
                            "required": ["text"]
                        }),
                    },
                },
                move |args| {
                    let mn = mn.clone();
                    Box::pin(async move {
                        let text = args["text"].as_str().ok_or("Missing 'text' parameter")?.to_string();
                        let ns = args["ns"].as_str().map(|s| s.to_string());
                        tokio::task::spawn_blocking(move || {
                            mn.ingest(&text, ns.as_deref())
                        })
                        .await
                        .map_err(|e| format!("Mnemonics task panicked: {}", e))?
                    })
                },
            );
        }
    }

    // Wasm plugin tools. Two tools instead of one so the LLM can discover
    // what's installed before deciding to call something. Always exposed
    // because the registry is always present; `plugin_list` just returns
    // an empty list if no plugins are installed.
    {
        let plugins = plugins.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "plugin_list".to_string(),
                    description: "List Wasm plugins installed for this Goblin instance. Plugins live in ~/.goblin/plugins/ and are sandboxed (no fs, no network, fuel-limited).".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    }),
                },
            },
            move |_args| {
                let plugins = plugins.clone();
                Box::pin(async move {
                    let list = plugins.list();
                    if list.is_empty() {
                        Ok("No plugins installed.".to_string())
                    } else {
                        Ok(format!("Installed plugins ({}): {}", list.len(), list.join(", ")))
                    }
                })
            },
        );
    }
    {
        let plugins = plugins.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "plugin_run".to_string(),
                    description: "Invoke a Wasm plugin with a UTF-8 string input. Plugin runs in a sandbox: no network, no filesystem, fuel-limited so it cannot hang. Use `plugin_list` first to discover what's available.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Plugin name (matches a file under ~/.goblin/plugins/)" },
                            "input": { "type": "string", "description": "UTF-8 input to pass to the plugin" }
                        },
                        "required": ["name", "input"]
                    }),
                },
            },
            move |args| {
                let plugins = plugins.clone();
                Box::pin(async move {
                    let name = args["name"].as_str().ok_or("Missing 'name' parameter")?.to_string();
                    let input = args["input"].as_str().ok_or("Missing 'input' parameter")?.to_string();
                    tokio::task::spawn_blocking(move || plugins.run(&name, &input))
                        .await
                        .map_err(|e| format!("Plugin task panicked: {}", e))?
                })
            },
        );
    }

    // Generic MCP dispatcher tools. One per verb (list/call) instead of
    // one per (server, tool) pair, because static tool registration here
    // happens before we know what an MCP server will expose. The LLM
    // discovers tools dynamically via mcp_servers + mcp_tools then
    // invokes them via mcp_call.
    {
        let hub = mcp_hub.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "mcp_servers".to_string(),
                    description: "List MCP servers Goblin auto-connected to at boot (from [mcp.servers.*] config). Each server exposes one or more tools you can inspect with mcp_tools.".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {}, "required": []}),
                },
            },
            move |_args| {
                let hub = hub.clone();
                Box::pin(async move {
                    let names = hub.server_names();
                    if names.is_empty() {
                        Ok("No MCP servers configured.".to_string())
                    } else {
                        Ok(format!("MCP servers ({}): {}", names.len(), names.join(", ")))
                    }
                })
            },
        );
    }
    {
        let hub = mcp_hub.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "mcp_tools".to_string(),
                    description: "List the tools available on a specific MCP server. Returns name, description, and input JSON Schema for each tool.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": { "type": "string", "description": "MCP server name (see mcp_servers)." }
                        },
                        "required": ["server"]
                    }),
                },
            },
            move |args| {
                let hub = hub.clone();
                Box::pin(async move {
                    let server = args["server"].as_str().ok_or("Missing 'server' parameter")?.to_string();
                    let tools = hub.list_tools(&server)
                        .ok_or_else(|| format!("Unknown MCP server: {}", server))?;
                    let payload = serde_json::to_string_pretty(&tools)
                        .map_err(|e| format!("Serialize: {}", e))?;
                    Ok(payload)
                })
            },
        );
    }
    {
        let hub = mcp_hub.clone();
        registry.register(
            crate::provider::ToolDefinition {
                def_type: "function".to_string(),
                function: crate::provider::FunctionDef {
                    name: "mcp_call".to_string(),
                    description: "Invoke a tool on an MCP server. Pass `server`, `tool`, and `arguments` (a JSON object matching the tool's inputSchema from mcp_tools).".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": { "type": "string" },
                            "tool": { "type": "string" },
                            "arguments": { "type": "object", "description": "Arguments object matching the tool's inputSchema." }
                        },
                        "required": ["server", "tool"]
                    }),
                },
            },
            move |args| {
                let hub = hub.clone();
                Box::pin(async move {
                    let server = args["server"].as_str().ok_or("Missing 'server' parameter")?.to_string();
                    let tool = args["tool"].as_str().ok_or("Missing 'tool' parameter")?.to_string();
                    let arguments = args.get("arguments").cloned().unwrap_or(serde_json::Value::Object(Default::default()));
                    tokio::task::spawn_blocking(move || hub.call(&server, &tool, arguments))
                        .await
                        .map_err(|e| format!("MCP task panicked: {}", e))?
                })
            },
        );
    }

    registry
}
