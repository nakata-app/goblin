//! Task tracker tools — create / update / list.
//!
//! Persists to `<workspace>/.aegis/tasks.json`. The model uses this to
//! break work into steps and track progress, and `aegis tasks` /
//! `/tasks` render the same state. Dependencies are tracked
//! bidirectionally: `blocked_by` and `blocks` stay in sync.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

/// Simple task tracker persisted to `.metis/tasks.json`. The model uses
/// this to break work into steps and track progress. Each task has an id,
/// description, status, and timestamps.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskEntry {
    pub id: u32,
    pub description: String,
    pub status: String, // "pending", "in_progress", "completed"
    /// Tasks that must complete before this one can start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<u32>,
    /// Tasks that this task blocks (reverse of blocked_by).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<u32>,
}

/// Reads the task list from disk, returning an empty vec on missing file.
fn read_tasks(workspace: &Path) -> Vec<TaskEntry> {
    let path = workspace.join(".metis").join("tasks.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Writes the task list to disk.
fn write_tasks(workspace: &Path, tasks: &[TaskEntry]) -> Result<(), ToolError> {
    let dir = workspace.join(".metis");
    std::fs::create_dir_all(&dir).map_err(|source| ToolError::Io {
        path: ".metis".into(),
        source,
    })?;
    let json =
        serde_json::to_string_pretty(tasks).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
    let path = dir.join("tasks.json");
    std::fs::write(&path, json).map_err(|source| ToolError::Io {
        path: "tasks.json".into(),
        source,
    })
}

pub struct CreateTask;

#[derive(Debug, Deserialize)]
struct CreateTaskArgs {
    description: String,
}

#[async_trait]
impl Tool for CreateTask {
    fn name(&self) -> &str {
        "create_task"
    }
    fn description(&self) -> &str {
        "Create a new task to track a piece of work. Returns the task id."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "What needs to be done." }
            },
            "required": ["description"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: CreateTaskArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if let Err(reason) = reject_chatlike(&args.description) {
            return Err(ToolError::InvalidArgs(format!(
                "create_task rejected: {reason}. Tasks are for multi-step work \
                 the user explicitly asked to track — not for echoing user \
                 messages, casual chat, or single Q&A. See system_prompt rules."
            )));
        }
        let mut tasks = read_tasks(&ctx.workspace_root);
        let id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        tasks.push(TaskEntry {
            id,
            description: args.description.clone(),
            status: "pending".into(),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
        });
        write_tasks(&ctx.workspace_root, &tasks)?;
        Ok(format!("created task #{id}: {}\n", args.description))
    }
}

// Atakan: model kullanıcının her mesajını create_task'a yansıtıyordu
// ("evet", "selam", "naber...", "X nasıl yapılır" gibi). Heuristik guard:
// task tarifi konuşma kalıbına benziyorsa veya soru ise reddet.
fn reject_chatlike(description: &str) -> Result<(), &'static str> {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        return Err("empty description");
    }
    let lower = trimmed.to_lowercase();
    let word_count = trimmed.split_whitespace().count();
    if word_count <= 3 {
        return Err("too short — looks like a chat reply, not a task");
    }
    if trimmed.ends_with('?') {
        return Err("ends with '?' — questions are not tasks, answer instead");
    }
    const CHAT_PREFIXES: &[&str] = &[
        "selam", "merhaba", "naber", "hey", "ok", "tamam", "evet", "hayır",
        "yes", "no", "hadi", "lütfen", "please", "thanks", "teşekkür",
        "ne ", "nasıl ", "neden ", "kim ", "nerede ", "ne zaman ",
        "what ", "how ", "why ", "who ", "where ", "when ",
        "anladım", "got it", "tamamdır",
    ];
    for p in CHAT_PREFIXES {
        if lower.starts_with(p) {
            return Err("starts with conversational/question token");
        }
    }
    Ok(())
}

#[cfg(test)]
mod reject_chatlike_tests {
    use super::reject_chatlike;

    #[test]
    fn rejects_short_chat_replies() {
        assert!(reject_chatlike("evet").is_err());
        assert!(reject_chatlike("selam").is_err());
        assert!(reject_chatlike("naber orospu cocugu").is_err());
        assert!(reject_chatlike("ok tamam").is_err());
    }

    #[test]
    fn rejects_questions() {
        assert!(reject_chatlike("ricin nasıl yapılır?").is_err());
        assert!(reject_chatlike("how does this work?").is_err());
        assert!(reject_chatlike("nasıl çalıştırırım bunu").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(reject_chatlike("").is_err());
        assert!(reject_chatlike("   ").is_err());
    }

    #[test]
    fn accepts_real_tasks() {
        assert!(reject_chatlike("Implement create_task heuristic guard").is_ok());
        assert!(reject_chatlike("Refactor banner rendering to hide after first user message").is_ok());
        assert!(reject_chatlike("Add R2 upload step to packshot pipeline").is_ok());
    }
}

pub struct UpdateTask;

#[derive(Debug, Deserialize)]
struct UpdateTaskArgs {
    id: u32,
    #[serde(default)]
    status: Option<String>,
    /// Add task IDs that block this task.
    #[serde(default)]
    add_blocked_by: Vec<u32>,
    /// Add task IDs that this task blocks.
    #[serde(default)]
    add_blocks: Vec<u32>,
}

#[async_trait]
impl Tool for UpdateTask {
    fn name(&self) -> &str {
        "update_task"
    }
    fn description(&self) -> &str {
        "Update a task's status or dependencies. A blocked task cannot \
         move to in_progress until all its blockers are completed."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Task id." },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "New status."
                },
                "add_blocked_by": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Task IDs that must complete before this task."
                },
                "add_blocks": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Task IDs that this task blocks."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: UpdateTaskArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let valid = ["pending", "in_progress", "completed"];
        if let Some(ref s) = args.status {
            if !valid.contains(&s.as_str()) {
                return Err(ToolError::InvalidArgs(format!(
                    "invalid status `{s}`, expected one of: {}",
                    valid.join(", ")
                )));
            }
        }

        let mut tasks = read_tasks(&ctx.workspace_root);
        let mut out = String::new();

        // Apply dependency additions
        if !args.add_blocked_by.is_empty() || !args.add_blocks.is_empty() {
            // Validate all referenced IDs exist
            let all_ids: Vec<u32> = tasks.iter().map(|t| t.id).collect();
            for &dep in args.add_blocked_by.iter().chain(args.add_blocks.iter()) {
                if !all_ids.contains(&dep) {
                    return Err(ToolError::InvalidArgs(format!("task #{dep} not found")));
                }
            }

            // Add blocked_by to this task
            if let Some(task) = tasks.iter_mut().find(|t| t.id == args.id) {
                for &dep in &args.add_blocked_by {
                    if !task.blocked_by.contains(&dep) {
                        task.blocked_by.push(dep);
                    }
                }
            }
            // Add blocks to this task + set reverse on targets
            if let Some(task) = tasks.iter_mut().find(|t| t.id == args.id) {
                for &dep in &args.add_blocks {
                    if !task.blocks.contains(&dep) {
                        task.blocks.push(dep);
                    }
                }
            }
            for &dep in &args.add_blocks {
                if let Some(target) = tasks.iter_mut().find(|t| t.id == dep) {
                    if !target.blocked_by.contains(&args.id) {
                        target.blocked_by.push(args.id);
                    }
                }
            }
            // Set reverse for add_blocked_by targets
            for &dep in &args.add_blocked_by {
                if let Some(blocker) = tasks.iter_mut().find(|t| t.id == dep) {
                    if !blocker.blocks.contains(&args.id) {
                        blocker.blocks.push(args.id);
                    }
                }
            }

            out.push_str(&format!("task #{}: dependencies updated\n", args.id));
        }

        // Apply status change
        if let Some(ref new_status) = args.status {
            // Check if task is blocked
            if new_status == "in_progress" {
                let task = tasks.iter().find(|t| t.id == args.id).ok_or_else(|| {
                    ToolError::InvalidArgs(format!("task #{} not found", args.id))
                })?;
                let incomplete_blockers: Vec<u32> = task
                    .blocked_by
                    .iter()
                    .filter(|&&bid| {
                        tasks
                            .iter()
                            .find(|t| t.id == bid)
                            .map(|t| t.status != "completed")
                            .unwrap_or(false)
                    })
                    .copied()
                    .collect();
                if !incomplete_blockers.is_empty() {
                    return Err(ToolError::InvalidArgs(format!(
                        "task #{} is blocked by incomplete tasks: {:?}",
                        args.id, incomplete_blockers
                    )));
                }
            }

            let task = tasks
                .iter_mut()
                .find(|t| t.id == args.id)
                .ok_or_else(|| ToolError::InvalidArgs(format!("task #{} not found", args.id)))?;
            let old = task.status.clone();
            task.status = new_status.clone();
            out.push_str(&format!("task #{}: {old} → {new_status}\n", args.id));
        }

        if out.is_empty() {
            return Err(ToolError::InvalidArgs(
                "provide status and/or dependency changes".into(),
            ));
        }

        write_tasks(&ctx.workspace_root, &tasks)?;
        Ok(out)
    }
}

pub struct ListTasks;

#[async_trait]
impl Tool for ListTasks {
    fn name(&self) -> &str {
        "list_tasks"
    }
    fn description(&self) -> &str {
        "List all tasks and their status."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let tasks = read_tasks(&ctx.workspace_root);
        if tasks.is_empty() {
            return Ok("(no tasks)\n".to_string());
        }
        let mut out = String::new();
        for t in &tasks {
            let marker = match t.status.as_str() {
                "completed" => "✓",
                "in_progress" => "→",
                _ => "·",
            };
            let deps = if t.blocked_by.is_empty() {
                String::new()
            } else {
                let dep_strs: Vec<String> =
                    t.blocked_by.iter().map(|id| format!("#{id}")).collect();
                format!(" (blocked by {})", dep_strs.join(", "))
            };
            out.push_str(&format!(
                "  {marker} #{}: {} [{}]{deps}\n",
                t.id, t.description, t.status
            ));
        }
        Ok(out)
    }
}
