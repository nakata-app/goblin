//! Prompt self-modification tools for the agent.
//!
//! These tools let the agent modify its own system prompt in response to
//! user feedback. Each change is git-committed for rollback and triggers
//! a `cargo build --release` to make the change effective.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};
use crate::prompt_mod;

pub struct ModifyPrompt;

#[derive(Debug, Deserialize)]
struct ModifyPromptArgs {
    old_string: String,
    new_string: String,
    reason: String,
}

#[async_trait]
impl Tool for ModifyPrompt {
    fn name(&self) -> &str {
        "modify_prompt"
    }

    fn description(&self) -> &str {
        "Modify the agent's own system prompt (system_prompt.md) by replacing old_string with new_string. \
         Use this when the user gives feedback like 'don't do X', 'stop saying Y', 'change Z'. \
         The change is git-committed for rollback and auto-rebuilt. \
         `old_string` must match exactly once in the file (add surrounding context if needed). \
         `reason` is a short description of why (e.g., 'user said stop using emojis'). \
         IMPORTANT: only call this when the user explicitly gives behavioral feedback \
         or you detect a pattern from multiple corrections. Do NOT call for single-instance corrections."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "old_string": {
                    "type": "string",
                    "description": "Exact text to find and replace in system_prompt.md. Must match exactly once."
                },
                "new_string": {
                    "type": "string",
                    "description": "Text to replace it with."
                },
                "reason": {
                    "type": "string",
                    "description": "Short description of why this change is being made (used as git commit message)."
                }
            },
            "required": ["old_string", "new_string", "reason"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: ModifyPromptArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let aegis_root = ctx
            .aegis_root
            .as_ref()
            .ok_or_else(|| ToolError::InvalidArgs("aegis_root not configured".into()))?;

        match prompt_mod::modify_prompt(aegis_root, &args.old_string, &args.new_string, &args.reason)
        {
            Ok(result) => {
                let mut output = format!(
                    "Prompt modified: {}\nBuild: {}",
                    result.path.display(),
                    result.build_status
                );
                if let Some(ref hash) = result.commit_hash {
                    output.push_str(&format!(
                        "\nGit commit: {}\nRollback with: rollback_prompt",
                        hash
                    ));
                }
                Ok(output)
            }
            Err(e) => {
                let msg = match &e {
                    prompt_mod::PromptModError::OldStringNotFound => {
                        "old_string not found in system_prompt.md. Re-read the file and try again.".to_string()
                    }
                    prompt_mod::PromptModError::MultipleMatches => {
                        "old_string matched multiple times. Add more surrounding context to make it unique.".to_string()
                    }
                    prompt_mod::PromptModError::BuildFailed(s) => {
                        format!("Edit was applied but build failed: {}. Consider rolling back.", s)
                    }
                    _ => e.to_string(),
                };
                Err(ToolError::InvalidArgs(msg))
            }
        }
    }
}

pub struct RollbackPrompt;

#[derive(Debug, Deserialize)]
struct RollbackPromptArgs {
    /// Optional confirmation (required to prevent accidental rollbacks).
    confirm: bool,
}

#[async_trait]
impl Tool for RollbackPrompt {
    fn name(&self) -> &str {
        "rollback_prompt"
    }

    fn description(&self) -> &str {
        "Rollback the last modification to system_prompt.md. \
         Restores the file from git HEAD~1 and rebuilds. \
         Use this when a prompt modification had unintended effects. \
         Requires confirm=true."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "description": "Set to true to confirm the rollback."
                }
            },
            "required": ["confirm"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: RollbackPromptArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if !args.confirm {
            return Err(ToolError::InvalidArgs(
                "Rollback requires confirm=true. Set it to proceed.".into(),
            ));
        }

        let aegis_root = ctx
            .aegis_root
            .as_ref()
            .ok_or_else(|| ToolError::InvalidArgs("aegis_root not configured".into()))?;

        match prompt_mod::rollback_prompt(aegis_root) {
            Ok(result) => {
                let mut output = format!(
                    "Rollback successful. Build: {}",
                    result.build_status
                );
                if let Some(ref hash) = result.commit_hash {
                    output.push_str(&format!("\nGit commit: {}", hash));
                }
                Ok(output)
            }
            Err(e) => Err(ToolError::InvalidArgs(e.to_string())),
        }
    }
}

pub struct ShowPromptChanges;

#[async_trait]
impl Tool for ShowPromptChanges {
    fn name(&self) -> &str {
        "show_prompt_changes"
    }

    fn description(&self) -> &str {
        "Show recent git commit history for system_prompt.md. \
         Use this to review what prompt modifications have been applied."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let aegis_root = ctx
            .aegis_root
            .as_ref()
            .ok_or_else(|| ToolError::InvalidArgs("aegis_root not configured".into()))?;

        match prompt_mod::show_changes(aegis_root) {
            Ok(log) => {
                if log.is_empty() {
                    Ok("No changes found for system_prompt.md".to_string())
                } else {
                    Ok(format!("Recent prompt changes:\n{}", log))
                }
            }
            Err(e) => Err(ToolError::InvalidArgs(e.to_string())),
        }
    }
}
