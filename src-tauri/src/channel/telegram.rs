//! Telegram Bot API client.
//! - `send`: outbound text message (already used by channel::publish)
//! - `start_polling`: background loop that reads incoming messages,
//!   routes them through the agent, and replies. Activated when
//!   channels.telegram.auto_reply = true in config.toml.

use crate::agent::r#loop::AgentLoop;
use crate::config::TelegramConfig;
use std::sync::Arc;
use tokio::sync::Mutex;

const TELEGRAM_API: &str = "https://api.telegram.org";

/// Redact a Telegram chat_id so log lines still distinguish chats without
/// revealing the full numeric ID. "123456789" -> "12…9", short IDs masked.
fn mask_chat_id(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(2).collect();
    let tail: String = chars.iter().rev().take(1).collect();
    format!("{head}…{tail}")
}

/// Send `text` to `chat_id` via the bot identified by `bot_token`.
/// Returns Ok(()) on HTTP 2xx, an Err with the response status / body
/// otherwise. Times out after 10 seconds so a network blip cannot pile
/// up tokio tasks.
pub async fn send(bot_token: &str, chat_id: &str, text: &str) -> Result<(), String> {
    if bot_token.is_empty() {
        return Err("telegram: bot_token is empty".to_string());
    }
    if chat_id.is_empty() {
        return Err("telegram: chat_id is empty".to_string());
    }

    let url = format!("{}/bot{}/sendMessage", TELEGRAM_API, bot_token);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("telegram: build client: {}", e))?;

    // Telegram caps messages at 4096 chars. Trim before send so a
    // verbose decision summary does not 400 the API. Char-boundary
    // safe — we slice by chars(), not bytes.
    let trimmed: String = if text.chars().count() > 4096 {
        text.chars().take(4090).collect::<String>() + "\n…"
    } else {
        text.to_string()
    };

    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": trimmed,
        "disable_web_page_preview": true,
    });

    let resp = client.post(&url).json(&body).send().await
        .map_err(|e| format!("telegram: request: {}", e))?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(format!("telegram: {} {}", status, body))
}

/// Background polling loop. Runs forever until the process exits.
/// Calls getUpdates long-poll (30s timeout), routes each text message
/// through the agent, sends the reply back via sendMessage.
pub async fn start_polling(
    cfg: TelegramConfig,
    agent_slot: Arc<Mutex<Option<AgentLoop>>>,
) {
    if cfg.bot_token.is_empty() {
        eprintln!("[telegram] auto_reply=true but bot_token is empty — polling skipped");
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()
    {
        Ok(c) => c,
        Err(e) => { eprintln!("[telegram] build client: {}", e); return; }
    };

    let mut offset: i64 = 0;

    // İzin verilen chat id'leri: allowed_chat_ids varsa onlar,
    // yoksa sadece config'deki chat_id.
    let allowed: Vec<String> = if cfg.allowed_chat_ids.is_empty() {
        if cfg.chat_id.is_empty() { vec![] } else { vec![cfg.chat_id.clone()] }
    } else {
        cfg.allowed_chat_ids.clone()
    };

    eprintln!("[telegram] polling başladı (izinli: {:?})", allowed);

    loop {
        let url = format!(
            "{}/bot{}/getUpdates?timeout=30&offset={}",
            TELEGRAM_API, cfg.bot_token, offset
        );

        let updates = match client.get(&url).send().await {
            Ok(r) => match r.json::<serde_json::Value>().await {
                Ok(v) => v,
                Err(e) => { eprintln!("[telegram] parse: {}", e); tokio::time::sleep(std::time::Duration::from_secs(5)).await; continue; }
            },
            Err(e) => { eprintln!("[telegram] getUpdates: {}", e); tokio::time::sleep(std::time::Duration::from_secs(5)).await; continue; }
        };

        let arr = match updates["result"].as_array() {
            Some(a) => a.clone(),
            None => { tokio::time::sleep(std::time::Duration::from_secs(5)).await; continue; }
        };

        for update in &arr {
            let update_id = update["update_id"].as_i64().unwrap_or(0);
            if update_id >= offset { offset = update_id + 1; }

            // Sadece düz metin mesajları işle
            let msg = match update.get("message") { Some(m) => m, None => continue };
            let text = match msg["text"].as_str() { Some(t) => t, None => continue };
            let chat_id_val = match msg["chat"]["id"].as_i64() {
                Some(id) => id.to_string(),
                None => continue,
            };
            let from_name = msg["from"]["first_name"].as_str().unwrap_or("kullanıcı");

            // İzin kontrolü
            if !allowed.is_empty() && !allowed.contains(&chat_id_val) {
                eprintln!("[telegram] izinsiz chat_id {} reddedildi", mask_chat_id(&chat_id_val));
                continue;
            }

            // Never log message text — it leaks private content to stdout.
            // Length-only summary is enough to confirm the message was received.
            eprintln!("[telegram] inbound from chat={} ({} chars)", mask_chat_id(&chat_id_val), text.chars().count());
            let _ = from_name; // intentionally unused in the log

            // Agent'a gönder
            let mut guard = agent_slot.lock().await;
            let reply = match guard.as_mut() {
                None => "Goblin henüz hazır değil.".to_string(),
                Some(agent) => {
                    let soul = crate::agent::soul::load_soul();
                    let ctx = format!("Telegram üzerinden {} şunu yazdı:", from_name);
                    match agent.send_message(
                        text, Some(&ctx), &[], &[], None, None,
                        soul.as_deref(), &[], &[],
                    ).await {
                        Ok(r) => r.content,
                        Err(e) => format!("Hata: {}", e),
                    }
                }
            };
            drop(guard);

            // Cevabı Telegram'a gönder
            if let Err(e) = send(&cfg.bot_token, &chat_id_val, &reply).await {
                eprintln!("[telegram] reply send: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_empty_token_errors_locally() {
        let err = send("", "12345", "hi").await.unwrap_err();
        assert!(err.contains("bot_token"));
    }

    #[tokio::test]
    async fn send_empty_chat_id_errors_locally() {
        let err = send("123:abc", "", "hi").await.unwrap_err();
        assert!(err.contains("chat_id"));
    }
}
