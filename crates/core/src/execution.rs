//! Execution strategies — failure-driven improvements extracted from the
//! agent loop (Session 17+).
//!
//! This module encapsulates four execution concerns that were previously
//! embedded in [`crate::agent`]:
//!
//! 1. **Retry policy** — transient-error retry with exponential backoff
//!    for provider calls (rate limits, 5xx, network failures).
//! 2. **Loop detection** — detects when the model re-issues the same
//!    tool calls across 3 consecutive turns (silent stuck loop).
//! 3. **Consecutive error tracking** — counts tool errors across turns,
//!    injects escalating hints when the streak grows.
//! 4. **Error enrichment** — appends targeted recovery hints to tool
//!    error text so the model can self-correct.
//!
//! None of this holds any state except what the caller feeds in.
//! Everything is pure — the agent loop owns the counters and feeds them
//! back each turn.

use aegis_api::{ApiError, ToolCall};

// ---------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------

/// Maximum number of transient-error retries for provider calls.
pub const MAX_RETRIES: u32 = 3;

/// Determine if an API error is transient and worth retrying.
///
/// * `Http` (network-level) errors are always transient.
/// * `Status` errors are transient for 429 and 5xx codes.
/// * `Decode` and `MissingKey` are permanent — retrying won't help.
pub fn is_transient(err: &ApiError) -> bool {
    match err {
        ApiError::Http(_) => true,
        ApiError::Status { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 504),
        ApiError::Timeout { .. } => true,
        ApiError::Decode(_) | ApiError::MissingKey(_) => false,
    }
}

/// Retry an async operation up to [`MAX_RETRIES`] times on transient
/// errors with exponential backoff (1s, 2s, 4s).
///
/// This was previously a `#[cfg(test)]`-only free function in `agent.rs`.
/// Extracting it makes it available for the real agent loop in future
/// sessions that upgrade from `#[cfg(test)]` to production.
pub async fn retry_transient_async<F, Fut, T>(mut f: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES && is_transient(&e) => {
                attempt += 1;
                let delay = std::time::Duration::from_secs(1 << (attempt - 1));
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Loop detection
// ---------------------------------------------------------------------------

/// Build a stable fingerprint for a set of tool calls so identical calls
/// can be detected across turns. The fingerprint is
/// `tool_name(arg1,arg2,…)` for each call, joined with `|`.
pub fn tool_call_signature(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| {
            let args = c.function.arguments.as_str();
            // Trim whitespace to normalise small formatting diffs.
            let args = args.trim();
            format!("{}({})", c.function.name, args)
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Truncate a signature for error messages so the user sees the gist
/// without a multi-line JSON dump.
pub fn truncate_for_error(sig: &str, max_len: usize) -> String {
    if sig.len() <= max_len {
        sig.to_string()
    } else {
        format!("{}…", &sig[..max_len])
    }
}

/// Detector state for identical-tool-call loops. The caller pushes a
/// signature each turn and calls `check`; when three consecutive
/// signatures match, the detector fires.
#[derive(Debug, Default)]
pub struct LoopDetector {
    signatures: Vec<String>,
}

impl LoopDetector {
    /// Record the tool-call signature for this turn. Call even when there
    /// are no tool calls — a plain-text turn clears the streak.
    pub fn push(&mut self, sig: Option<String>) {
        match sig {
            Some(s) => self.signatures.push(s),
            None => self.signatures.clear(),
        }
    }

    /// Returns `Some(turn, signature)` if the last 3 turns had the same
    /// tool-call fingerprint. `turn` is the current turn number (the
    /// position where the third identical call was pushed).
    pub fn check(&self) -> Option<(usize, String)> {
        let n = self.signatures.len();
        if n >= 3
            && self.signatures[n - 1] == self.signatures[n - 2]
            && self.signatures[n - 2] == self.signatures[n - 3]
        {
            Some((n, self.signatures[n - 1].clone()))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Consecutive error tracking
// ---------------------------------------------------------------------------

/// Tracks how many consecutive tool calls have returned errors and
/// decides when to inject escalating hints into the conversation.
#[derive(Debug, Default)]
pub struct ErrorTracker {
    /// Number of consecutive tool calls that returned an error.
    pub count: u32,
}

impl ErrorTracker {
    /// Record a tool result. Pass `true` if the tool returned an error,
    /// `false` otherwise. A successful call resets the counter.
    pub fn record(&mut self, is_error: bool) {
        if is_error {
            self.count += 1;
        } else {
            self.count = 0;
        }
    }

    /// Inject an escalating hint into the conversation when the error
    /// streak reaches a threshold. Returns the hint message to append as
    /// a system message, or `None` if no threshold was crossed.
    ///
    /// Thresholds (additive — a streak of 5 triggers both the 3-message
    /// and the 5-message):
    /// - 3 errors: "plan-reassessment" — stop and try a different approach
    /// - 5 errors: "halt escalation" — stop completely, ask user for help
    pub fn escalation_hint(&self) -> Option<String> {
        match self.count {
            5 => Some(
                "[plan-reassessment] You have failed 5 times in a row. \
                 Your current approach is not working. Step back, re-read \
                 the user's original request, and try a completely different \
                 approach. Do not retry the same strategy."
                    .to_string(),
            ),
            3 => Some(
                "[plan-reassessment] 3 consecutive tool errors detected. \
                 Stop and re-evaluate. You are likely making the same \
                 mistake repeatedly. Try a fundamentally different strategy \
                 instead of retrying the same approach."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Error enrichment
// ---------------------------------------------------------------------------

/// Append a targeted recovery hint to tool error text so the model can
/// self-correct instead of blindly retrying.
///
/// Also warns when consecutive errors pile up — if the model is stuck
/// in a loop, the hint tells it to stop and re-evaluate.
pub fn enrich_error_hint(tool_name: &str, error: &str, consecutive: u32) -> String {
    let mut enriched = error.to_string();
    let err_lower = error.to_lowercase();

    // ── Cross-tool generic patterns ──────────────────────────────────

    let mut matched_generic = false;

    if err_lower.contains("permission denied") || err_lower.contains("access denied") {
        enriched.push_str(
            "\n[hint] Permission denied. Check file/directory permissions with \
             `ls -la`, ensure the path is inside the workspace, or try \
             `chmod`/`sudo` if appropriate.",
        );
        matched_generic = true;
    }

    if !matched_generic
        && (err_lower.contains("no such file")
            || err_lower.contains("does not exist")
            || err_lower.contains("not found")
                && (err_lower.contains("file")
                    || err_lower.contains("path")
                    || err_lower.contains("directory")))
    {
        let is_edit_substring = tool_name == "edit_file"
            && (err_lower.contains("not found in file") || err_lower.contains("editnotfound"));
        if !is_edit_substring {
            enriched.push_str(
                "\n[hint] File/path not found. Use `glob` or `ls` to list the directory \
                 and verify the correct path. Watch for typos or wrong extensions.",
            );
            matched_generic = true;
        }
    }

    if !matched_generic && err_lower.contains("timeout") {
        enriched.push_str(
            "\n[hint] Command timed out. The operation took too long. \
             Consider running it in the background, breaking it into \
             smaller steps, or increasing the timeout.",
        );
        // Last generic check — `matched_generic` not needed beyond here.
    }

    // ── Tool-specific patterns ──────────────────────────────────────

    match tool_name {
        "edit_file" | "multi_edit" => {
            if err_lower.contains("not found in file") || err_lower.contains("editnotfound") {
                enriched.push_str(
                    "\n[hint] `old_string` not found. Read the file again to get \
                     the current content, then retry with the exact text.",
                );
            } else if err_lower.contains("not unique") {
                enriched.push_str(
                    "\n[hint] `old_string` is not unique. Add more surrounding \
                     context lines to make it unique, or use `replace_all: true`.",
                );
            }
        }
        "bash" if err_lower.contains("spawn") || err_lower.contains("command not found") => {
            enriched.push_str(
                "\n[hint] Command not found or failed to spawn. Check the \
                 command name, ensure required tools are installed, and \
                 verify the PATH.",
            );
        }
        "grep" if err_lower.contains("regex") || err_lower.contains("invalid") => {
            enriched.push_str(
                "\n[hint] Invalid regex pattern. Check for unmatched \
                 parentheses, brackets, or other syntax errors.",
            );
        }
        _ => {}
    }

    // ── Consecutive error escalation ─────────────────────────────────

    if consecutive >= 5 {
        enriched.push_str(&format!(
            "\n[warning] {} consecutive tool errors. The current approach is \
             not working. Step back and reconsider: ask yourself what the \
             user actually wants, then try a completely different strategy.",
            consecutive
        ));
    } else if consecutive >= 3 {
        enriched.push_str(&format!(
            "\n[warning] {} consecutive tool errors. STOP retrying the same approach. \
             Step back and reconsider: try a different strategy, re-read the \
             relevant files, or use ask_user to get guidance.",
            consecutive
        ));
    }

    enriched
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_api::ToolCallFunction;

    fn tc(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    // ── is_transient ─────────────────────────────────────────────────

    #[test]
    fn is_transient_classifies_correctly() {
        assert!(is_transient(&ApiError::Status {
            status: 429,
            body: "rate limit".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 500,
            body: "boom".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 502,
            body: "bad gateway".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 503,
            body: "unavailable".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 504,
            body: "timeout".into()
        }));
        assert!(!is_transient(&ApiError::Status {
            status: 400,
            body: "bad request".into()
        }));
        assert!(!is_transient(&ApiError::Status {
            status: 404,
            body: "not found".into()
        }));
    }

    // ── tool_call_signature ──────────────────────────────────────────

    #[test]
    fn signature_is_deterministic() {
        let calls = vec![
            tc("read_file", "{\"path\":\"foo.rs\"}"),
            tc("grep", "{\"pattern\":\"fn main\"}"),
        ];
        let sig = tool_call_signature(&calls);
        assert_eq!(
            sig,
            "read_file({\"path\":\"foo.rs\"})|grep({\"pattern\":\"fn main\"})"
        );
    }

    #[test]
    fn signature_normalises_whitespace() {
        let calls = vec![tc("bash", "  {\"cmd\": \"ls\"}  ")];
        let sig = tool_call_signature(&calls);
        assert_eq!(sig, "bash({\"cmd\": \"ls\"})");
    }

    // ── LoopDetector ─────────────────────────────────────────────────

    #[test]
    fn loop_detector_fires_on_three_identical() {
        let mut ld = LoopDetector::default();
        let sig = "read_file({\"path\":\"x\"})".to_string();
        ld.push(Some(sig.clone()));
        assert!(ld.check().is_none());
        ld.push(Some(sig.clone()));
        assert!(ld.check().is_none());
        ld.push(Some(sig.clone()));
        assert!(ld.check().is_some());
    }

    #[test]
    fn loop_detector_resets_on_empty_turn() {
        let mut ld = LoopDetector::default();
        let sig = "bash({\"cmd\":\"ls\"})".to_string();
        ld.push(Some(sig.clone()));
        ld.push(Some(sig.clone()));
        ld.push(None); // plain-text turn
        assert!(ld.check().is_none());
        ld.push(Some(sig.clone()));
        ld.push(Some(sig.clone()));
        assert!(ld.check().is_none());
    }

    #[test]
    fn loop_detector_requires_three_consecutive() {
        let mut ld = LoopDetector::default();
        ld.push(Some("A".into()));
        ld.push(Some("A".into()));
        ld.push(Some("B".into())); // different — no fire
        assert!(ld.check().is_none());
        ld.push(Some("B".into()));
        ld.push(Some("B".into()));
        assert!(ld.check().is_some()); // B-B-B
    }

    // ── ErrorTracker ─────────────────────────────────────────────────

    #[test]
    fn error_tracker_counts_consecutive_errors() {
        let mut et = ErrorTracker::default();
        assert_eq!(et.count, 0);

        et.record(true);
        assert_eq!(et.count, 1);

        et.record(true);
        assert_eq!(et.count, 2);

        et.record(false); // success resets
        assert_eq!(et.count, 0);

        et.record(true);
        assert_eq!(et.count, 1);
    }

    #[test]
    fn error_tracker_escalation_hints() {
        let mut et = ErrorTracker::default();
        assert!(et.escalation_hint().is_none());

        et.count = 2;
        assert!(et.escalation_hint().is_none());

        et.count = 3;
        assert!(et.escalation_hint().unwrap().contains("3 consecutive"));

        et.count = 5;
        let hint = et.escalation_hint().unwrap();
        assert!(hint.contains("5 times"));
        assert!(hint.contains("failed"));
    }

    // ── enrich_error_hint ────────────────────────────────────────────

    #[test]
    fn enrich_permission_denied() {
        let out = enrich_error_hint("bash", "error: permission denied", 0);
        assert!(out.contains("[hint] Permission denied"));
    }

    #[test]
    fn enrich_file_not_found() {
        let out = enrich_error_hint("read_file", "No such file", 0);
        assert!(out.contains("[hint] File/path not found"));
    }

    #[test]
    fn enrich_edit_not_found() {
        let out = enrich_error_hint("edit_file", "error: `old_string` not found in foo.rs", 0);
        assert!(out.contains("old_string"));
    }

    #[test]
    fn enrich_consecutive_escalation() {
        let out = enrich_error_hint("bash", "error: permission denied", 3);
        assert!(out.contains("[warning] 3 consecutive tool errors"));
    }

    #[test]
    fn enrich_consecutive_max() {
        let out = enrich_error_hint("grep", "regex invalid", 5);
        assert!(out.contains("[warning] 5 consecutive tool errors"));
    }
}
