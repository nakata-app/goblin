//! HalluGuard HTTP client — optional post-response hallucination check.
//!
//! Expects a running `halluguard serve` daemon at `HALLUGUARD_URL`
//! (default http://127.0.0.1:7801). If the daemon is not reachable the
//! check is silently skipped — Aegis never blocks on HalluGuard.

use serde::{Deserialize, Serialize};

const DEFAULT_URL: &str = "http://127.0.0.1:7801";

fn daemon_url() -> String {
    std::env::var("HALLUGUARD_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

#[derive(Serialize)]
struct CheckRequest<'a> {
    corpus: &'a [String],
    answer: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct FlaggedClaim {
    pub text: String,
    pub score: f32,
}

#[derive(Debug, Deserialize)]
pub struct CheckResult {
    pub ok: bool,
    pub n_claims: u32,
    pub n_flagged: u32,
    pub trust_score: f32,
    pub flagged: Vec<FlaggedClaim>,
}

/// Check `answer` against `corpus` docs via the HalluGuard daemon.
///
/// Returns `None` if the daemon is not running or the request fails —
/// callers should treat `None` as "no check performed" and proceed normally.
pub async fn check(corpus: &[String], answer: &str) -> Option<CheckResult> {
    if answer.trim().is_empty() || corpus.is_empty() {
        return None;
    }

    let url = format!("{}/check", daemon_url());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let body = CheckRequest { corpus, answer };
    let resp = client.post(&url).json(&body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<CheckResult>().await.ok()
}

/// Best-effort healthcheck — returns true only if daemon responds 200.
pub async fn is_available() -> bool {
    let url = format!("{}/healthz", daemon_url());
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(&url)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}
