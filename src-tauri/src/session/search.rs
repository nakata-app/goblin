use rusqlite::{Connection, params};

pub fn search_sessions(conn: &Connection, query: &str, limit: i32) -> Result<Vec<String>, String> {
    let mut stmt = conn.prepare(
        "SELECT id FROM sessions_fts WHERE sessions_fts MATCH ?1 LIMIT ?2"
    ).map_err(|e| format!("FTS prepare error: {}", e))?;

    let rows = stmt.query_map(params![query, limit], |row| row.get::<_, String>(0))
        .map_err(|e| format!("FTS query error: {}", e))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| format!("Row error: {}", e))?);
    }
    Ok(results)
}
