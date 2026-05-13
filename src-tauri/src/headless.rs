//! Headless entry points. Right now this is just the MCP stdio server
//! used by `src/bin/goblin-mcp.rs`. Anything that needs to drive the
//! Goblin tool surface *without* spinning up Tauri lives here.

use crate::config::Config;
use crate::mcp::McpHub;
use crate::plugin::PluginRegistry;
use crate::task::TaskStore;
use crate::tools::mcp_server::McpServerHandle;
use crate::tools::ToolRegistry;
use crate::whatsapp::WhatsappBridge;
use std::sync::Arc;

/// Build a tool registry suitable for a headless MCP server. Memory,
/// session and cron stores are intentionally *not* threaded in here —
/// the agent loop is what consumes those, and a headless tool host
/// doesn't run an agent loop. Tools that need their own storage (e.g.
/// task_store for delegate_task) get fresh in-memory instances.
pub fn build_headless_registry(cfg: &Config) -> Result<ToolRegistry, String> {
    // Spin up just enough infrastructure for the tools to function.
    // McpHub is empty in headless mode — we are the MCP server, not
    // the client.
    let task_store = TaskStore::new_in_memory()?;
    let whatsapp = Arc::new(WhatsappBridge::new());
    let plugins = Arc::new(PluginRegistry::new()?);
    let mcp_hub = Arc::new(McpHub::new());

    Ok(crate::tools::create_tool_registry(
        cfg.stt.clone(),
        cfg.tts.clone(),
        cfg.tools.clone(),
        task_store,
        whatsapp,
        None,
        plugins,
        mcp_hub,
    ))
}

/// Run the MCP stdio server on the current thread. Returns Err with a
/// short reason for the fatal cases (config missing required pieces
/// for tools to work, etc.); otherwise loops forever until stdin EOF
/// closes the session.
pub fn run_mcp_stdio() -> Result<(), String> {
    let cfg = Config::load().map_err(|e| format!("config: {}", e))?;
    let registry = Arc::new(build_headless_registry(&cfg)?);

    // Snapshot tool defs once before handing the registry to the
    // server — the server clones them per tools/list.
    let tool_defs: Vec<(String, String, serde_json::Value)> = registry
        .definitions()
        .iter()
        .map(|d| {
            (
                d.function.name.clone(),
                d.function.description.clone(),
                d.function.parameters.clone(),
            )
        })
        .collect();

    let handle = McpServerHandle::new();
    handle.run_stdio(registry, tool_defs);
    Ok(())
}
