//! Cluster safety tools — halluguard + promptguard daemons.
//!
//! Each tool lazily starts its Python daemon on localhost and communicates
//! via HTTP. The daemon processes persist across tool calls within a session
//! so the sentence-transformer model stays warm.
//!
//! Ports:
//!   halluguard  → 7801  (FastAPI / uvicorn)
//!   promptguard → 8765  (stdlib HTTPServer)

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const HALLUGUARD_PORT: u16 = 7801;
const PROMPTGUARD_PORT: u16 = 8765;

/// halluguard binary uses Framework Python 3.12 (installed via pip3.12 --break-system-packages).
const HALLUGUARD_BIN: &str = "/Library/Frameworks/Python.framework/Versions/3.12/bin/halluguard";
/// promptguard is installed in Homebrew Python 3.12.
const PROMPTGUARD_PYTHON: &str = "/opt/homebrew/bin/python3.12";

// ── daemon lifecycle ────────────────────────────────────────────────────────

/// Returns Ok if the daemon is already accepting connections.
async fn daemon_healthy(port: u16, path: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Spawn daemon as a detached background process. stdout/stderr → /dev/null.
fn spawn_daemon(args: &[&str]) -> Result<(), ToolError> {
    std::process::Command::new(args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| ToolError::Spawn(format!("daemon spawn: {e}")))
}

/// Ensure daemon is running; start it if not, wait up to `timeout_s`.
async fn ensure_daemon(
    port: u16,
    health_path: &str,
    cmd: &[&str],
    timeout_s: u64,
) -> Result<(), ToolError> {
    if daemon_healthy(port, health_path).await {
        return Ok(());
    }
    spawn_daemon(cmd)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if daemon_healthy(port, health_path).await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ToolError::Spawn(format!(
                "daemon on :{port} did not become healthy within {timeout_s}s"
            )));
        }
    }
}

// ── CheckHallucination ──────────────────────────────────────────────────────

/// check_hallucination: verify that an LLM answer is grounded in the provided
/// corpus documents. Uses halluguard's reverse-RAG approach — no LLM judge.
pub struct CheckHallucination;

#[async_trait]
impl Tool for CheckHallucination {
    fn name(&self) -> &str {
        "check_hallucination"
    }

    fn description(&self) -> &str {
        "Check whether an LLM-generated answer is supported by the provided \
         corpus documents. Returns a trust score (0.0–1.0), the number of \
         flagged claims, and the flagged claim texts. Uses halluguard \
         (reverse-RAG, no LLM judge). Useful before surfacing AI-generated \
         content to users."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string",
                    "description": "The LLM-generated answer to verify"
                },
                "corpus": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Source documents the answer should be grounded in"
                },
                "threshold": {
                    "type": "number",
                    "description": "Cosine similarity threshold (default 0.55)"
                }
            },
            "required": ["answer", "corpus"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let answer = args["answer"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("answer must be a string".into()))?
            .to_string();
        let corpus: Vec<String> = args["corpus"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArgs("corpus must be an array".into()))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if corpus.is_empty() {
            return Err(ToolError::InvalidArgs("corpus must not be empty".into()));
        }
        let threshold = args["threshold"].as_f64().unwrap_or(0.55);

        ensure_daemon(
            HALLUGUARD_PORT,
            "/healthz",
            &[HALLUGUARD_BIN, "serve", "--port", "7801"],
            30,
        )
        .await?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| ToolError::Spawn(format!("http client: {e}")))?;

        let body = json!({
            "corpus": corpus,
            "answer": answer,
            "threshold": threshold,
            "top_k": 5,
            "chunk_size": 200,
            "chunk_overlap": 50
        });

        let resp = client
            .post(format!("http://127.0.0.1:{HALLUGUARD_PORT}/check"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Spawn(format!("halluguard request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ToolError::Spawn(format!(
                "halluguard returned {status}: {text}"
            )));
        }

        let result: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Spawn(format!("halluguard response parse: {e}")))?;

        let ok = result["ok"].as_bool().unwrap_or(false);
        let n_claims = result["n_claims"].as_u64().unwrap_or(0);
        let n_flagged = result["n_flagged"].as_u64().unwrap_or(0);
        let trust_score = result["trust_score"].as_f64().unwrap_or(0.0);
        let flagged: Vec<String> = result["flagged"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c["text"].as_str().map(|t| format!("  • {t}")))
                    .collect()
            })
            .unwrap_or_default();

        let status_str = if ok { "CLEAN" } else { "FLAGGED" };
        let mut out = format!(
            "halluguard: {status_str}\ntrust_score: {trust_score:.2}  claims: {n_claims}  flagged: {n_flagged}\n"
        );
        if !flagged.is_empty() {
            out.push_str("\nFlagged claims:\n");
            out.push_str(&flagged.join("\n"));
        }
        Ok(out)
    }
}

// ── ScanInput ───────────────────────────────────────────────────────────────

/// scan_input: detect prompt injection / jailbreak attempts in user text.
pub struct ScanInput;

#[async_trait]
impl Tool for ScanInput {
    fn name(&self) -> &str {
        "scan_input"
    }

    fn description(&self) -> &str {
        "Scan user-supplied text for prompt injection or jailbreak patterns. \
         Returns a suggested action (PASS / WARN / BLOCK), a risk score \
         (0.0–1.0), and matched rule names. Uses promptguard's rule pack \
         (deterministic, no LLM judge). Call before forwarding untrusted \
         input to the model."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The user input to scan"
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("text must be a string".into()))?
            .to_string();

        ensure_daemon(
            PROMPTGUARD_PORT,
            "/health",
            &[PROMPTGUARD_PYTHON, "-m", "promptguard", "serve", "--port", "8765"],
            15,
        )
        .await?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ToolError::Spawn(format!("http client: {e}")))?;

        let resp = client
            .post(format!("http://127.0.0.1:{PROMPTGUARD_PORT}/check"))
            .json(&json!({ "text": text }))
            .send()
            .await
            .map_err(|e| ToolError::Spawn(format!("promptguard request: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Spawn(format!(
                "promptguard returned {status}: {body}"
            )));
        }

        let result: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Spawn(format!("promptguard response parse: {e}")))?;

        let action = result["action"].as_str().unwrap_or("UNKNOWN");
        let risk_score = result["risk_score"].as_f64().unwrap_or(0.0);
        let matched_rules: Vec<&str> = result["matched_rules"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut out = format!("scan_input: {action}  risk_score: {risk_score:.2}\n");
        if !matched_rules.is_empty() {
            out.push_str("matched_rules: ");
            out.push_str(&matched_rules.join(", "));
            out.push('\n');
        }
        Ok(out)
    }
}
