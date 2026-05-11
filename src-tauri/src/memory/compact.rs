use super::db::MemoryDb;

pub fn compact_if_needed(db: &MemoryDb, days_old: i32) {
    match db.compact(days_old) {
        Ok(count) => {
            if count > 0 {
                eprintln!("[memory] compacted {} old records", count);
            }
        }
        Err(e) => eprintln!("[memory] compact error: {}", e),
    }
}
