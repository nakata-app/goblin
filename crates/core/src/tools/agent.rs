//! Subagent spawning tools — `agent` (single) and `parallel_agents` (fan-out).
//!
//! Both tools require an `agent_spawner` to be configured on the
//! `ToolContext`; without it they return an "interactive session required"
//! error. `parallel_agents` fans out via `tokio::task::JoinSet` with a
//! per-agent timeout.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{AgentSpawnRequest, Tool, ToolContext, ToolError};

/// Wrap a subagent prompt with a standard CC-style brief so the subagent
/// knows its role, doesn't ask clarifying questions, and gets the task
/// description as structured context separate from the user instructions.
fn with_subagent_brief(description: &str, prompt: &str) -> String {
    format!(
        "[Subagent brief]\n\
         Task: {description}\n\
         Complete this task autonomously. Do NOT ask clarifying questions — \
         use the available tools to gather any information you need.\n\
         [End brief]\n\n\
         {prompt}"
    )
}

// ---------------------------------------------------------------------
// Agent — spawn a subagent with its own isolated context
// ---------------------------------------------------------------------

pub struct AgentTool;

#[derive(Debug, Deserialize)]
struct AgentToolArgs {
    description: String,
    prompt: String,
    #[serde(default)]
    subagent_type: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    run_in_background: Option<bool>,
    #[serde(default)]
    isolation: Option<String>,
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "Launch a subagent to handle a complex, multi-step task. The subagent \
         has its own isolated context and returns a single result. Available \
         types: general-purpose (default), explore (codebase search), \
         plan (architecture/design)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short (3-5 word) description of the task"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the subagent to perform"
                },
                "subagent_type": {
                    "type": "string",
                    "enum": ["general-purpose", "explore", "plan"],
                    "description": "Type of agent: general-purpose (full tools), explore (read-only codebase search), plan (read-only architecture design)"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for this subagent (e.g. 'sonnet', 'haiku')"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run this agent in the background. You will be notified when it completes."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Isolation mode. 'worktree' creates a temporary git worktree so the agent works on an isolated copy."
                }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let a: AgentToolArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let spawner = ctx.agent_spawner.as_ref().ok_or_else(|| {
            ToolError::InvalidArgs(
                "agent spawner not configured — subagents require an interactive session"
                    .to_string(),
            )
        })?;
        let briefed_prompt = with_subagent_brief(&a.description, &a.prompt);
        let req = AgentSpawnRequest {
            description: a.description,
            prompt: briefed_prompt,
            subagent_type: a.subagent_type,
            model: a.model,
            run_in_background: a.run_in_background.unwrap_or(false),
            isolation: a.isolation,
        };
        spawner(req).map_err(ToolError::Spawn)
    }
}

// ---------------------------------------------------------------------
// Parallel Agents — fan out multiple subagents concurrently
// ---------------------------------------------------------------------

pub struct ParallelAgentsTool;

#[derive(Debug, Deserialize)]
struct ParallelAgentsArgs {
    agents: Vec<ParallelAgentBrief>,
    #[serde(default = "default_parallel_timeout")]
    timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
struct ParallelAgentBrief {
    description: String,
    prompt: String,
    #[serde(default)]
    model: Option<String>,
}

fn default_parallel_timeout() -> u64 {
    300
}

#[async_trait]
impl Tool for ParallelAgentsTool {
    fn name(&self) -> &str {
        "parallel_agents"
    }
    fn description(&self) -> &str {
        "Run multiple subagents concurrently and collect their results. Each \
         agent runs in its own isolated context. Use this when you have \
         independent tasks that can be parallelized (e.g. searching different \
         parts of a codebase, researching multiple topics simultaneously)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {
                                "type": "string",
                                "description": "Short (3-5 word) description of this agent's task"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "The task for this subagent to perform"
                            },
                            "model": {
                                "type": "string",
                                "description": "Optional model override for this agent"
                            }
                        },
                        "required": ["description", "prompt"],
                        "additionalProperties": false
                    },
                    "minItems": 1,
                    "description": "List of agent briefs to run concurrently"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Per-agent timeout in seconds (default 300)",
                    "default": 300
                }
            },
            "required": ["agents"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let a: ParallelAgentsArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if a.agents.is_empty() {
            return Err(ToolError::InvalidArgs(
                "agents array must not be empty".to_string(),
            ));
        }

        let spawner = ctx.agent_spawner.as_ref().ok_or_else(|| {
            ToolError::InvalidArgs(
                "agent spawner not configured — subagents require an interactive session"
                    .to_string(),
            )
        })?;

        let timeout = Duration::from_secs(a.timeout_secs);
        let spawner = Arc::clone(spawner);

        // Fan out all agents concurrently via JoinSet
        let mut join_set = tokio::task::JoinSet::new();
        for (idx, brief) in a.agents.into_iter().enumerate() {
            let spawner = Arc::clone(&spawner);
            let briefed = with_subagent_brief(&brief.description, &brief.prompt);
            let req = AgentSpawnRequest {
                description: brief.description.clone(),
                prompt: briefed,
                subagent_type: None,
                model: brief.model,
                run_in_background: false,
                isolation: None,
            };
            let desc = brief.description;
            join_set.spawn(async move {
                let result = tokio::time::timeout(timeout, async { spawner(req) }).await;
                (idx, desc, result)
            });
        }

        // Collect results, ordered by original index
        let agent_count = join_set.len();
        let mut results: Vec<Option<(String, Result<String, String>)>> = vec![None; agent_count];

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, desc, Ok(spawn_result))) => {
                    results[idx] = Some((desc, spawn_result));
                }
                Ok((idx, desc, Err(_elapsed))) => {
                    results[idx] = Some((
                        desc,
                        Err(format!("agent timed out after {}s", timeout.as_secs())),
                    ));
                }
                Err(join_err) => {
                    // JoinError means the task panicked — find first empty slot
                    if let Some(slot) = results.iter_mut().find(|s| s.is_none()) {
                        *slot = Some((
                            "unknown".to_string(),
                            Err(format!("agent task failed: {join_err}")),
                        ));
                    }
                }
            }
        }

        // Format combined report
        let mut report = String::new();
        for (i, entry) in results.into_iter().enumerate() {
            let (desc, result) = entry.unwrap_or_else(|| {
                (
                    "unknown".to_string(),
                    Err("agent result missing".to_string()),
                )
            });
            report.push_str(&format!("## Agent {} — {}\n\n", i + 1, desc));
            match result {
                Ok(text) => report.push_str(&text),
                Err(err) => report.push_str(&format!("**Error:** {err}")),
            }
            report.push_str("\n\n---\n\n");
        }

        Ok(report)
    }
}
