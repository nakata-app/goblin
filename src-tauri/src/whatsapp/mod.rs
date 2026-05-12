use serde::{Deserialize, Serialize};
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
}

impl WhatsappBridge {
    pub fn new() -> Self {
        Self {
            process: Mutex::new(None),
            client: reqwest::Client::new(),
        }
    }

    fn base_url() -> String {
        format!("http://127.0.0.1:{}", BRIDGE_PORT)
    }

    /// Start the Node.js WhatsApp bridge sidecar
    pub async fn start(&self, app_dir: &str) -> Result<(), String> {
        let mut guard = self.process.lock().await;
        if guard.is_some() {
            return Ok(()); // already running
        }

        let bridge_dir = std::path::Path::new(app_dir)
            .join("src-tauri")
            .join("whatsapp-bridge");

        let child = Command::new("node")
            .arg("index.js")
            .current_dir(&bridge_dir)
            .env("BRIDGE_PORT", BRIDGE_PORT.to_string())
            .env("BRIDGE_TOKEN", BRIDGE_TOKEN)
            .env("BRIDGE_AUTH_DIR", bridge_dir.join("auth").to_str().unwrap_or("auth"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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
