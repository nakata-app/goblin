// Memory database - Phase 1b
pub struct MemoryDb;

impl MemoryDb {
    pub fn new(_db_path: &str) -> Result<Self, String> {
        Ok(Self)
    }
}
