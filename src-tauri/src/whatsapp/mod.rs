pub mod db;
pub use db::{WaContact, WaConversationDb, WaHistoryMessage};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const BRIDGE_PORT: u16 = 3469;
const BRIDGE_TOKEN: &str = "goblin-whatsapp-bridge";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub status: String,
    pub error: Option<String>,
    pub user: Option<WaUser>,
    pub qr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaUser {
    pub jid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaMessage {
    pub id: String,
    pub from: String,
    pub text: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub success: bool,
    pub id: Option<String>,
    pub error: Option<String>,
}

pub struct WhatsappBridge {
    process: Mutex<Option<Child>>,
    client: reqwest::Client,
    pub db: Arc<WaConversationDb>,
}

impl WhatsappBridge {
    pub fn new() -> Self {
        let db = WaConversationDb::open()
            .map(Arc::new)
            .unwrap_or_else(|e| {
                eprintln!("[wa_db] Failed to open conversation db: {e}");
                // Fallback: open in-memory db won't persist but won't crash
                Arc::new(WaConversationDb::open_memory().expect("in-memory db must work"))
            });
        Self {
            process: Mutex::new(None),
            client: reqwest::Client::new(),
            db,
        }
    }

    fn base_url() -> String {
        format!("http://127.0.0.1:{}", BRIDGE_PORT)
    }

    /// Start the Node.js WhatsApp bridge sidecar
    pub async fn start(&self, app_dir: &str) -> Result<(), String> {
        let mut guard = self.process.lock().await;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => return Ok(()), // alive
                _ => {
                    let _ = guard.take(); // dead or errored, clear handle
                }
            }
        }

        let bridge_dir = std::path::Path::new(app_dir)
            .join("src-tauri")
            .join("whatsapp-bridge");

        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let auth_dir = std::path::PathBuf::from(&home).join(".goblin").join("whatsapp-auth");

        // Install dependencies on first run (or after node_modules is wiped)
        if !bridge_dir.join("node_modules").exists() {
            let install = tokio::process::Command::new("npm")
                .arg("install")
                .current_dir(&bridge_dir)
                .status()
                .await
                .map_err(|e| format!("npm install failed: {}", e))?;
            if !install.success() {
                return Err("npm install exited with non-zero status".to_string());
            }
        }

        let child = Command::new("node")
            .arg("index.js")
            .current_dir(&bridge_dir)
            .env("BRIDGE_PORT", BRIDGE_PORT.to_string())
            .env("BRIDGE_TOKEN", BRIDGE_TOKEN)
            .env("BRIDGE_AUTH_DIR", auth_dir.to_str().unwrap_or("/tmp/whatsapp-auth"))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to start WhatsApp bridge: {}", e))?;

        *guard = Some(child);

        // Wait for bridge to come online (health check with retries)
        for i in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if self.health_check().await.is_ok() {
                return Ok(());
            }
            if i == 9 {
                return Err("WhatsApp bridge failed to start after 5 seconds".to_string());
            }
        }
        Ok(())
    }

    /// Stop the bridge
    pub async fn stop(&self) {
        let mut guard = self.process.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
    }

    /// Check if bridge is running
    pub async fn is_running(&self) -> bool {
        self.health_check().await.is_ok()
    }

    async fn health_check(&self) -> Result<(), String> {
        let url = format!("{}/health", Self::base_url());
        match self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => Ok(()),
            _ => Err("Bridge not reachable".to_string()),
        }
    }

    /// Get bridge status including QR code
    pub async fn get_status(&self) -> Result<BridgeStatus, String> {
        let url = format!("{}/status", Self::base_url());
        let resp = self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .send()
            .await
            .map_err(|e| format!("Failed to get status: {}", e))?;

        let status: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse status: {}", e))?;

        let bridge_status = status.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
        let bridge_error = status.get("error").and_then(|v| v.as_str()).map(|s| s.to_string());
        let user = status.get("user").and_then(|u| {
            Some(WaUser {
                jid: u.get("jid")?.as_str()?.to_string(),
                name: u.get("name")?.as_str()?.to_string(),
            })
        });

        // Also fetch QR
        let qr = self.get_qr().await.ok();

        Ok(BridgeStatus {
            status: bridge_status.to_string(),
            error: bridge_error,
            user,
            qr,
        })
    }

    /// Get QR code (base64 PNG data URL)
    async fn get_qr(&self) -> Result<String, String> {
        let url = format!("{}/qr", Self::base_url());
        let resp = self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .send()
            .await
            .map_err(|e| format!("Failed to get QR: {}", e))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse QR response: {}", e))?;

        data.get("qr")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "No QR available".to_string())
    }

    /// Send a WhatsApp message
    pub async fn send_message(&self, jid: &str, text: &str) -> Result<SendResult, String> {
        let url = format!("{}/send", Self::base_url());
        let body = serde_json::json!({
            "jid": jid,
            "text": text,
        });

        let resp = self
            .client
            .post(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {}", e))?;

        let status = resp.status().as_u16();
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse send response: {}", e))?;

        if status == 200 {
            Ok(SendResult {
                success: true,
                id: data.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                error: None,
            })
        } else {
            Ok(SendResult {
                success: false,
                id: None,
                error: data.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
            })
        }
    }

    /// Fetch contact names from the bridge. Returns a (jid → display name)
    /// map populated by Baileys' contacts events + msg.pushName. Empty on
    /// any failure so callers can degrade gracefully.
    pub async fn fetch_contact_names(&self) -> std::collections::HashMap<String, String> {
        let url = format!("{}/contacts", Self::base_url());
        let Ok(resp) = self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        else {
            return std::collections::HashMap::new();
        };
        let Ok(data) = resp.json::<serde_json::Value>().await else {
            return std::collections::HashMap::new();
        };
        let mut out = std::collections::HashMap::new();
        if let Some(arr) = data.get("contacts").and_then(|v| v.as_array()) {
            for c in arr {
                if let (Some(jid), Some(name)) = (
                    c.get("jid").and_then(|v| v.as_str()),
                    c.get("name").and_then(|v| v.as_str()),
                ) {
                    out.insert(jid.to_string(), name.to_string());
                }
            }
        }
        out
    }

    /// Fetch a profile picture as a data URL. None when unavailable
    /// (private profile / disconnected / network error). Bridge caches
    /// for 24h, so it is cheap to call repeatedly.
    pub async fn profile_picture(&self, jid: &str) -> Option<String> {
        let url = format!("{}/profile-picture/{}", Self::base_url(), urlencoding::encode(jid));
        let resp = self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
            .ok()?;
        let data: serde_json::Value = resp.json().await.ok()?;
        data.get("photo").and_then(|v| v.as_str()).map(|s| s.to_string())
    }

    /// Poll for new messages
    pub async fn poll_messages(&self) -> Result<Vec<WaMessage>, String> {
        let url = format!("{}/messages", Self::base_url());
        let resp = self
            .client
            .get(&url)
            .header("x-bridge-token", BRIDGE_TOKEN)
            .send()
            .await
            .map_err(|e| format!("Failed to poll messages: {}", e))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse messages: {}", e))?;

        let messages: Vec<WaMessage> = data
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(WaMessage {
                            id: m.get("id")?.as_str()?.to_string(),
                            from: m.get("from")?.as_str()?.to_string(),
                            text: m.get("text")?.as_str()?.to_string(),
                            timestamp: m.get("timestamp")?.as_u64()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(messages)
    }
}

impl Drop for WhatsappBridge {
    fn drop(&mut self) {
        // Attempt sync kill as fallback
        if let Ok(mut guard) = self.process.try_lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.start_kill();
            }
        }
    }
}
