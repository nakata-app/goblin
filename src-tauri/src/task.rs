use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub status: String,
    pub prompt: Option<String>,
    pub result: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub parent_id: Option<String>,
    pub depth: i32,
    pub agent_type: Option<String>,
}

#[derive(Clone)]
pub struct TaskStore {
    conn: Arc<Mutex<Connection>>,
}

impl TaskStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    pub fn new_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory db: {}", e))?;
        let store = Self { conn: Arc::new(Mutex::new(conn)) };
        store.init_schema()?;
        Ok(store)
    }

    pub fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                prompt TEXT,
                result TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);"
        ).map_err(|e| format!("Task schema error: {}", e))?;

        // Migrations
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN prompt TEXT;");
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN parent_id TEXT;");
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN depth INTEGER DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN agent_type TEXT;");
        let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);");
        let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_tasks_depth ON tasks(depth);");

        Ok(())
    }

    pub fn list(&self, session_id: &str) -> Result<Vec<TaskRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, name, status, prompt, result, created_at, updated_at,
                    COALESCE(parent_id, '') as parent_id, COALESCE(depth, 0) as depth, agent_type
             FROM tasks WHERE session_id = ?1 ORDER BY depth ASC, created_at ASC"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![session_id], |row| {
            Ok(TaskRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
                prompt: row.get(4)?,
                result: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                parent_id: row.get::<_, String>(8).ok().filter(|s| !s.is_empty()),
                depth: row.get(9)?,
                agent_type: row.get(10)?,
            })
        }).map_err(|e| format!("List tasks error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn list_pending(&self) -> Result<Vec<TaskRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, name, status, prompt, result, created_at, updated_at,
                    COALESCE(parent_id, '') as parent_id, COALESCE(depth, 0) as depth, agent_type
             FROM tasks WHERE status = 'pending' ORDER BY depth ASC, created_at ASC LIMIT 5"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map([], |row| {
            Ok(TaskRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
                prompt: row.get(4)?,
                result: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                parent_id: row.get::<_, String>(8).ok().filter(|s| !s.is_empty()),
                depth: row.get(9)?,
                agent_type: row.get(10)?,
            })
        }).map_err(|e| format!("List pending error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn upsert(&self, session_id: &str, id: &str, name: &str, status: &str, result: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        conn.execute(
            "INSERT INTO tasks (id, session_id, name, status, result, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET name=?3, status=?4, result=?5, updated_at=?6",
            params![id, session_id, name, status, result, now],
        ).map_err(|e| format!("Upsert task error: {}", e))?;
        Ok(())
    }

    pub fn upsert_subtask(
        &self,
        session_id: &str,
        id: &str,
        name: &str,
        status: &str,
        prompt: Option<&str>,
        parent_id: Option<&str>,
        depth: i32,
        agent_type: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();

        // Check depth limit
        if depth > 5 {
            return Err(format!("Max sub-agent depth (5) exceeded. Current depth: {}", depth));
        }

        // Count active children under this parent
        if let Some(pid) = parent_id {
            let sibling_count: i32 = conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_id = ?1 AND status IN ('pending', 'running')",
                params![pid],
                |row| row.get(0),
            ).unwrap_or(0);
            if sibling_count >= 5 {
                return Err(format!("Max children (5) per agent reached for parent {}", pid));
            }
        }

        conn.execute(
            "INSERT INTO tasks (id, session_id, name, status, prompt, parent_id, depth, agent_type, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(id) DO UPDATE SET name=?3, status=?4, prompt=?5, parent_id=?6, depth=?7, agent_type=?8, updated_at=?9",
            params![id, session_id, name, status, prompt, parent_id, depth, agent_type, now],
        ).map_err(|e| format!("Upsert subtask error: {}", e))?;
        Ok(())
    }

    pub fn task_tree(&self, session_id: &str) -> Result<Vec<TaskTree>, String> {
        let tasks = self.list(session_id)?;
        Ok(build_tree(&tasks))
    }

    pub fn upsert_with_prompt(&self, session_id: &str, id: &str, name: &str, status: &str, prompt: Option<&str>, result: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        conn.execute(
            "INSERT INTO tasks (id, session_id, name, status, prompt, result, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(id) DO UPDATE SET name=?3, status=?4, prompt=?5, result=?6, updated_at=?7",
            params![id, session_id, name, status, prompt, result, now],
        ).map_err(|e| format!("Upsert task error: {}", e))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let affected = conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])
            .map_err(|e| format!("Delete task error: {}", e))?;
        Ok(affected > 0)
    }

    pub fn clear_session(&self, session_id: &str) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let affected = conn.execute("DELETE FROM tasks WHERE session_id = ?1", params![session_id])
            .map_err(|e| format!("Clear tasks error: {}", e))?;
        Ok(affected)
    }
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTree {
    pub task: TaskRecord,
    pub children: Vec<TaskTree>,
}

fn build_tree(tasks: &[TaskRecord]) -> Vec<TaskTree> {
    let mut roots: Vec<TaskTree> = Vec::new();
    for task in tasks {
        if task.parent_id.is_none() || task.parent_id.as_deref() == Some("") {
            let children = build_children(task, tasks);
            roots.push(TaskTree {
                task: task.clone(),
                children,
            });
        }
    }
    roots
}

fn build_children(parent: &TaskRecord, all: &[TaskRecord]) -> Vec<TaskTree> {
    all.iter()
        .filter(|t| t.parent_id.as_deref() == Some(&parent.id))
        .map(|t| {
            let children = build_children(t, all);
            TaskTree {
                task: t.clone(),
                children,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_store() -> TaskStore {
        TaskStore::new_in_memory().unwrap()
    }

    #[test]
    fn upsert_and_list_tasks() {
        let store = in_memory_store();
        store.upsert("s1", "t1", "read_file", "running", None).unwrap();
        store.upsert("s1", "t2", "write_file", "done", Some("success")).unwrap();

        let list = store.list("s1").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "read_file");
        assert_eq!(list[0].status, "running");
        assert_eq!(list[1].result.as_deref(), Some("success"));
        // Hierarchy fields defaulted
        assert!(list[0].parent_id.is_none());
        assert_eq!(list[0].depth, 0);
    }

    #[test]
    fn upsert_updates_existing() {
        let store = in_memory_store();
        store.upsert("s1", "t1", "bash", "running", None).unwrap();
        store.upsert("s1", "t1", "bash", "done", Some("output")).unwrap();

        let list = store.list("s1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, "done");
        assert_eq!(list[0].result.as_deref(), Some("output"));
    }

    #[test]
    fn session_scoped() {
        let store = in_memory_store();
        store.upsert("s1", "t1", "task_a", "pending", None).unwrap();
        store.upsert("s2", "t2", "task_b", "pending", None).unwrap();

        assert_eq!(store.list("s1").unwrap().len(), 1);
        assert_eq!(store.list("s2").unwrap().len(), 1);
        assert_eq!(store.list("s3").unwrap().len(), 0);
    }

    #[test]
    fn delete_task() {
        let store = in_memory_store();
        store.upsert("s1", "t1", "grep", "done", None).unwrap();
        assert!(store.delete("t1").unwrap());
        assert!(!store.delete("t1").unwrap());
        assert_eq!(store.list("s1").unwrap().len(), 0);
    }

    #[test]
    fn clear_session() {
        let store = in_memory_store();
        store.upsert("s1", "t1", "a", "pending", None).unwrap();
        store.upsert("s1", "t2", "b", "pending", None).unwrap();
        store.upsert("s2", "t3", "c", "pending", None).unwrap();

        let cleared = store.clear_session("s1").unwrap();
        assert_eq!(cleared, 2);
        assert_eq!(store.list("s1").unwrap().len(), 0);
        assert_eq!(store.list("s2").unwrap().len(), 1);
    }

    #[test]
    fn subtask_hierarchy() {
        let store = in_memory_store();
        store.upsert_subtask("s1", "parent", "Analyze", "pending", Some("analyze code"), None, 0, Some("explore")).unwrap();
        store.upsert_subtask("s1", "child1", "Fix bug", "pending", Some("fix null ptr"), Some("parent"), 1, Some("general")).unwrap();
        store.upsert_subtask("s1", "child2", "Write test", "pending", Some("add tests"), Some("parent"), 1, Some("general")).unwrap();

        let tree = store.task_tree("s1").unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].task.name, "Analyze");
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].task.name, "Fix bug");
        assert_eq!(tree[0].children[0].task.depth, 1);
    }

    #[test]
    fn subtask_depth_limit() {
        let store = in_memory_store();
        let result = store.upsert_subtask("s1", "deep", "Task", "pending", Some("work"), Some("parent"), 6, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("depth"));
    }
}
