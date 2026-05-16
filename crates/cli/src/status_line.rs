//! Custom status-line renderer.
//!
//! Mirrors Claude Code's `statusLine` hook: when configured, every render
//! tick may invoke a user-supplied shell command, pipe a small JSON
//! payload to its stdin, and use the first line of stdout as the status
//! footer. The command runs on a worker thread and is throttled by
//! `ttl`, so a slow script never blocks the TUI.
//!
//! Lifecycle
//! ---------
//! - `install(cmd)` once at startup, after the config is merged.
//! - From the render path, call `maybe_refresh(|| payload)` and read
//!   `current()`. `maybe_refresh` is non-blocking — it spawns a worker
//!   only when the cached value has aged past `ttl` and no other run is
//!   in flight.
//! - The worker enforces a 1-second wall-clock cap so a wedged script
//!   can never starve the renderer.
//!
//! Failures (spawn error, non-zero exit, timeout, empty stdout) leave
//! the previous output intact so a transient hiccup doesn't blank the
//! status line; `current()` returns `None` only when the command has
//! never produced output.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub struct StatusLine {
    cmd: String,
    output: Mutex<String>,
    last: Mutex<Option<Instant>>,
    inflight: Mutex<bool>,
    ttl: Duration,
    timeout: Duration,
}

static GLOBAL: OnceLock<&'static StatusLine> = OnceLock::new();

/// Wire a command to the global status-line slot. Idempotent — the
/// first install wins so repeated calls during reload don't leak
/// background workers.
pub fn install(cmd: String) {
    let leaked: &'static StatusLine = Box::leak(Box::new(StatusLine {
        cmd,
        output: Mutex::new(String::new()),
        last: Mutex::new(None),
        inflight: Mutex::new(false),
        ttl: Duration::from_millis(800),
        timeout: Duration::from_secs(1),
    }));
    let _ = GLOBAL.set(leaked);
}

/// Trigger a refresh if the cached output is older than `ttl` and no
/// worker is running. Non-blocking; the payload closure only runs when
/// a refresh is actually scheduled, so callers don't pay JSON cost on
/// throttled ticks.
pub fn maybe_refresh<F>(payload_fn: F)
where
    F: FnOnce() -> serde_json::Value + Send + 'static,
{
    let Some(s) = GLOBAL.get().copied() else { return };
    {
        let last = s.last.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < s.ttl {
                return;
            }
        }
    }
    {
        let mut inflight = s.inflight.lock().unwrap();
        if *inflight {
            return;
        }
        *inflight = true;
    }
    std::thread::spawn(move || {
        let payload = payload_fn();
        if let Some(out) = run_command(&s.cmd, &payload, s.timeout) {
            let first_line: String = out.lines().next().unwrap_or("").trim_end().to_string();
            if !first_line.is_empty() {
                *s.output.lock().unwrap() = first_line;
            }
        }
        *s.last.lock().unwrap() = Some(Instant::now());
        *s.inflight.lock().unwrap() = false;
    });
}

/// Last successful first-line of stdout, or `None` if the command has
/// never produced output yet.
pub fn current() -> Option<String> {
    let s = GLOBAL.get().copied()?;
    let out = s.output.lock().unwrap().clone();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// True when a status-line command is configured (regardless of whether
/// it has produced output yet). Used by the renderer to decide whether
/// to override the default recap line.
pub fn is_active() -> bool {
    GLOBAL.get().is_some()
}

pub(crate) fn run_command(
    cmd: &str,
    payload: &serde_json::Value,
    timeout: Duration,
) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{payload}");
    }

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }

    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_pipes_payload_to_stdin() {
        let payload = serde_json::json!({"model": "x-1", "branch": "main"});
        let out = run_command(
            "read p; echo \"M=$(echo $p | sed 's/.*model\":\"\\([^\"]*\\).*/\\1/')\"",
            &payload,
            Duration::from_secs(2),
        )
        .expect("command should run");
        assert!(out.starts_with("M=x-1"), "got: {out:?}");
    }

    #[test]
    fn run_command_kills_on_timeout() {
        let started = Instant::now();
        let result = run_command(
            "sleep 5",
            &serde_json::json!({}),
            Duration::from_millis(150),
        );
        assert!(result.is_none(), "expected None on timeout");
        // Wall-clock budget: timeout (150ms) + spawn/poll slack. Anything
        // approaching the 5s `sleep` would mean we never killed the
        // child — fail loud.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "kill path took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_command_returns_none_on_nonzero_exit() {
        let out = run_command(
            "echo nope; exit 3",
            &serde_json::json!({}),
            Duration::from_secs(2),
        );
        assert!(out.is_none());
    }

    /// Full install → refresh → current round-trip. OnceLock makes
    /// `install` idempotent across the test binary so this is the only
    /// install-using test in the file.
    #[test]
    fn install_refresh_current_roundtrip() {
        install("echo HELLO_STATUS".into());
        assert!(is_active());
        maybe_refresh(|| serde_json::json!({"k": "v"}));
        let deadline = Instant::now() + Duration::from_secs(3);
        while current().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(current().as_deref(), Some("HELLO_STATUS"));
    }
}

