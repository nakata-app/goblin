use rusqlite::{Connection, params};
use std::sync::Mutex;
use std::path::PathBuf;

pub struct MemoryDb {
    conn: Mutex<Connection>,
}

impl MemoryDb {
    pub fn open(db_path: &str) -> Result<Self, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("Pragma error: {}", e))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn default_path() -> PathBuf {
        let mut path = dirs_path();
        path.push("memory.db");
        path
    }

    pub fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                ns TEXT NOT NULL,
                tier INTEGER DEFAULT 1,
                text TEXT NOT NULL,
                meta TEXT,
                created INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL,
                access_count INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS observations (
                id TEXT PRIMARY KEY,
                ts INTEGER NOT NULL,
                session_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                args_summary TEXT,
                result_summary TEXT,
                success INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS learned (
                id TEXT PRIMARY KEY,
                preference TEXT NOT NULL,
                reinforcement_count INTEGER DEFAULT 1,
                last_seen INTEGER NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, ns, content=memories, content_rowid=rowid);"
        ).map_err(|e| format!("Schema init error: {}", e))?;

        Ok(())
    }

    // ---- Memory CRUD ----

    pub fn add_memory(
        &self,
        id: &str,
        ns: &str,
        tier: i32,
        text: &str,
        meta: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();

        conn.execute(
            "INSERT INTO memories (id, ns, tier, text, meta, created, last_accessed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, ns, tier, text, meta, now],
        ).map_err(|e| format!("Insert error: {}", e))?;

        Ok(())
    }

    pub fn search_memories(
        &self,
        ns: Option<&str>,
        query: &str,
        limit: i32,
    ) -> Result<Vec<MemoryRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();

        let (sql, params_vals): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(ns_val) = ns {
            (
                "SELECT id, ns, tier, text, meta, created, last_accessed, access_count
                 FROM memories
                 WHERE ns = ?1 AND text LIKE ?2
                 ORDER BY tier DESC, last_accessed DESC
                 LIMIT ?3".to_string(),
                vec![
                    Box::new(ns_val.to_string()),
                    Box::new(format!("%{}%", query)),
                    Box::new(limit),
                ],
            )
        } else {
            (
                "SELECT id, ns, tier, text, meta, created, last_accessed, access_count
                 FROM memories
                 WHERE text LIKE ?1
                 ORDER BY tier DESC, last_accessed DESC
                 LIMIT ?2".to_string(),
                vec![
                    Box::new(format!("%{}%", query)),
                    Box::new(limit),
                ],
            )
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vals.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare error: {}", e))?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(MemoryRecord {
                id: row.get(0)?,
                ns: row.get(1)?,
                tier: row.get(2)?,
                text: row.get(3)?,
                meta: row.get(4)?,
                created: row.get(5)?,
                last_accessed: row.get(6)?,
                access_count: row.get(7)?,
            })
        }).map_err(|e| format!("Query error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }

        // Update last_accessed and access_count for found memories
        for record in &results {
            conn.execute(
                "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id = ?2",
                params![now, record.id],
            ).ok();
        }

        Ok(results)
    }

    pub fn get_memories_by_ns(&self, ns: &str, limit: i32) -> Result<Vec<MemoryRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT id, ns, tier, text, meta, created, last_accessed, access_count
             FROM memories
             WHERE ns = ?1
             ORDER BY tier DESC, last_accessed DESC
             LIMIT ?2"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![ns, limit], |row| {
            Ok(MemoryRecord {
                id: row.get(0)?,
                ns: row.get(1)?,
                tier: row.get(2)?,
                text: row.get(3)?,
                meta: row.get(4)?,
                created: row.get(5)?,
                last_accessed: row.get(6)?,
                access_count: row.get(7)?,
            })
        }).map_err(|e| format!("Query error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn remove_memory(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let affected = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])
            .map_err(|e| format!("Delete error: {}", e))?;
        Ok(affected > 0)
    }

    pub fn memory_stats(&self) -> Result<MemoryStats, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let total: i32 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .map_err(|e| format!("Count error: {}", e))?;

        let by_ns: Vec<(String, i32)> = {
            let mut stmt = conn.prepare(
                "SELECT ns, COUNT(*) as cnt FROM memories GROUP BY ns ORDER BY cnt DESC"
            ).map_err(|e| format!("Prepare error: {}", e))?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
            }).map_err(|e| format!("Query error: {}", e))?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(|e| format!("Row error: {}", e))?);
            }
            results
        };

        Ok(MemoryStats { total, by_ns })
    }

    // ---- Observations ----

    pub fn record_observation(
        &self,
        id: &str,
        session_id: &str,
        tool_name: &str,
        args_summary: Option<&str>,
        result_summary: Option<&str>,
        success: bool,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();

        conn.execute(
            "INSERT INTO observations (id, ts, session_id, tool_name, args_summary, result_summary, success)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, now, session_id, tool_name, args_summary, result_summary, success],
        ).map_err(|e| format!("Insert observation error: {}", e))?;

        Ok(())
    }

    // ---- Learned (reinforcement) ----

    pub fn reinforce(&self, preference: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        let id = format!("learn_{}", simple_hash(preference));

        conn.execute(
            "INSERT INTO learned (id, preference, reinforcement_count, last_seen)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(id) DO UPDATE SET
               reinforcement_count = reinforcement_count + 1,
               last_seen = ?3",
            params![id, preference, now],
        ).map_err(|e| format!("Reinforce error: {}", e))?;

        Ok(())
    }

    pub fn get_learned(&self, limit: i32) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT preference FROM learned ORDER BY reinforcement_count DESC LIMIT ?1"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![limit], |row| row.get::<_, String>(0))
            .map_err(|e| format!("Query error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    // ---- Compact ----

    pub fn compact(&self, days_old: i32) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let cutoff = current_timestamp() - (days_old as i64 * 86400);

        let count = conn.execute(
            "DELETE FROM memories WHERE tier = 1 AND last_accessed < ?1",
            params![cutoff],
        ).map_err(|e| format!("Compact error: {}", e))?;

        Ok(count)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryRecord {
    pub id: String,
    pub ns: String,
    pub tier: i32,
    pub text: String,
    pub meta: Option<String>,
    pub created: i64,
    pub last_accessed: i64,
    pub access_count: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryStats {
    pub total: i32,
    pub by_ns: Vec<(String, i32)>,
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn dirs_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".goblin");
        return p;
    }
    PathBuf::from(".")
}

fn simple_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}
