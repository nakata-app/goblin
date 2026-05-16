//! Shared agent-spawner builder for REPL, TUI, one-shot, and IDE entry
//! points. The `agent` and `parallel_agents` tools call back into this
//! closure when the model wants to fan out work onto a subagent. Without
//! it, the tools fail with `agent spawner not configured`.
//!
//! Originally lived inline in `repl/builder.rs`; extracted so the TUI
//! (which is the primary surface) and the one-shot/IDE paths can share
//! the exact same wiring instead of silently dropping the capability.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aegis_api::ChatProvider;
use aegis_core::{
    AgentConfig, AgentSpawnRequest, AgentSpawnerFn, BackgroundAgents, BackgroundResult, Permission,
    Subagent, SubagentBrief, SubagentType, ToolContext, ToolRegistry,
};

pub fn build(
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    base_config: AgentConfig,
    permission: Arc<dyn Permission>,
    background_agents: BackgroundAgents,
) -> AgentSpawnerFn {
    let sc = client;
    let sr = registry;
    let ws: PathBuf = workspace.to_path_buf();
    let perm = permission;
    let bg_store = background_agents;
    Arc::new(move |req: AgentSpawnRequest| {
        let ty = match req.subagent_type.as_deref() {
            Some(name) => SubagentType::by_name(name).ok_or_else(|| {
                format!(
                    "unknown subagent type: `{name}`. Available: general-purpose, explore, plan"
                )
            })?,
            None => SubagentType::general_purpose(),
        };
        let mut child_config = base_config.clone();
        if let Some(ref model) = req.model {
            child_config.model = model.clone();
        }

        let resolve_workspace =
            |ws: &Path, isolation: &Option<String>| -> Result<(PathBuf, Option<String>), String> {
                match isolation.as_deref() {
                    Some("worktree") => {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let branch = format!("metis-agent-{ts}");
                        let wt_path = ws.join(".metis").join("worktrees").join(&branch);
                        std::fs::create_dir_all(wt_path.parent().unwrap())
                            .map_err(|e| format!("failed to create worktree dir: {e}"))?;
                        let output = std::process::Command::new("git")
                            .args(["worktree", "add", "-b", &branch, wt_path.to_str().unwrap()])
                            .current_dir(ws)
                            .output()
                            .map_err(|e| format!("git worktree add failed: {e}"))?;
                        if !output.status.success() {
                            return Err(format!(
                                "git worktree add failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            ));
                        }
                        Ok((wt_path, Some(branch)))
                    }
                    _ => Ok((ws.to_path_buf(), None)),
                }
            };

        let cleanup_worktree = |ws: &Path, branch: &Option<String>| {
            if let Some(ref branch) = branch {
                let _ = std::process::Command::new("git")
                    .args(["worktree", "remove", "--force", branch])
                    .current_dir(ws)
                    .output();
                let _ = std::process::Command::new("git")
                    .args(["branch", "-D", branch])
                    .current_dir(ws)
                    .output();
            }
        };

        if req.run_in_background {
            let sc = Arc::clone(&sc);
            let sr = Arc::clone(&sr);
            let ws = ws.clone();
            let perm = Arc::clone(&perm);
            let bg = bg_store.clone();
            let desc = req.description.clone();
            let isolation = req.isolation.clone();
            bg.inc_pending();
            tokio::task::spawn(async move {
                let (child_ws, branch) = match resolve_workspace(&ws, &isolation) {
                    Ok(v) => v,
                    Err(e) => {
                        bg.push_completed(BackgroundResult {
                            description: desc,
                            result: Err(e),
                        });
                        return;
                    }
                };
                let child_ctx = ToolContext::new(child_ws);
                let spawner =
                    Subagent::new(&*sc, &sr, child_ctx, child_config).with_permission(perm);
                let brief = SubagentBrief {
                    description: desc.clone(),
                    prompt: req.prompt,
                    system_prompt: None,
                };
                let result = spawner
                    .spawn_typed(&ty, brief)
                    .await
                    .map(|r| r.final_text)
                    .map_err(|e| e.to_string());
                cleanup_worktree(&ws, &branch);
                bg.push_completed(BackgroundResult {
                    description: desc,
                    result,
                });
            });
            Ok(format!(
                "Agent \"{}\" started in background. You will be notified when it completes.",
                req.description
            ))
        } else {
            let (child_ws, branch) = resolve_workspace(&ws, &req.isolation)?;
            let child_ctx = ToolContext::new(child_ws);
            let spawner = Subagent::new(&*sc, &sr, child_ctx, child_config)
                .with_permission(Arc::clone(&perm));
            let brief = SubagentBrief {
                description: req.description,
                prompt: req.prompt,
                system_prompt: None,
            };
            let report = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(spawner.spawn_typed(&ty, brief))
            })
            .map_err(|e| e.to_string());
            cleanup_worktree(&ws, &branch);
            let report = report?;
            Ok(report.final_text)
        }
    })
}
