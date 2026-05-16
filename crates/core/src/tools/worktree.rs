//! Git-worktree isolation tools — `enter_worktree` / `exit_worktree`.
//!
//! `enter_worktree` creates a temporary branch in a fresh worktree under
//! the system temp dir; subsequent file ops target that path until
//! `exit_worktree` runs. Clean exits (no changes) auto-delete the
//! worktree and branch; dirty exits keep the path and report it.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError, WorktreeState};

pub struct EnterWorktree;

#[derive(Debug, Deserialize)]
struct EnterWorktreeArgs {
    /// Optional branch name. Defaults to `metis-worktree-<timestamp>`.
    #[serde(default)]
    branch: Option<String>,
}

#[async_trait]
impl Tool for EnterWorktree {
    fn name(&self) -> &str {
        "enter_worktree"
    }
    fn description(&self) -> &str {
        "Create a temporary git worktree and switch the workspace to it. \
         All file operations will target the worktree until exit_worktree \
         is called. Useful for isolated experiments or parallel work."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "Branch name for the worktree (auto-generated if omitted)"
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: EnterWorktreeArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Check not already in a worktree
        {
            let wt = ctx.worktree.lock().unwrap();
            if wt.is_some() {
                return Err(ToolError::InvalidArgs(
                    "already in a worktree — call exit_worktree first".to_string(),
                ));
            }
        }

        // Verify we're in a git repo
        let git_check = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&ctx.workspace_root)
            .output()
            .map_err(|e| ToolError::Spawn(format!("git: {e}")))?;
        if !git_check.status.success() {
            return Err(ToolError::InvalidArgs(
                "not inside a git repository".to_string(),
            ));
        }

        // Generate a unique suffix using timestamp + thread ID to avoid
        // collisions in parallel test environments.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let tid = format!("{:?}", std::thread::current().id());
        let suffix = format!("{ts}-{}", tid.replace(|c: char| !c.is_ascii_digit(), ""));
        let branch = args
            .branch
            .unwrap_or_else(|| format!("metis-worktree-{suffix}"));
        // Place worktree in system temp dir to avoid nested-git issues
        let wt_path = std::env::temp_dir().join(format!("metis-wt-{suffix}"));

        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                wt_path.to_str().unwrap_or("."),
                "HEAD",
            ])
            .current_dir(&ctx.workspace_root)
            .output()
            .map_err(|e| ToolError::Spawn(format!("git worktree add: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ToolError::Spawn(format!(
                "git worktree add failed: {stderr}"
            )));
        }

        let state = WorktreeState {
            original_root: ctx.workspace_root.clone(),
            worktree_path: wt_path.clone(),
            branch_name: branch.clone(),
        };
        *ctx.worktree.lock().unwrap() = Some(state);

        Ok(format!(
            "Worktree created at {} on branch `{branch}`\n\
             All file operations now target the worktree.\n\
             Call exit_worktree to return to the original workspace.",
            wt_path.display()
        ))
    }
}

pub struct ExitWorktree;

#[async_trait]
impl Tool for ExitWorktree {
    fn name(&self) -> &str {
        "exit_worktree"
    }
    fn description(&self) -> &str {
        "Exit the current git worktree and return to the original workspace. \
         If the worktree has no changes, it is automatically removed. \
         Otherwise, the worktree path and branch name are reported."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let state = {
            let mut wt = ctx.worktree.lock().unwrap();
            match wt.take() {
                Some(s) => s,
                None => {
                    return Err(ToolError::InvalidArgs(
                        "not in a worktree — nothing to exit".to_string(),
                    ));
                }
            }
        };

        // Check if worktree has any changes
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&state.worktree_path)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let has_changes = !status.is_empty();

        if !has_changes {
            // Clean removal — no changes, remove worktree and branch
            let _ = std::process::Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    state.worktree_path.to_str().unwrap_or("."),
                ])
                .current_dir(&state.original_root)
                .output();
            let _ = std::process::Command::new("git")
                .args(["branch", "-D", &state.branch_name])
                .current_dir(&state.original_root)
                .output();
            Ok(format!(
                "Exited worktree. No changes detected — worktree and branch `{}` removed.\n\
                 Workspace restored to {}",
                state.branch_name,
                state.original_root.display()
            ))
        } else {
            // Keep worktree, report location
            Ok(format!(
                "Exited worktree. Changes detected — worktree preserved at {}\n\
                 Branch: `{}`\n\
                 Workspace restored to {}",
                state.worktree_path.display(),
                state.branch_name,
                state.original_root.display()
            ))
        }
    }
}
