//! Event-driven hook system.
//!
//! Hooks are shell commands that fire at well-defined points in the
//! agent lifecycle: session start, session end, before/after tool use,
//! user prompt submission, and compaction. Each hook has an `on_fail` policy:
//!
//! - **block** — a non-zero exit code aborts the operation and the
//!   hook's stderr is surfaced to the model as an error.
//! - **warn** — a non-zero exit code is logged but the operation
//!   proceeds anyway.
//!
//! Configuration is loaded from `.aegis/hooks.toml` (workspace-level)
//! and `~/.aegis/hooks.toml` (user-level). Workspace hooks run first;
//! user hooks second. Each file uses the same schema.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

// ------------------------------------------------------------------
// Types
// ------------------------------------------------------------------

/// The lifecycle events that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Compact,
}

impl HookEvent {
    /// The TOML table name for this event.
    pub fn key(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::Compact => "compact",
        }
    }
}

/// What to do when a hook exits non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum OnFail {
    Block,
    #[default]
    Warn,
}

/// A single hook entry in the config file.
#[derive(Debug, Clone, Deserialize)]
pub struct HookEntry {
    /// Shell command to run (passed to `sh -c`).
    pub command: String,
    /// Behaviour on non-zero exit. Default: warn.
    #[serde(default)]
    pub on_fail: OnFail,
    /// Wall-clock timeout in seconds. Default: 10.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    10
}

/// TOML config file shape.
///
/// ```toml
/// [[session_start]]
/// command = "git pull --ff-only"
/// on_fail = "warn"
///
/// [[pre_tool_use]]
/// command = "echo tool=$METIS_TOOL_NAME"
/// on_fail = "block"
/// ```
/// Fired once at session start (new sessions only, not resumed).
/// The value is a synthetic first user turn that triggers the agent
/// to write an opening message before waiting for real user input.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookConfig {
    /// Synthetic first user turn sent automatically on new sessions.
    /// Agent writes an opening message, then REPL waits for user input.
    #[serde(default)]
    pub opening_prompt: Option<String>,
    #[serde(default)]
    pub session_start: Vec<HookEntry>,
    #[serde(default)]
    pub session_end: Vec<HookEntry>,
    #[serde(default)]
    pub pre_tool_use: Vec<HookEntry>,
    #[serde(default)]
    pub post_tool_use: Vec<HookEntry>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookEntry>,
    #[serde(default)]
    pub compact: Vec<HookEntry>,
}

impl HookConfig {
    /// Hooks for a given event.
    pub fn hooks_for(&self, event: HookEvent) -> &[HookEntry] {
        match event {
            HookEvent::SessionStart => &self.session_start,
            HookEvent::SessionEnd => &self.session_end,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::Compact => &self.compact,
        }
    }

    /// True if no hooks are configured.
    pub fn is_empty(&self) -> bool {
        self.opening_prompt.is_none()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.user_prompt_submit.is_empty()
            && self.compact.is_empty()
    }

    /// Merge another config into this one. The `other` hooks are
    /// appended after `self` hooks, so workspace hooks run first
    /// when workspace is `self` and user-level is `other`.
    pub fn merge(&mut self, other: HookConfig) {
        if self.opening_prompt.is_none() {
            self.opening_prompt = other.opening_prompt;
        }
        self.session_start.extend(other.session_start);
        self.session_end.extend(other.session_end);
        self.pre_tool_use.extend(other.pre_tool_use);
        self.post_tool_use.extend(other.post_tool_use);
        self.user_prompt_submit.extend(other.user_prompt_submit);
        self.compact.extend(other.compact);
    }
}

/// Outcome of running a single hook.
#[derive(Debug, Clone)]
pub struct HookResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Outcome of running all hooks for an event.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    /// Combined stdout from all hooks (for forwarding to model).
    pub output: String,
    /// True if any blocking hook failed.
    pub blocked: bool,
    /// Block reason (stderr of the first blocking failure).
    pub block_reason: Option<String>,
    /// Individual results.
    pub results: Vec<HookResult>,
}

// ------------------------------------------------------------------
// Runner
// ------------------------------------------------------------------

/// Runs hooks for a given event, passing context via env vars.
///
/// Hooks for the same event run **in parallel** — total wall time is
/// `max(t1, t2, ...)` rather than `t1 + t2 + ...`. Output ordering is
/// preserved (results joined in submission order) so the model context
/// stays deterministic. Matches Claude Code's hook execution semantics.
pub fn run_hooks(
    config: &HookConfig,
    event: HookEvent,
    env: &HashMap<String, String>,
    working_dir: &Path,
) -> HookOutcome {
    let hooks = config.hooks_for(event);
    if hooks.is_empty() {
        return HookOutcome {
            output: String::new(),
            blocked: false,
            block_reason: None,
            results: Vec::new(),
        };
    }

    // Spawn one OS thread per hook. `run_one` is itself blocking (spawns
    // a child process and waits with a timeout), so threads are the
    // right primitive — no async runtime needed and child wait happens
    // off the caller's thread.
    let handles: Vec<_> = hooks
        .iter()
        .map(|hook| {
            let h = hook.clone();
            let e = env.clone();
            let d = working_dir.to_path_buf();
            std::thread::spawn(move || run_one(&h, &e, &d))
        })
        .collect();

    let mut results = Vec::with_capacity(hooks.len());
    let mut output_parts = Vec::new();
    let mut blocked = false;
    let mut block_reason = None;

    // Join in submission order so output_parts and the first-blocker
    // selection stay deterministic regardless of which thread finished
    // first.
    for (hook, handle) in hooks.iter().zip(handles) {
        let result = handle.join().unwrap_or_else(|_| HookResult {
            command: hook.command.clone(),
            exit_code: None,
            stdout: String::new(),
            stderr: "hook thread panicked".to_string(),
            success: false,
        });
        if !result.stdout.is_empty() {
            output_parts.push(result.stdout.clone());
        }
        if !result.success && hook.on_fail == OnFail::Block && !blocked {
            blocked = true;
            let reason = if result.stderr.is_empty() {
                format!(
                    "hook `{}` exited with code {:?}",
                    hook.command, result.exit_code
                )
            } else {
                result.stderr.trim().to_string()
            };
            block_reason = Some(reason);
        }
        results.push(result);
    }

    HookOutcome {
        output: output_parts.join("\n"),
        blocked,
        block_reason,
        results,
    }
}

fn run_one(hook: &HookEntry, env: &HashMap<String, String>, working_dir: &Path) -> HookResult {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&hook.command);
    cmd.current_dir(working_dir);
    cmd.envs(env);
    // Strip secret-shaped env vars so hook scripts cannot observe API keys.
    // Hooks are user-defined shell commands; a bug or malicious hook could
    // exfiltrate keys via a network call if they remain in the environment.
    let to_strip: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| crate::tools::is_secret_env_var(k))
        .collect();
    for k in to_strip {
        cmd.env_remove(k);
    }

    // Capture output, with timeout via wait_with_output after spawn.
    let child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return HookResult {
                command: hook.command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to spawn: {e}"),
                success: false,
            };
        }
    };

    let timeout = Duration::from_secs(hook.timeout_secs);
    match wait_with_timeout(child, timeout) {
        Ok((code, stdout, stderr)) => {
            let success = code == Some(0);
            HookResult {
                command: hook.command.clone(),
                exit_code: code,
                stdout,
                stderr,
                success,
            }
        }
        Err(msg) => HookResult {
            command: hook.command.clone(),
            exit_code: None,
            stdout: String::new(),
            stderr: msg,
            success: false,
        },
    }
}

/// Wait for a child with a wall-clock timeout. On timeout the child
/// is killed.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(Option<i32>, String, String), String> {
    use std::thread;
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = read_pipe(child.stdout.take());
                let stderr = read_pipe(child.stderr.take());
                return Ok((status.code(), stdout, stderr));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("hook timed out after {}s", timeout.as_secs()));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait error: {e}")),
        }
    }
}

fn read_pipe(pipe: Option<impl std::io::Read>) -> String {
    match pipe {
        Some(mut p) => {
            let mut buf = String::new();
            let _ = p.read_to_string(&mut buf);
            buf
        }
        None => String::new(),
    }
}

// ------------------------------------------------------------------
// Config loading
// ------------------------------------------------------------------

/// Loads hooks from `.metis/hooks.toml` (workspace) merged with
/// `~/.metis/hooks.toml` (user). Workspace hooks run first.
pub fn load_hooks(workspace: &Path) -> HookConfig {
    let mut config = HookConfig::default();

    // Workspace-level
    let ws_path = workspace.join(".metis").join("hooks.toml");
    if let Some(c) = load_file(&ws_path) {
        config = c;
    }

    // User-level
    if let Some(home) = dirs::home_dir() {
        let user_path = home.join(".metis").join("hooks.toml");
        if let Some(c) = load_file(&user_path) {
            config.merge(c);
        }
    }

    config
}

fn load_file(path: &Path) -> Option<HookConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Wraps hook output in a system-reminder tag for model injection.
pub fn format_hook_output(event: HookEvent, output: &str) -> String {
    if output.is_empty() {
        return String::new();
    }
    let label = event.key();
    format!("<system-reminder>\n{label} hook output: {output}\n</system-reminder>")
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_no_hooks() {
        let cfg = HookConfig::default();
        assert!(cfg.is_empty());
        assert!(cfg.hooks_for(HookEvent::SessionStart).is_empty());
    }

    #[test]
    fn parse_toml_config() {
        let toml_str = r#"
[[session_start]]
command = "echo hello"
on_fail = "warn"

[[pre_tool_use]]
command = "echo tool=$METIS_TOOL_NAME"
on_fail = "block"
timeout_secs = 5
"#;
        let cfg: HookConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.session_start.len(), 1);
        assert_eq!(cfg.session_start[0].command, "echo hello");
        assert_eq!(cfg.session_start[0].on_fail, OnFail::Warn);
        assert_eq!(cfg.session_start[0].timeout_secs, 10); // default

        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].on_fail, OnFail::Block);
        assert_eq!(cfg.pre_tool_use[0].timeout_secs, 5);
    }

    #[test]
    fn merge_appends() {
        let mut a = HookConfig {
            session_start: vec![HookEntry {
                command: "a".into(),
                on_fail: OnFail::Warn,
                timeout_secs: 10,
            }],
            ..Default::default()
        };
        let b = HookConfig {
            session_start: vec![HookEntry {
                command: "b".into(),
                on_fail: OnFail::Block,
                timeout_secs: 5,
            }],
            ..Default::default()
        };
        a.merge(b);
        assert_eq!(a.session_start.len(), 2);
        assert_eq!(a.session_start[0].command, "a");
        assert_eq!(a.session_start[1].command, "b");
    }

    #[test]
    fn run_echo_hook_captures_stdout() {
        let cfg = HookConfig {
            session_start: vec![HookEntry {
                command: "echo 'hook ran'".into(),
                on_fail: OnFail::Warn,
                timeout_secs: 5,
            }],
            ..Default::default()
        };
        let env = HashMap::new();
        let dir = std::env::temp_dir();
        let outcome = run_hooks(&cfg, HookEvent::SessionStart, &env, &dir);
        assert!(!outcome.blocked);
        assert!(outcome.output.contains("hook ran"));
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].success);
    }

    #[test]
    fn blocking_hook_failure_sets_blocked() {
        let cfg = HookConfig {
            pre_tool_use: vec![HookEntry {
                command: "echo 'denied' >&2; exit 1".into(),
                on_fail: OnFail::Block,
                timeout_secs: 5,
            }],
            ..Default::default()
        };
        let env = HashMap::new();
        let dir = std::env::temp_dir();
        let outcome = run_hooks(&cfg, HookEvent::PreToolUse, &env, &dir);
        assert!(outcome.blocked);
        assert!(outcome.block_reason.as_ref().unwrap().contains("denied"));
    }

    #[test]
    fn warn_hook_failure_does_not_block() {
        let cfg = HookConfig {
            compact: vec![HookEntry {
                command: "exit 1".into(),
                on_fail: OnFail::Warn,
                timeout_secs: 5,
            }],
            ..Default::default()
        };
        let env = HashMap::new();
        let dir = std::env::temp_dir();
        let outcome = run_hooks(&cfg, HookEvent::Compact, &env, &dir);
        assert!(!outcome.blocked);
        assert!(!outcome.results[0].success);
    }

    #[test]
    fn env_vars_passed_to_hook() {
        let cfg = HookConfig {
            pre_tool_use: vec![HookEntry {
                command: "echo $METIS_TOOL_NAME".into(),
                on_fail: OnFail::Warn,
                timeout_secs: 5,
            }],
            ..Default::default()
        };
        let mut env = HashMap::new();
        env.insert("METIS_TOOL_NAME".into(), "read_file".into());
        let dir = std::env::temp_dir();
        let outcome = run_hooks(&cfg, HookEvent::PreToolUse, &env, &dir);
        assert!(outcome.output.contains("read_file"));
    }

    #[test]
    fn format_hook_output_wraps_in_tag() {
        let out = format_hook_output(HookEvent::SessionStart, "Already up to date.");
        assert!(out.contains("<system-reminder>"));
        assert!(out.contains("session_start hook output:"));
        assert!(out.contains("Already up to date."));
    }

    #[test]
    fn format_hook_output_empty_returns_empty() {
        assert_eq!(format_hook_output(HookEvent::Compact, ""), "");
    }

    #[test]
    fn hook_timeout_kills_long_command() {
        let cfg = HookConfig {
            session_start: vec![HookEntry {
                command: "sleep 60".into(),
                on_fail: OnFail::Warn,
                timeout_secs: 1,
            }],
            ..Default::default()
        };
        let env = HashMap::new();
        let dir = std::env::temp_dir();
        let outcome = run_hooks(&cfg, HookEvent::SessionStart, &env, &dir);
        assert!(!outcome.results[0].success);
        assert!(outcome.results[0].stderr.contains("timed out"));
    }
}
