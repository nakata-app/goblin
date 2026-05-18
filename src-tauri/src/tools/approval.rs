use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::mpsc::UnboundedSender;
use serde_json::Value;

const APPROVAL_TIMEOUT_SECS: u64 = 30;

const DANGEROUS_TOOLS: &[&str] = &[
    "bash",
    "bash_background",
    "write_file",
    "edit_file",
    "multi_edit",
    "sandbox_exec",
    "git_commit",
    "git_pr_create",
    "whatsapp_send",
    "peer_send",
    "peer_broadcast",
];

pub fn needs_approval(tool_name: &str) -> bool {
    DANGEROUS_TOOLS.contains(&tool_name)
}

#[derive(Default)]
pub struct ApprovalGate {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn respond(&self, id: &str, approved: bool) -> Result<(), String> {
        let mut guard = self.pending.lock().map_err(|e| format!("Approval lock poisoned: {}", e))?;
        match guard.remove(id) {
            Some(tx) => {
                let _ = tx.send(approved);
                Ok(())
            }
            None => Err(format!("No pending approval with id {}", id)),
        }
    }

    fn register(&self, id: String, tx: oneshot::Sender<bool>) -> Result<(), String> {
        let mut guard = self.pending.lock().map_err(|e| format!("Approval lock poisoned: {}", e))?;
        guard.insert(id, tx);
        Ok(())
    }

    fn discard(&self, id: &str) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.remove(id);
        }
    }
}

/// Request user approval for a dangerous tool. Returns Ok(()) if approved,
/// Err(reason) if rejected or timed out. The progress channel ferries the
/// request to the frontend; the frontend responds via the
/// `tool_approval_response` Tauri command which calls
/// `ApprovalGate::respond`.
pub async fn request_approval(
    gate: &Arc<ApprovalGate>,
    progress: &Option<UnboundedSender<Value>>,
    tool_name: &str,
    args: &Value,
) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<bool>();
    gate.register(id.clone(), tx)?;

    if let Some(progress_tx) = progress {
        let _ = progress_tx.send(serde_json::json!({
            "type": "tool_approval_request",
            "id": id,
            "tool": tool_name,
            "args": args,
        }));
    } else {
        // No channel means no UI is listening; reject by default rather than
        // hang. This shouldn't happen in normal desktop flow but covers
        // headless / cron / test paths.
        gate.discard(&id);
        return Err("user_rejected (no UI channel)".to_string());
    }

    let timeout = tokio::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err("user_rejected".to_string()),
        Ok(Err(_)) => {
            gate.discard(&id);
            Err("user_rejected (channel dropped)".to_string())
        }
        Err(_) => {
            gate.discard(&id);
            Err(format!("user_rejected (approval timed out after {}s)", APPROVAL_TIMEOUT_SECS))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_tools_match() {
        assert!(needs_approval("bash"));
        assert!(needs_approval("write_file"));
        assert!(needs_approval("sandbox_exec"));
        assert!(needs_approval("whatsapp_send"));
    }

    #[test]
    fn safe_tools_pass() {
        assert!(!needs_approval("read_file"));
        assert!(!needs_approval("grep"));
        assert!(!needs_approval("web_fetch"));
        assert!(!needs_approval("git_status"));
        assert!(!needs_approval("git_diff"));
    }

    #[tokio::test]
    async fn approval_approved_path() {
        let gate = ApprovalGate::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let progress = Some(tx);

        let gate_clone = gate.clone();
        let task = tokio::spawn(async move {
            request_approval(&gate_clone, &progress, "bash", &serde_json::json!({"command": "ls"})).await
        });

        let event = rx.recv().await.expect("event received");
        let id = event["id"].as_str().unwrap().to_string();
        assert_eq!(event["type"], "tool_approval_request");
        assert_eq!(event["tool"], "bash");

        gate.respond(&id, true).unwrap();
        let res = task.await.unwrap();
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn approval_rejected_path() {
        let gate = ApprovalGate::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let progress = Some(tx);

        let gate_clone = gate.clone();
        let task = tokio::spawn(async move {
            request_approval(&gate_clone, &progress, "bash", &serde_json::json!({"command": "rm -rf /"})).await
        });

        let event = rx.recv().await.unwrap();
        let id = event["id"].as_str().unwrap().to_string();

        gate.respond(&id, false).unwrap();
        let res = task.await.unwrap();
        assert_eq!(res.unwrap_err(), "user_rejected");
    }

    #[tokio::test]
    async fn approval_no_channel_rejects() {
        let gate = ApprovalGate::new();
        let res = request_approval(&gate, &None, "bash", &serde_json::json!({})).await;
        assert!(res.unwrap_err().contains("no UI channel"));
    }
}
