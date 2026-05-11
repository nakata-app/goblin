use super::db::MemoryDb;

pub fn inject_memories(db: &MemoryDb, ns: &str, limit: i32) -> Vec<String> {
    match db.search_memories(Some(ns), "", limit) {
        Ok(records) => records.into_iter().map(|r| r.text).collect(),
        Err(e) => {
            eprintln!("[memory] inject error: {}", e);
            Vec::new()
        }
    }
}

pub fn inject_learned(db: &MemoryDb, limit: i32) -> Vec<String> {
    match db.get_learned(limit) {
        Ok(prefs) => prefs,
        Err(e) => {
            eprintln!("[memory] learned error: {}", e);
            Vec::new()
        }
    }
}
