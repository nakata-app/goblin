//! Cron tools — persistent scheduled entry management.
//!
//! Entries live in `<workspace>/.aegis/cron.json`. The REPL reads
//! `read_crons` to tick the polling loop; the tools here let the agent
//! create / list / delete schedules.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronEntry {
    pub id: u32,
    pub schedule: String,
    pub command: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
}

pub fn read_crons(workspace: &Path) -> Vec<CronEntry> {
    let path = workspace.join(".metis").join("cron.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_crons(workspace: &Path, crons: &[CronEntry]) -> Result<(), ToolError> {
    let dir = workspace.join(".metis");
    std::fs::create_dir_all(&dir).map_err(|source| ToolError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let json = serde_json::to_string_pretty(crons)
        .map_err(|e| ToolError::InvalidArgs(format!("json serialize: {e}")))?;
    std::fs::write(dir.join("cron.json"), json).map_err(|source| ToolError::Io {
        path: ".metis/cron.json".to_string(),
        source,
    })
}

pub struct CronCreate;

#[derive(Debug, Deserialize)]
struct CronCreateArgs {
    /// Cron schedule (e.g. "0 9 * * 1-5" for weekdays at 9am).
    schedule: String,
    /// Command to run (shell command or prompt text).
    command: String,
    /// Optional human description.
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for CronCreate {
    fn name(&self) -> &str {
        "cron_create"
    }
    fn description(&self) -> &str {
        "Create a scheduled cron entry. Schedule uses standard cron syntax \
         (minute hour day month weekday). The command will be executed at \
         the scheduled times."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "Cron expression (e.g. '0 9 * * 1-5')"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command or prompt to run"
                },
                "description": {
                    "type": "string",
                    "description": "Human-readable description"
                }
            },
            "required": ["schedule", "command"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: CronCreateArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        // Validate schedule has 5 fields
        let fields: Vec<&str> = args.schedule.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ToolError::InvalidArgs(
                "cron schedule must have exactly 5 fields (min hour day month weekday)".to_string(),
            ));
        }
        let mut crons = read_crons(&ctx.workspace_root);
        let id = crons.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        crons.push(CronEntry {
            id,
            schedule: args.schedule.clone(),
            command: args.command.clone(),
            description: args.description.unwrap_or_default(),
            enabled: true,
        });
        write_crons(&ctx.workspace_root, &crons)?;
        Ok(format!(
            "Created cron #{id}: `{}` → `{}`\n",
            args.schedule, args.command
        ))
    }
}

pub struct CronList;

#[async_trait]
impl Tool for CronList {
    fn name(&self) -> &str {
        "cron_list"
    }
    fn description(&self) -> &str {
        "List all scheduled cron entries."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let crons = read_crons(&ctx.workspace_root);
        if crons.is_empty() {
            return Ok("(no cron entries)\n".to_string());
        }
        let mut out = String::new();
        for c in &crons {
            let status = if c.enabled { "✓" } else { "✗" };
            let desc = if c.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", c.description)
            };
            out.push_str(&format!(
                "  {status} #{}: {} → `{}`{desc}\n",
                c.id, c.schedule, c.command
            ));
        }
        Ok(out)
    }
}

pub struct CronDelete;

#[derive(Debug, Deserialize)]
struct CronDeleteArgs {
    id: u32,
}

#[async_trait]
impl Tool for CronDelete {
    fn name(&self) -> &str {
        "cron_delete"
    }
    fn description(&self) -> &str {
        "Delete a cron entry by id."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Cron entry id to delete" }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: CronDeleteArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let mut crons = read_crons(&ctx.workspace_root);
        let before = crons.len();
        crons.retain(|c| c.id != args.id);
        if crons.len() == before {
            return Err(ToolError::InvalidArgs(format!(
                "cron #{} not found",
                args.id
            )));
        }
        write_crons(&ctx.workspace_root, &crons)?;
        Ok(format!("Deleted cron #{}\n", args.id))
    }
}
