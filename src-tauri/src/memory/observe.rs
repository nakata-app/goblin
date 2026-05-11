use super::db::MemoryDb;
use uuid::Uuid;

pub fn observe_tool_call(
    db: &MemoryDb,
    session_id: &str,
    tool_name: &str,
    args_summary: Option<&str>,
    result_summary: Option<&str>,
    success: bool,
) {
    let id = Uuid::new_v4().to_string();
    if let Err(e) = db.record_observation(&id, session_id, tool_name, args_summary, result_summary, success) {
        eprintln!("[memory] observe error: {}", e);
    }
}
