//! User-facing task management for `goblin`.
//!
//! Simple, file-backed task list stored in `.aegis/tasks.json`. Each task
//! has an integer id, text description, and a done/not-done flag. No
//! priorities, no dates, no categories — deliberately minimal.
//!
//! CLI: `aegis tasks`, `aegis tasks add "X"`, `aegis tasks done 3`,
//!       `aegis tasks rm 3`, `aegis tasks clear`.
//!
//! REPL: `/tasks`, `/task add X`, `/task done 3`, `/task rm 3`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserTask {
    pub id: u32,
    pub text: String,
    pub done: bool,
    pub created_at: String,
}

/// Load tasks from `.metis/tasks_user.json`. Returns an empty vec on
/// missing or malformed file. We use a separate file from the AI-facing
/// `tasks.json` to avoid collisions.
pub fn load_tasks(workspace: &Path) -> Vec<UserTask> {
    let path = workspace.join(".metis").join("tasks_user.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Snapshot of an agent-managed task (written by the `create_task` /
/// `update_task` tools to `.metis/tasks.json`). Sidebar reads this each
/// frame so the "Todo" card live-updates as the agent ticks tasks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentTask {
    pub id: u32,
    pub description: String,
    pub status: String, // "pending", "in_progress", "completed"
}

/// Load agent task list from `.metis/tasks.json` for sidebar render.
/// Tolerates extra fields (blocks/blocked_by) by ignoring them. Empty
/// vec on missing or malformed file.
pub fn load_agent_tasks(workspace: &Path) -> Vec<AgentTask> {
    let path = workspace.join(".metis").join("tasks.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist tasks to `.metis/tasks_user.json`.
pub fn save_tasks(workspace: &Path, tasks: &[UserTask]) -> Result<()> {
    let dir = workspace.join(".metis");
    std::fs::create_dir_all(&dir).context("could not create .metis directory")?;
    let json = serde_json::to_string_pretty(tasks).context("could not serialise tasks")?;
    std::fs::write(dir.join("tasks_user.json"), json).context("could not write tasks file")
}

/// Add a task and return its assigned id.
pub fn add_task(workspace: &Path, text: &str) -> Result<u32> {
    let mut tasks = load_tasks(workspace);
    let id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    tasks.push(UserTask {
        id,
        text: text.to_string(),
        done: false,
        created_at: {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}", d.as_secs())
        },
    });
    save_tasks(workspace, &tasks)?;
    Ok(id)
}

/// Mark a task as done. Returns the task text on success.
pub fn complete_task(workspace: &Path, id: u32) -> Result<String> {
    let mut tasks = load_tasks(workspace);
    let task = tasks
        .iter_mut()
        .find(|t| t.id == id)
        .with_context(|| format!("task #{id} not found"))?;
    task.done = true;
    let text = task.text.clone();
    save_tasks(workspace, &tasks)?;
    Ok(text)
}

/// Delete a task by id. Returns the task text on success.
pub fn delete_task(workspace: &Path, id: u32) -> Result<String> {
    let mut tasks = load_tasks(workspace);
    let pos = tasks
        .iter()
        .position(|t| t.id == id)
        .with_context(|| format!("task #{id} not found"))?;
    let text = tasks[pos].text.clone();
    tasks.remove(pos);
    save_tasks(workspace, &tasks)?;
    Ok(text)
}

/// Remove all completed tasks. Returns how many were removed.
pub fn clear_done(workspace: &Path) -> Result<usize> {
    let tasks = load_tasks(workspace);
    let before = tasks.len();
    let remaining: Vec<UserTask> = tasks.into_iter().filter(|t| !t.done).collect();
    let removed = before - remaining.len();
    save_tasks(workspace, &remaining)?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

const TURQUOISE: &str = "\x1b[38;2;0;229;209m";
const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Format the task list for terminal output.
pub fn format_task_list(tasks: &[UserTask]) -> String {
    if tasks.is_empty() {
        return format!("{DIM}(no tasks){RESET}\n");
    }
    let mut out = format!("{TURQUOISE}Tasks:{RESET}\n");
    for t in tasks {
        if t.done {
            out.push_str(&format!(
                "  {DIM}{:>3}. [{GREEN}✓{RESET}{DIM}] {}{RESET}\n",
                t.id, t.text
            ));
        } else {
            out.push_str(&format!("  {:>3}. [ ] {}\n", t.id, t.text));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CLI subcommand dispatcher
// ---------------------------------------------------------------------------

/// Handle `metis tasks [subcommand] [args...]`.
/// Returns Ok(()) on success, prints to stderr.
pub fn run_tasks_command(args: &[&str], workspace: &Path) -> Result<()> {
    match args.first().copied() {
        None | Some("list") => {
            let tasks = load_tasks(workspace);
            eprint!("{}", format_task_list(&tasks));
            Ok(())
        }
        Some("add") => {
            let text = args[1..].join(" ");
            if text.is_empty() {
                bail!("usage: metis tasks add <description>");
            }
            let id = add_task(workspace, &text)?;
            eprintln!("{TURQUOISE}+{RESET} task #{id}: {text}");
            Ok(())
        }
        Some("done") => {
            let id: u32 = args
                .get(1)
                .with_context(|| "usage: metis tasks done <id>")?
                .parse()
                .context("task id must be a number")?;
            let text = complete_task(workspace, id)?;
            eprintln!("{GREEN}✓{RESET} task #{id}: {text}");
            Ok(())
        }
        Some("rm") | Some("remove") | Some("delete") => {
            let id: u32 = args
                .get(1)
                .with_context(|| "usage: metis tasks rm <id>")?
                .parse()
                .context("task id must be a number")?;
            let text = delete_task(workspace, id)?;
            eprintln!("{DIM}✗ removed task #{id}: {text}{RESET}");
            Ok(())
        }
        Some("clear") => {
            let removed = clear_done(workspace)?;
            if removed == 0 {
                eprintln!("{DIM}(no completed tasks to clear){RESET}");
            } else {
                eprintln!("{TURQUOISE}cleared {removed} completed task(s){RESET}");
            }
            Ok(())
        }
        Some(other) => {
            bail!("unknown tasks subcommand `{other}` — try: add, done, rm, clear");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Create a unique temp directory for each test.
    fn workspace() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("metis_task_test_{id}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_empty_returns_empty_vec() {
        let ws = workspace();
        assert!(load_tasks(&ws).is_empty());
    }

    #[test]
    fn add_and_load_roundtrip() {
        let ws = workspace();
        let id1 = add_task(&ws, "first task").unwrap();
        let id2 = add_task(&ws, "second task").unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);

        let tasks = load_tasks(&ws);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "first task");
        assert_eq!(tasks[1].text, "second task");
        assert!(!tasks[0].done);
        assert!(!tasks[1].done);
    }

    #[test]
    fn complete_task_marks_done() {
        let ws = workspace();
        add_task(&ws, "do something").unwrap();
        let text = complete_task(&ws, 1).unwrap();
        assert_eq!(text, "do something");

        let tasks = load_tasks(&ws);
        assert!(tasks[0].done);
    }

    #[test]
    fn complete_nonexistent_task_errors() {
        let ws = workspace();
        assert!(complete_task(&ws, 99).is_err());
    }

    #[test]
    fn delete_task_removes_it() {
        let ws = workspace();
        add_task(&ws, "keep").unwrap();
        add_task(&ws, "remove me").unwrap();
        let text = delete_task(&ws, 2).unwrap();
        assert_eq!(text, "remove me");

        let tasks = load_tasks(&ws);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "keep");
    }

    #[test]
    fn delete_nonexistent_task_errors() {
        let ws = workspace();
        assert!(delete_task(&ws, 42).is_err());
    }

    #[test]
    fn clear_done_removes_completed_only() {
        let ws = workspace();
        add_task(&ws, "pending").unwrap();
        add_task(&ws, "will complete").unwrap();
        add_task(&ws, "also pending").unwrap();
        complete_task(&ws, 2).unwrap();

        let removed = clear_done(&ws).unwrap();
        assert_eq!(removed, 1);

        let tasks = load_tasks(&ws);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| !t.done));
    }

    #[test]
    fn clear_done_when_none_completed() {
        let ws = workspace();
        add_task(&ws, "pending").unwrap();
        let removed = clear_done(&ws).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn auto_incrementing_ids_after_deletion() {
        let ws = workspace();
        add_task(&ws, "one").unwrap(); // id 1
        add_task(&ws, "two").unwrap(); // id 2
        delete_task(&ws, 2).unwrap();
        let id = add_task(&ws, "three").unwrap(); // should be 2 (max of remaining is 1, +1)
        assert_eq!(id, 2);
    }

    #[test]
    fn format_empty_list() {
        let output = format_task_list(&[]);
        assert!(output.contains("no tasks"));
    }

    #[test]
    fn format_list_shows_checkmarks() {
        let tasks = vec![
            UserTask {
                id: 1,
                text: "done item".into(),
                done: true,
                created_at: "2025-01-01T00:00:00Z".into(),
            },
            UserTask {
                id: 2,
                text: "pending item".into(),
                done: false,
                created_at: "2025-01-01T00:00:00Z".into(),
            },
        ];
        let output = format_task_list(&tasks);
        assert!(output.contains("Tasks:"));
        assert!(output.contains("✓"));
        assert!(output.contains("done item"));
        assert!(output.contains("[ ]"));
        assert!(output.contains("pending item"));
    }

    #[test]
    fn run_tasks_list_empty() {
        let ws = workspace();
        // Should not error even with no tasks
        run_tasks_command(&[], &ws).unwrap();
    }

    #[test]
    fn run_tasks_add_and_list() {
        let ws = workspace();
        run_tasks_command(&["add", "hello", "world"], &ws).unwrap();
        let tasks = load_tasks(&ws);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "hello world");
    }

    #[test]
    fn run_tasks_done_and_rm() {
        let ws = workspace();
        run_tasks_command(&["add", "task1"], &ws).unwrap();
        run_tasks_command(&["done", "1"], &ws).unwrap();
        assert!(load_tasks(&ws)[0].done);

        run_tasks_command(&["rm", "1"], &ws).unwrap();
        assert!(load_tasks(&ws).is_empty());
    }

    #[test]
    fn run_tasks_clear() {
        let ws = workspace();
        run_tasks_command(&["add", "a"], &ws).unwrap();
        run_tasks_command(&["add", "b"], &ws).unwrap();
        run_tasks_command(&["done", "1"], &ws).unwrap();
        run_tasks_command(&["clear"], &ws).unwrap();
        let tasks = load_tasks(&ws);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "b");
    }

    #[test]
    fn run_tasks_add_empty_errors() {
        let ws = workspace();
        assert!(run_tasks_command(&["add"], &ws).is_err());
    }

    #[test]
    fn run_tasks_unknown_subcommand_errors() {
        let ws = workspace();
        assert!(run_tasks_command(&["banana"], &ws).is_err());
    }

    // ---------- Agent task live-capture ----------

    #[test]
    fn load_agent_tasks_returns_empty_when_missing() {
        let ws = workspace();
        assert!(load_agent_tasks(&ws).is_empty());
    }

    #[test]
    fn load_agent_tasks_parses_core_tool_format() {
        let ws = workspace();
        let dir = ws.join(".metis");
        std::fs::create_dir_all(&dir).unwrap();
        // Mirrors what crates/core/src/tools/task.rs writes.
        let payload = r#"[
            {"id": 1, "description": "design schema", "status": "completed"},
            {"id": 2, "description": "write migration", "status": "in_progress"},
            {"id": 3, "description": "wire api", "status": "pending", "blocked_by": [2]}
        ]"#;
        std::fs::write(dir.join("tasks.json"), payload).unwrap();
        let tasks = load_agent_tasks(&ws);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[1].status, "in_progress");
        assert_eq!(tasks[2].description, "wire api");
    }

    #[test]
    fn load_agent_tasks_tolerates_malformed_json() {
        let ws = workspace();
        let dir = ws.join(".metis");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tasks.json"), "{not json").unwrap();
        assert!(load_agent_tasks(&ws).is_empty());
    }

    #[test]
    fn user_and_agent_task_files_are_independent() {
        let ws = workspace();
        add_task(&ws, "user todo").unwrap();
        let dir = ws.join(".metis");
        std::fs::write(
            dir.join("tasks.json"),
            r#"[{"id":1,"description":"agent todo","status":"pending"}]"#,
        )
        .unwrap();
        assert_eq!(load_tasks(&ws).len(), 1);
        assert_eq!(load_agent_tasks(&ws).len(), 1);
        assert_eq!(load_tasks(&ws)[0].text, "user todo");
        assert_eq!(load_agent_tasks(&ws)[0].description, "agent todo");
    }
}
