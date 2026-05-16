//! Plan-mode toggle tools — `enter_plan_mode` / `exit_plan_mode`.
//!
//! Plan mode gates mutating tools: while `PlanState::Drafting` is active,
//! the agent loop rejects any write/edit/bash tool call so the model can
//! research and plan safely before execution begins.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{PlanState, Tool, ToolContext, ToolError};

pub struct EnterPlanMode;

#[async_trait]
impl Tool for EnterPlanMode {
    fn name(&self) -> &str {
        "enter_plan_mode"
    }
    fn description(&self) -> &str {
        "Enter plan mode. While active, only read-only tools may execute; mutating tools (edit_file, write_file, bash, etc.) will be rejected."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let mut state = ctx.plan_state.lock().unwrap();
        if *state == PlanState::Drafting {
            return Ok("Already in plan mode (drafting).".to_string());
        }
        *state = PlanState::Drafting;
        Ok(
            "Plan mode ON (state: drafting) — only read-only tools allowed. \
            Research, analyze, and draft your plan. When done, call `exit_plan_mode`."
                .to_string(),
        )
    }
}

pub struct ExitPlanMode;

#[async_trait]
impl Tool for ExitPlanMode {
    fn name(&self) -> &str {
        "exit_plan_mode"
    }
    fn description(&self) -> &str {
        "Exit plan mode and begin executing the plan. All tools are re-enabled. \
         The plan from the previous messages becomes the execution guide."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let mut state = ctx.plan_state.lock().unwrap();
        if *state == PlanState::Normal {
            return Err(ToolError::InvalidArgs(
                "not in plan mode — call `enter_plan_mode` first".to_string(),
            ));
        }
        *state = PlanState::Executing;
        Ok(
            "Plan mode OFF (state: executing) — all tools allowed. Proceed with \
            executing the plan from the conversation above. When execution is \
            complete, the state returns to normal automatically."
                .to_string(),
        )
    }
}
