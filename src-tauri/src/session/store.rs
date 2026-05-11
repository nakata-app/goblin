use rusqlite::{Connection, params};
use std::sync::Mutex;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub title: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub cost: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub messages: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub model: Option<String>,
    pub message_count: i64,
    pub cost: f64,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

impl SessionStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    pub fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                model TEXT,
                provider TEXT,
                cost REAL DEFAULT 0,
                tokens_in INTEGER DEFAULT 0,
                tokens_out INTEGER DEFAULT 0,
                messages TEXT
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(title, messages, content=sessions, content_rowid=rowid);"
        ).map_err(|e| format!("Session schema error: {}", e))?;
        Ok(())
    }

    pub fn create(&self, id: &str, model: Option<&str>, provider: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        conn.execute(
            "INSERT INTO sessions (id, started_at, model, provider) VALUES (?1, ?2, ?3, ?4)",
            params![id, now, model, provider],
        ).map_err(|e| format!("Create session error: {}", e))?;
        Ok(())
    }

    pub fn end(&self, id: &str, messages_jsonl: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        let msg_count = messages_jsonl.lines().count() as i64;

        conn.execute(
            "UPDATE sessions SET ended_at = ?1, messages = ?2 WHERE id = ?3",
            params![now, messages_jsonl, id],
        ).map_err(|e| format!("End session error: {}", e))?;
        Ok(())
    }

    pub fn update_stats(
        &self,
        id: &str,
        tokens_in: i64,
        tokens_out: i64,
        cost: f64,
        model: &str,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute(
            "UPDATE sessions SET tokens_in = tokens_in + ?1, tokens_out = tokens_out + ?2, cost = cost + ?3, model = ?4 WHERE id = ?5",
            params![tokens_in, tokens_out, cost, model, id],
        ).map_err(|e| format!("Update stats error: {}", e))?;
        Ok(())
    }

    pub fn update_title(&self, id: &str, title: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![title, id],
        ).map_err(|e| format!("Update title error: {}", e))?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<SessionRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, started_at, ended_at, model, provider, cost, tokens_in, tokens_out, messages
             FROM sessions WHERE id = ?1"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let result = stmt.query_row(params![id], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                title: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                cost: row.get(6)?,
                tokens_in: row.get(7)?,
                tokens_out: row.get(8)?,
                messages: row.get(9)?,
            })
        });

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Get session error: {}", e)),
        }
    }

    pub fn list(&self, limit: i32) -> Result<Vec<SessionSummary>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, started_at, ended_at, model, cost, messages
             FROM sessions ORDER BY started_at DESC LIMIT ?1"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![limit], |row| {
            let messages: Option<String> = row.get(6)?;
            let count = messages
                .as_ref()
                .map(|m| m.lines().count() as i64)
                .unwrap_or(0);
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                model: row.get(4)?,
                message_count: count,
                cost: row.get(5)?,
            })
        }).map_err(|e| format!("List error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let affected = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| format!("Delete error: {}", e))?;
        Ok(affected > 0)
    }

    pub fn search(&self, query: &str, limit: i32) -> Result<Vec<SessionSummary>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let ids = crate::session::search::search_sessions(&conn, query, limit)?;

        let mut results = Vec::new();
        for id in ids {
            let mut stmt = conn.prepare(
                "SELECT id, title, started_at, ended_at, model, cost, messages
                 FROM sessions WHERE id = ?1"
            ).map_err(|e| format!("Prepare error: {}", e))?;

            if let Ok(row) = stmt.query_row(params![id], |row| {
                let messages: Option<String> = row.get(6)?;
                let count = messages
                    .as_ref()
                    .map(|m| m.lines().count() as i64)
                    .unwrap_or(0);
                Ok(SessionSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    started_at: row.get(2)?,
                    ended_at: row.get(3)?,
                    model: row.get(4)?,
                    message_count: count,
                    cost: row.get(5)?,
                })
            }) {
                results.push(row);
            }
        }
        Ok(results)
    }
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
