//! Monitor tool — stream a background command's output line by line.
//!
//! Runs a shell command and returns its stdout lines as they arrive.
//! Designed for watching long-running processes (`cargo watch`, `npm run dev`,
//! server logs) without blocking the agent for the full duration.
//!
//! # Behaviour
//!
//! * Runs the command via `sh -c` (same as `Bash`).
//! * Collects up to `max_lines` stdout lines (default 50).
//! * Stops early when `timeout_secs` elapses (default 30s).
//! * Returns all collected lines + exit status summary.
//! * stderr is merged into stdout (2>&1) so log lines aren't lost.
//!
//! # CC parity
//!
//! Claude Code's `Monitor` tool watches a background process handle and
//! surfaces each new stdout line as a notification. Aegis's version is
//! simpler: it runs the command in the foreground but with a timeout +
//! line cap, which handles the common case of "run this, show me the
//! first N lines of output".

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use super::{Tool, ToolContext, ToolError};

pub struct Monitor;

#[derive(Debug, Deserialize)]
struct MonitorArgs {
    /// Shell command to run (via `sh -c`).
    command: String,
    /// Maximum lines to collect before stopping (default: 50).
    #[serde(default = "default_max_lines")]
    max_lines: usize,
    /// Stop after this many seconds even if the command hasn't exited (default: 30).
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_max_lines() -> usize {
    50
}
fn default_timeout() -> u64 {
    30
}

#[async_trait]
impl Tool for Monitor {
    fn name(&self) -> &str {
        "monitor"
    }

    fn description(&self) -> &str {
        "Run a shell command and stream its output line by line, stopping after \
         max_lines lines or timeout_secs seconds. Useful for watching build \
         output, test runs, or log tails without blocking indefinitely."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run (via sh -c). stderr is merged with stdout."
                },
                "max_lines": {
                    "type": "integer",
                    "description": "Stop after this many output lines (default: 50).",
                    "default": 50
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Stop after this many seconds even if the command hasn't exited (default: 30).",
                    "default": 30
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: MonitorArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("monitor args: {e}")))?;

        let max_lines = args.max_lines.clamp(1, 500);
        let timeout = Duration::from_secs(args.timeout_secs.clamp(1, 300));

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&args.command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.current_dir(&ctx.effective_root());

        let mut child = cmd.spawn().map_err(|e| {
            ToolError::InvalidArgs(format!("failed to spawn `{}`: {e}", args.command))
        })?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let deadline = Instant::now() + timeout;
        let mut collected: Vec<String> = Vec::new();
        let mut truncated = false;

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx2 = tx.clone();

        let _t1 = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let _t2 = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx2.send(format!("[stderr] {line}")).is_err() {
                    break;
                }
            }
        });

        loop {
            if collected.len() >= max_lines {
                truncated = true;
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                truncated = true;
                break;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(line) => collected.push(line),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        truncated = true;
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let _ = child.kill();
        let exit = child.wait().ok();
        let exit_code = exit.and_then(|s| s.code()).unwrap_or(-1);

        let mut output = collected.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n[monitor stopped: {} lines, timeout {}s or max_lines {} reached]",
                collected.len(),
                args.timeout_secs,
                max_lines
            ));
        } else {
            output.push_str(&format!("\n[exit code: {exit_code}]"));
        }
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// ScheduleWakeup — dynamic loop pacing
// ---------------------------------------------------------------------------

/// Signal to the loop runtime to pause for N seconds before firing again.
///
/// This is a lightweight "sleep hint" for autonomous `/loop` flows. The tool
/// stores the requested delay in the tool context's workspace, and the loop
/// scheduler reads it at the end of each turn to decide how long to wait.
///
/// Mirrors Claude Code's `ScheduleWakeup` which lets the agent self-pace
/// iterations (e.g. "check the build every 5 minutes").
///
/// # Usage
///
/// ```text
/// ScheduleWakeup({ "delay_seconds": 60, "reason": "waiting for build" })
/// ```
///
/// Returns immediately; the delay is applied by the loop host, not here.
pub struct ScheduleWakeup;

#[derive(Debug, Deserialize)]
struct ScheduleWakeupArgs {
    /// How many seconds to wait before the next loop iteration (1–3600).
    delay_seconds: u64,
    /// Human-readable reason shown in the status line (optional).
    #[serde(default)]
    reason: String,
}

#[async_trait]
impl Tool for ScheduleWakeup {
    fn name(&self) -> &'static str {
        "schedule_wakeup"
    }

    fn description(&self) -> &'static str {
        "In an autonomous /loop context, schedule a wakeup after delay_seconds \
         seconds. The loop will pause for that long before firing the next iteration. \
         Use this to self-pace polling loops (e.g. check a build every 60 seconds)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delay_seconds": {
                    "type": "integer",
                    "description": "Seconds to wait before the next loop iteration (1–3600).",
                    "minimum": 1,
                    "maximum": 3600
                },
                "reason": {
                    "type": "string",
                    "description": "Short description shown in the status line while waiting."
                }
            },
            "required": ["delay_seconds"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: ScheduleWakeupArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("schedule_wakeup args: {e}")))?;

        let delay = args.delay_seconds.clamp(1, 3600);
        let reason = if args.reason.is_empty() {
            format!("scheduled wakeup in {delay}s")
        } else {
            args.reason.clone()
        };

        let ws = ctx.effective_root();
        let hint_path = ws.join(".metis").join("loop_wakeup.json");
        let _ = std::fs::create_dir_all(hint_path.parent().unwrap());
        let hint = json!({
            "delay_seconds": delay,
            "reason": reason,
            "set_at": crate::telemetry::now_iso8601()
        });
        let _ = std::fs::write(&hint_path, hint.to_string());

        Ok(format!(
            "Wakeup scheduled: {delay}s — {reason}"
        ))
    }
}

// ---------------------------------------------------------------------------
// Utility: read the pending wakeup hint (used by loop host)
// ---------------------------------------------------------------------------

/// Read the pending wakeup delay written by `ScheduleWakeup`. Returns `None`
/// if no hint is pending. Consuming: deletes the file after reading so the
/// hint fires only once.
pub fn read_wakeup_hint(workspace: &std::path::Path) -> Option<(u64, String)> {
    let path = workspace.join(".metis").join("loop_wakeup.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    let delay = v.get("delay_seconds")?.as_u64()?;
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let _ = std::fs::remove_file(&path);
    Some((delay, reason))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;

    fn make_ctx(ws: &std::path::Path) -> ToolContext {
        ToolContext::new(ws.to_path_buf())
    }

    #[tokio::test]
    async fn monitor_captures_echo_output() {
        let tmp = std::env::temp_dir();
        let ctx = make_ctx(&tmp);
        let m = Monitor;
        let output = m
            .execute(
                json!({ "command": "echo hello && echo world", "max_lines": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.contains("hello"), "missing hello: {output}");
        assert!(output.contains("world"), "missing world: {output}");
    }

    #[tokio::test]
    async fn monitor_respects_max_lines() {
        let tmp = std::env::temp_dir();
        let ctx = make_ctx(&tmp);
        let m = Monitor;
        let output = m
            .execute(
                json!({ "command": "seq 1 100", "max_lines": 5 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            output.contains("monitor stopped"),
            "should note truncation: {output}"
        );
    }

    #[tokio::test]
    async fn monitor_captures_stderr() {
        let tmp = std::env::temp_dir();
        let ctx = make_ctx(&tmp);
        let m = Monitor;
        let output = m
            .execute(
                json!({ "command": "echo err >&2", "max_lines": 10, "timeout_secs": 5 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.contains("err"), "stderr not captured: {output}");
    }

    #[tokio::test]
    async fn schedule_wakeup_writes_hint() {
        let ws = std::env::temp_dir().join(format!("metis-wakeup-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ctx = make_ctx(&ws);
        let sw = ScheduleWakeup;
        let output = sw
            .execute(
                json!({ "delay_seconds": 42, "reason": "checking build" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.contains("42s"), "missing delay: {output}");
        assert!(output.contains("checking build"), "missing reason: {output}");

        let (delay, reason) = read_wakeup_hint(&ws).unwrap();
        assert_eq!(delay, 42);
        assert!(reason.contains("checking build"));

        // Second read should return None (file deleted after first read)
        assert!(read_wakeup_hint(&ws).is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }
}
