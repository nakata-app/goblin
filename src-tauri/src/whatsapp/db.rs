use rusqlite::{Connection, Result as SqlResult, params};
use std::path::PathBuf;
use tokio::sync::Mutex;

use super::WaMessage;

pub struct WaConversationDb {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WaHistoryMessage {
    pub id: String,
    pub jid: String,
    pub direction: String, // "in" | "out"
    pub text: String,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WaContact {
    pub jid: String,
    pub last_message: String,
    pub last_ts: i64,
    pub unread: i64,
}

impl WaConversationDb {
    pub fn open_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("WaDb in-memory: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wa_messages (
                id          TEXT PRIMARY KEY,
                jid         TEXT NOT NULL,
                direction   TEXT NOT NULL,
                text        TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_wa_jid_ts ON wa_messages(jid, timestamp_ms);",
        )
        .map_err(|e| format!("WaDb init: {e}"))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open() -> Result<Self, String> {
        let path = db_path();
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|e| format!("WaDb mkdir: {e}"))?;

        let conn = Connection::open(&path)
            .map_err(|e| format!("WaDb open: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wa_messages (
                id          TEXT PRIMARY KEY,
                jid         TEXT NOT NULL,
                direction   TEXT NOT NULL,
                text        TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_wa_jid_ts ON wa_messages(jid, timestamp_ms);",
        )
        .map_err(|e| format!("WaDb init: {e}"))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Save an incoming message from the bridge.
    pub async fn save_inbound(&self, msg: &WaMessage) -> SqlResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO wa_messages(id, jid, direction, text, timestamp_ms)
             VALUES (?1, ?2, 'in', ?3, ?4)",
            params![msg.id, msg.from, msg.text, msg.timestamp as i64 * 1000],
        )?;
        Ok(())
    }

    /// Save an outgoing agent reply.
    pub async fn save_outbound(&self, jid: &str, text: &str) -> SqlResult<()> {
        let conn = self.conn.lock().await;
        let id = format!("out_{}", chrono::Utc::now().timestamp_millis());
        conn.execute(
            "INSERT INTO wa_messages(id, jid, direction, text, timestamp_ms)
             VALUES (?1, ?2, 'out', ?3, ?4)",
            params![id, jid, text, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    /// Fetch conversation history with a specific JID (newest last).
    pub async fn get_history(&self, jid: &str, limit: usize) -> SqlResult<Vec<WaHistoryMessage>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, jid, direction, text, timestamp_ms
             FROM wa_messages WHERE jid = ?1
             ORDER BY timestamp_ms DESC LIMIT ?2",
        )?;
        let rows: Vec<WaHistoryMessage> = stmt
            .query_map(params![jid, limit as i64], |row| {
                Ok(WaHistoryMessage {
                    id: row.get(0)?,
                    jid: row.get(1)?,
                    direction: row.get(2)?,
                    text: row.get(3)?,
                    timestamp_ms: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Return oldest-first
        let mut rows = rows;
        rows.reverse();
        Ok(rows)
    }

    /// List all contacts (JIDs) with their last message snippet.
    pub async fn list_contacts(&self) -> SqlResult<Vec<WaContact>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT jid,
                    text AS last_message,
                    MAX(timestamp_ms) AS last_ts,
                    SUM(CASE WHEN direction='in' THEN 1 ELSE 0 END) AS unread
             FROM wa_messages
             GROUP BY jid
             ORDER BY last_ts DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(WaContact {
                    jid: row.get(0)?,
                    last_message: row.get(1)?,
                    last_ts: row.get(2)?,
                    unread: row.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".goblin").join("whatsapp.db")
}
