//! Minimal Telegram Bot API client. Single `send` entry point —
//! POST /bot<token>/sendMessage with plain text body. No formatting,
//! no markdown escape dance; rendering richer messages is a follow-up
//! once the basic feed is stable.

const TELEGRAM_API: &str = "https://api.telegram.org";

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
