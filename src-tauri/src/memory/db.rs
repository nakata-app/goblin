use rusqlite::{Connection, params};
use std::sync::Mutex;
use std::path::PathBuf;
use super::embed::{EmbeddingClient, cosine_similarity};

pub struct MemoryDb {
    conn: Mutex<Connection>,
    embedding: Option<EmbeddingClient>,
}

impl MemoryDb {
    pub fn open(db_path: &str) -> Result<Self, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("Pragma error: {}", e))?;

        Ok(Self { conn: Mutex::new(conn), embedding: None })
    }

    pub fn set_embedding(&mut self, client: EmbeddingClient) {
        self.embedding = Some(client);
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

            CREATE TABLE IF NOT EXISTS embeddings (
                memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                vector BLOB NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, ns, content=memories, content_rowid=rowid);

            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, text, ns) VALUES (new.rowid, new.text, new.ns);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, text, ns) VALUES('delete', old.rowid, old.text, old.ns);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, text, ns) VALUES('delete', old.rowid, old.text, old.ns);
                INSERT INTO memories_fts(rowid, text, ns) VALUES (new.rowid, new.text, new.ns);
            END;"
        ).map_err(|e| format!("Schema init error: {}", e))?;

        // Backfill memories_fts for databases created before triggers existed
        let fts_count: i32 = conn
            .query_row("SELECT COUNT(*) FROM memories_fts", [], |r| r.get(0))
            .unwrap_or(0);
        let mem_count: i32 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap_or(0);
        if mem_count > 0 && fts_count < mem_count {
            conn.execute(
                "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')",
                [],
            ).ok();
        }

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

        // Generate and store embedding if configured
        if let Some(ref emb) = self.embedding {
            match emb.embed_blocking(text) {
                Ok(vector) => {
                    let blob = serialize_f32_vec(&vector);
                    conn.execute(
                        "INSERT INTO embeddings (memory_id, vector) VALUES (?1, ?2)",
                        params![id, blob],
                    ).ok();
                }
                Err(e) => {
                    eprintln!("[memory] embedding gen failed for '{}': {}", id, e);
                }
            }
        }

        Ok(())
    }

    /// Hybrid search: combines FTS5 (BM25 keyword ranking) with semantic
    /// embedding similarity via Reciprocal Rank Fusion (RRF). Falls back to
    /// FTS5-only if embedding is not configured, and to LIKE if FTS5 returns
    /// nothing usable (e.g. query that tokenizes to empty).
    pub fn search_memories(
        &self,
        ns: Option<&str>,
        query: &str,
        limit: i32,
    ) -> Result<Vec<MemoryRecord>, String> {
        let fetch = (limit * 4).max(20);
        let fts_hits = self.fts_search(ns, query, fetch).unwrap_or_default();

        let semantic_hits = if let Some(ref emb) = self.embedding {
            match emb.embed_blocking(query) {
                Ok(q) => self.semantic_search(ns, &q, fetch).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        if fts_hits.is_empty() && semantic_hits.is_empty() {
            return self.like_search(ns, query, limit);
        }

        // Reciprocal Rank Fusion: score(d) = Σ 1 / (k + rank_i(d)). k=60 is the
        // value from the original Cormack et al. paper; it down-weights the
        // first-place advantage so neither source dominates.
        let k: f32 = 60.0;
        use std::collections::HashMap;
        let mut scores: HashMap<String, f32> = HashMap::new();
        let mut by_id: HashMap<String, MemoryRecord> = HashMap::new();

        for (rank, r) in fts_hits.into_iter().enumerate() {
            *scores.entry(r.id.clone()).or_insert(0.0) += 1.0 / (k + (rank as f32 + 1.0));
            by_id.entry(r.id.clone()).or_insert(r);
        }
        for (rank, r) in semantic_hits.into_iter().enumerate() {
            *scores.entry(r.id.clone()).or_insert(0.0) += 1.0 / (k + (rank as f32 + 1.0));
            by_id.entry(r.id.clone()).or_insert(r);
        }

        let mut fused: Vec<(f32, MemoryRecord)> = scores.into_iter()
            .filter_map(|(id, s)| by_id.remove(&id).map(|r| (s, r)))
            .collect();
        fused.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let results: Vec<MemoryRecord> = fused.into_iter()
            .take(limit as usize)
            .map(|(_, r)| r)
            .collect();

        // Bump access stats for returned records
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();
        for r in &results {
            conn.execute(
                "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id = ?2",
                params![now, r.id],
            ).ok();
        }

        Ok(results)
    }

    /// Sanitize a free-form query into a safe FTS5 MATCH expression by
    /// splitting on whitespace and wrapping each token as a phrase query.
    /// Returns None if no usable tokens remain.
    fn fts5_match_expr(query: &str) -> Option<String> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        if tokens.is_empty() { None } else { Some(tokens.join(" OR ")) }
    }

    fn fts_search(
        &self,
        ns: Option<&str>,
        query: &str,
        limit: i32,
    ) -> Result<Vec<MemoryRecord>, String> {
        let Some(match_expr) = Self::fts5_match_expr(query) else {
            return Ok(Vec::new());
        };

        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let (sql, use_ns) = if ns.is_some() {
            (
                "SELECT m.id, m.ns, m.tier, m.text, m.meta, m.created, m.last_accessed, m.access_count
                 FROM memories_fts
                 JOIN memories m ON m.rowid = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND m.ns = ?2
                 ORDER BY bm25(memories_fts) ASC
                 LIMIT ?3",
                true,
            )
        } else {
            (
                "SELECT m.id, m.ns, m.tier, m.text, m.meta, m.created, m.last_accessed, m.access_count
                 FROM memories_fts
                 JOIN memories m ON m.rowid = memories_fts.rowid
                 WHERE memories_fts MATCH ?1
                 ORDER BY bm25(memories_fts) ASC
                 LIMIT ?2",
                false,
            )
        };

        let mut stmt = conn.prepare(sql).map_err(|e| format!("Prepare error: {}", e))?;
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<MemoryRecord> {
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
        };

        let rows: Vec<rusqlite::Result<MemoryRecord>> = if use_ns {
            stmt.query_map(params![match_expr, ns.unwrap(), limit], map_row)
                .map_err(|e| format!("FTS query error: {}", e))?
                .collect()
        } else {
            stmt.query_map(params![match_expr, limit], map_row)
                .map_err(|e| format!("FTS query error: {}", e))?
                .collect()
        };

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    fn semantic_search(
        &self,
        ns: Option<&str>,
        query_vec: &[f32],
        limit: i32,
    ) -> Result<Vec<MemoryRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let now = current_timestamp();

        // Fetch all memories with embeddings for the namespace
        let sql = if ns.is_some() {
            "SELECT m.id, m.ns, m.tier, m.text, m.meta, m.created, m.last_accessed, m.access_count, e.vector
             FROM memories m
             INNER JOIN embeddings e ON m.id = e.memory_id
             WHERE m.ns = ?1
             ORDER BY m.tier DESC, m.last_accessed DESC"
        } else {
            "SELECT m.id, m.ns, m.tier, m.text, m.meta, m.created, m.last_accessed, m.access_count, e.vector
             FROM memories m
             INNER JOIN embeddings e ON m.id = e.memory_id
             ORDER BY m.tier DESC, m.last_accessed DESC"
        };

        let mut stmt = conn.prepare(sql).map_err(|e| format!("Prepare error: {}", e))?;

        let mut scored: Vec<(f32, MemoryRecord)> = Vec::new();

        let rows: Vec<rusqlite::Result<(f32, MemoryRecord)>> = if let Some(ns_val) = ns {
            stmt.query_map(params![ns_val], |row| {
                let blob: Vec<u8> = row.get(8)?;
                let vec = deserialize_f32_vec(&blob);
                let sim = cosine_similarity(query_vec, &vec);
                Ok((sim, MemoryRecord {
                    id: row.get(0)?,
                    ns: row.get(1)?,
                    tier: row.get(2)?,
                    text: row.get(3)?,
                    meta: row.get(4)?,
                    created: row.get(5)?,
                    last_accessed: row.get(6)?,
                    access_count: row.get(7)?,
                }))
            }).map_err(|e| format!("Query error: {}", e))?
            .collect()
        } else {
            stmt.query_map([], |row| {
                let blob: Vec<u8> = row.get(8)?;
                let vec = deserialize_f32_vec(&blob);
                let sim = cosine_similarity(query_vec, &vec);
                Ok((sim, MemoryRecord {
                    id: row.get(0)?,
                    ns: row.get(1)?,
                    tier: row.get(2)?,
                    text: row.get(3)?,
                    meta: row.get(4)?,
                    created: row.get(5)?,
                    last_accessed: row.get(6)?,
                    access_count: row.get(7)?,
                }))
            }).map_err(|e| format!("Query error: {}", e))?
            .collect()
        };

        for row in rows {
            if let Ok((sim, record)) = row {
                scored.push((sim, record));
            }
        }

        // Sort by similarity descending
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let results: Vec<MemoryRecord> = scored
            .into_iter()
            .take(limit as usize)
            .map(|(_, r)| r)
            .collect();

        // Access bookkeeping happens in search_memories (the public entry).
        let _ = now;

        Ok(results)
    }

    fn like_search(
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

    pub fn list_memories(&self, limit: i32, offset: i32) -> Result<Vec<MemoryRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT id, ns, tier, text, meta, created, last_accessed, access_count
             FROM memories
             ORDER BY last_accessed DESC, created DESC
             LIMIT ?1 OFFSET ?2"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![limit, offset], |row| {
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

    pub fn list_observations(&self, limit: i32) -> Result<Vec<ObservationRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT id, ts, session_id, tool_name, args_summary, result_summary, success
             FROM observations
             ORDER BY ts DESC
             LIMIT ?1"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(ObservationRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                session_id: row.get(2)?,
                tool_name: row.get(3)?,
                args_summary: row.get(4)?,
                result_summary: row.get(5)?,
                success: row.get::<_, i32>(6)? != 0,
            })
        }).map_err(|e| format!("Query error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn list_learned_detailed(&self, limit: i32) -> Result<Vec<LearnedRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT id, preference, reinforcement_count, last_seen
             FROM learned
             ORDER BY reinforcement_count DESC, last_seen DESC
             LIMIT ?1"
        ).map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(LearnedRecord {
                id: row.get(0)?,
                preference: row.get(1)?,
                reinforcement_count: row.get(2)?,
                last_seen: row.get(3)?,
            })
        }).map_err(|e| format!("Query error: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservationRecord {
    pub id: String,
    pub ts: i64,
    pub session_id: String,
    pub tool_name: String,
    pub args_summary: Option<String>,
    pub result_summary: Option<String>,
    pub success: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LearnedRecord {
    pub id: String,
    pub preference: String,
    pub reinforcement_count: i32,
    pub last_seen: i64,
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

fn serialize_f32_vec(vec: &[f32]) -> Vec<u8> {
    vec.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect()
}

fn deserialize_f32_vec(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().unwrap();
            f32::from_le_bytes(arr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> MemoryDb {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let db = MemoryDb { conn: Mutex::new(conn), embedding: None };
        db.init_schema().unwrap();
        db
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

    #[test]
    fn serialize_deserialize_round_trip() {
        let original = vec![1.0, -2.5, 0.0, 3.14_f32];
        let blob = serialize_f32_vec(&original);
        let restored = deserialize_f32_vec(&blob);
        assert_eq!(original.len(), restored.len());
        for (a, b) in original.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 1e-6, "{} != {}", a, b);
        }
    }

    #[test]
    fn serialize_deserialize_empty() {
        let blob = serialize_f32_vec(&[]);
        let restored = deserialize_f32_vec(&blob);
        assert!(restored.is_empty());
    }

    #[test]
    fn search_falls_back_to_like_when_no_embedding() {
        let db = in_memory_db();
        db.add_memory("m1", "ns:test", 1, "Rust programming language", None).unwrap();
        db.add_memory("m2", "ns:test", 1, "Python scripting", None).unwrap();

        let results = db.search_memories(Some("ns:test"), "Python", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "Python scripting");
    }

    #[test]
    fn search_no_ns_returns_all_matching() {
        let db = in_memory_db();
        db.add_memory("a", "ns:a", 1, "common word", None).unwrap();
        db.add_memory("b", "ns:b", 1, "another common", None).unwrap();

        let results = db.search_memories(None, "common", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    // ── FTS5 + hybrid scoring ──

    #[test]
    fn fts_search_finds_by_keyword() {
        let db = in_memory_db();
        db.add_memory("k1", "ns:t", 1, "the cat sat on the mat", None).unwrap();
        db.add_memory("k2", "ns:t", 1, "dogs are loyal animals", None).unwrap();

        let results = db.fts_search(Some("ns:t"), "cat", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "k1");
    }

    #[test]
    fn fts_search_ignores_special_chars() {
        let db = in_memory_db();
        db.add_memory("s1", "ns:t", 1, "hello world", None).unwrap();
        // FTS5 raw `*` would be a syntax error; sanitizer must strip it.
        let results = db.fts_search(Some("ns:t"), "*** hello ***", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fts_search_empty_query_returns_empty() {
        let db = in_memory_db();
        db.add_memory("e1", "ns:t", 1, "anything", None).unwrap();
        let results = db.fts_search(Some("ns:t"), "!!!", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn fts_sync_on_delete() {
        let db = in_memory_db();
        db.add_memory("d1", "ns:t", 1, "unique-token-foo", None).unwrap();
        assert_eq!(db.fts_search(Some("ns:t"), "unique-token-foo", 10).unwrap().len(), 1);

        db.remove_memory("d1").unwrap();
        assert!(db.fts_search(Some("ns:t"), "unique-token-foo", 10).unwrap().is_empty());
    }

    #[test]
    fn fts_search_ranks_by_relevance() {
        let db = in_memory_db();
        // Two entries with "rust" but k2 has it more times; BM25 should rank
        // the more-frequent one higher even with shorter text.
        db.add_memory("k1", "ns:t", 1, "I sometimes write some rust code in the evening", None).unwrap();
        db.add_memory("k2", "ns:t", 1, "rust rust rust ferris", None).unwrap();

        let results = db.fts_search(Some("ns:t"), "rust", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "k2", "more frequent term should rank first");
    }

    #[test]
    fn search_uses_fts_when_available() {
        // No embedding configured → search_memories should still benefit from
        // FTS5 ranking instead of the older LIKE fallback.
        let db = in_memory_db();
        db.add_memory("a", "ns:t", 1, "alpha bravo charlie", None).unwrap();
        db.add_memory("b", "ns:t", 1, "delta echo foxtrot", None).unwrap();

        let results = db.search_memories(Some("ns:t"), "bravo", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[test]
    fn fts5_match_expr_handles_punctuation() {
        // Tokens with punctuation around them must still be matchable.
        assert!(MemoryDb::fts5_match_expr("hello, world!").is_some());
        // All-punctuation input must yield None (so we fall back to LIKE).
        assert!(MemoryDb::fts5_match_expr("!@#$%^&*()").is_none());
        // Embedded quotes must not break the generated MATCH expression.
        let expr = MemoryDb::fts5_match_expr("say \"hi\"").unwrap();
        assert!(expr.contains("OR") || !expr.is_empty());
    }
}
