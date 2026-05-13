//! Generic JSON webhook sink. POSTs each event as a small JSON
//! envelope. Same fire-and-forget contract as the Telegram channel: a
//! 5xx or a network blip is logged to stderr and otherwise ignored.

pub async fn send(
    url: &str,
    bearer_token: &str,
    kind: &str,
    text: &str,
) -> Result<(), String> {
    if url.is_empty() {
        return Err("webhook: url is empty".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("webhook: build client: {}", e))?;

    let body = serde_json::json!({
        "kind": kind,
        "text": text,
        "ts": chrono::Utc::now().timestamp(),
        "source": "goblin",
    });

    let mut req = client.post(url).json(&body);
    if !bearer_token.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", bearer_token));
    }

    let resp = req.send().await.map_err(|e| format!("webhook: request: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("webhook: {} {}", status, body));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_url_errors_locally() {
        let err = send("", "", "decision", "hi").await.unwrap_err();
        assert!(err.contains("url"));
    }
}
