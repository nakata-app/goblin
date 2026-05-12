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

    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> MemoryDb {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // manual init to avoid dirs_path issues
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
            );"
        ).unwrap();
        MemoryDb { conn: Mutex::new(conn) }
    }

    #[test]
    fn add_and_search_memory() {
        let db = in_memory_db();
        db.add_memory("m1", "proj:test", 1, "remember this fact", None).unwrap();

        let results = db.search_memories(Some("proj:test"), "remember", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "remember this fact");
        assert_eq!(results[0].ns, "proj:test");
    }

    #[test]
    fn search_by_ns_filter() {
        let db = in_memory_db();
        db.add_memory("a", "ns:a", 1, "alpha", None).unwrap();
        db.add_memory("b", "ns:b", 1, "beta", None).unwrap();

        let results = db.search_memories(Some("ns:a"), "alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "alpha");
    }

    #[test]
    fn search_no_match() {
        let db = in_memory_db();
        db.add_memory("x", "ns:x", 1, "hello", None).unwrap();
        let results = db.search_memories(Some("ns:x"), "nonexistent", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn get_memories_by_ns() {
        let db = in_memory_db();
        db.add_memory("1", "ns:x", 2, "important", None).unwrap();
        db.add_memory("2", "ns:x", 1, "normal", None).unwrap();
        db.add_memory("3", "ns:y", 1, "other", None).unwrap();

        let results = db.get_memories_by_ns("ns:x", 10).unwrap();
        assert_eq!(results.len(), 2);
        // tier 2 first
        assert_eq!(results[0].text, "important");
    }

    #[test]
    fn remove_memory() {
        let db = in_memory_db();
        db.add_memory("r1", "ns:r", 1, "to remove", None).unwrap();
        assert!(db.remove_memory("r1").unwrap());
        assert!(!db.remove_memory("r1").unwrap());
    }

    #[test]
    fn memory_stats() {
        let db = in_memory_db();
        db.add_memory("s1", "ns:a", 1, "a1", None).unwrap();
        db.add_memory("s2", "ns:a", 1, "a2", None).unwrap();
        db.add_memory("s3", "ns:b", 1, "b1", None).unwrap();

        let stats = db.memory_stats().unwrap();
        assert_eq!(stats.total, 3);
        assert!(stats.by_ns.iter().any(|(ns, _)| ns == "ns:a"));
    }

    #[test]
    fn record_observation() {
        let db = in_memory_db();
        db.record_observation("obs1", "sess1", "read_file", Some("path: test.txt"), Some("result: ok"), true).unwrap();
        // No query method, but no error means success
    }

    #[test]
    fn reinforce_and_get_learned() {
        let db = in_memory_db();
        db.reinforce("avoid npm").unwrap();
        db.reinforce("avoid npm").unwrap();
        db.reinforce("use rust").unwrap();

        let learned = db.get_learned(10).unwrap();
        assert_eq!(learned.len(), 2);
        assert_eq!(learned[0], "avoid npm"); // more reinforcements first
    }

    #[test]
    fn compact_removes_old_tier1() {
        let db = in_memory_db();
        db.add_memory("c1", "ns:c", 1, "old", None).unwrap();

        // Set last_accessed to old timestamp directly
        {
            let conn = db.conn.lock().unwrap();
            let old_ts = current_timestamp() - 100 * 86400; // 100 days ago
            conn.execute("UPDATE memories SET last_accessed = ?1 WHERE id = 'c1'", rusqlite::params![old_ts]).unwrap();
        }

        let removed = db.compact(30).unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn compact_keeps_tier2() {
        let db = in_memory_db();
        db.add_memory("c2", "ns:c", 2, "important old", None).unwrap();

        {
            let conn = db.conn.lock().unwrap();
            let old_ts = current_timestamp() - 100 * 86400;
            conn.execute("UPDATE memories SET last_accessed = ?1 WHERE id = 'c2'", rusqlite::params![old_ts]).unwrap();
        }

        let removed = db.compact(30).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn add_memory_duplicate_id_fails() {
        let db = in_memory_db();
        db.add_memory("dup", "ns:d", 1, "first", None).unwrap();
        let result = db.add_memory("dup", "ns:d", 1, "second", None);
        assert!(result.is_err());
    }
}
