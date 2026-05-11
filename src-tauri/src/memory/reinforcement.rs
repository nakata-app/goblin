use super::db::MemoryDb;

pub fn reinforce_preference(db: &MemoryDb, preference: &str) {
    if let Err(e) = db.reinforce(preference) {
        eprintln!("[memory] reinforce error: {}", e);
    }
}
