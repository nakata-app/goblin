use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::{Command, Child, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::collections::HashMap;
use regex::Regex;

/// Shell guardrails. `allowlist` is empty by default (every command
/// runs); when non-empty, a command must match at least one pattern.
/// `blocklist` is always checked. Both are applied to the raw command
/// string before spawn — this is the same string passed to `bash -c`.
struct ShellGuards {
    allowlist: Vec<Regex>,
    blocklist: Vec<Regex>,
}

impl ShellGuards {
    const fn empty() -> Self {
        Self { allowlist: Vec::new(), blocklist: Vec::new() }
    }

    fn check(&self, command: &str) -> Result<(), String> {
        // Blocklist wins. A pattern here always rejects, even if the
        // user also put the command in the allowlist by accident.
        for re in &self.blocklist {
            if re.is_match(command) {
                return Err(format!(
                    "Command rejected by shell_blocklist pattern: {}",
                    re.as_str()
                ));
            }
        }
        // Empty allowlist means "no allowlist enforced".
        if !self.allowlist.is_empty()
            && !self.allowlist.iter().any(|re| re.is_match(command))
        {
            return Err(
                "Command rejected: not in shell_allowlist. Add a matching regex to config.toml [tools] shell_allowlist or run the command yourself.".to_string()
            );
        }
        Ok(())
    }
}

static SHELL_GUARDS: RwLock<ShellGuards> = RwLock::new(ShellGuards::empty());

/// Replace the live shell guardrails. Called once at startup and again
/// from `save_config` whenever the tool registry is rebuilt. Bad regex
/// patterns are reported to stderr and skipped so one typo cannot
/// disable shell entirely.
pub fn apply_shell_guards(allowlist: &[String], blocklist: &[String]) {
    let compile = |patterns: &[String], label: &str| -> Vec<Regex> {
        patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    eprintln!("[shell] {} pattern '{}' ignored: {}", label, p, e);
                    None
                }
            })
            .collect()
    };
    let guards = ShellGuards {
        allowlist: compile(allowlist, "shell_allowlist"),
        blocklist: compile(blocklist, "shell_blocklist"),
    };
    if let Ok(mut g) = SHELL_GUARDS.write() {
        *g = guards;
    }
}

fn check_shell_guards(command: &str) -> Result<(), String> {
    SHELL_GUARDS
        .read()
        .ok()
        .map(|g| g.check(command))
        .unwrap_or(Ok(()))
}

/// A backgrounded child plus the rolling output buffers its reader
/// threads are filling. We keep the threads draining stdout/stderr
/// continuously so the kernel pipe buffer (64KB on macOS by default)
/// never fills and stalls the producer.
struct BgProc {
    child: Child,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
}

// Cap how much we keep in memory per process so a runaway emitter
// doesn't OOM Goblin. Older bytes are dropped once we exceed it.
const BG_BUF_CAP: usize = 256 * 1024;

static BG_PROCESSES: Mutex<Option<HashMap<u32, BgProc>>> = Mutex::new(None);

fn bg_registry() -> std::sync::MutexGuard<'static, Option<HashMap<u32, BgProc>>> {
    let mut guard = BG_PROCESSES.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    guard
}

fn spawn_drainer<R: std::io::Read + Send + 'static>(mut src: R, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match src.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    if let Ok(mut g) = buf.lock() {
                        g.extend_from_slice(&chunk[..n]);
                        // Trim from the front once we overshoot the cap.
                        if g.len() > BG_BUF_CAP {
                            let overflow = g.len() - BG_BUF_CAP;
                            g.drain(..overflow);
                        }
                    }
                }
                Err(_) => return,
            }
        }
    });
}

fn snapshot_buf(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    buf.lock()
        .ok()
        .map(|g| String::from_utf8_lossy(&g).to_string())
        .unwrap_or_default()
}

pub fn bash_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "bash".into(),
            description: "Executes a shell command. Returns stdout, stderr, and exit code. Timeout: 60 seconds. Uses bash on macOS/Linux, cmd on Windows.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute"},
                    "workdir": {"type": "string", "description": "Working directory for the command"},
                    "timeout": {"type": "integer", "description": "Timeout in seconds (default 60, max 300)"}
                },
                "required": ["command"]
            }),
        },
    }
}

pub fn bash_background_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "bash_background".into(),
            description: "Starts a command in the background and returns immediately with a process ID. Supports up to 50 concurrent processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute in background"},
                    "workdir": {"type": "string", "description": "Working directory for the command"}
                },
                "required": ["command"]
            }),
        },
    }
}

pub fn bash_background_check_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "bash_background_check".into(),
            description: "Checks the status of a background process. Use pid='all' to list all tracked processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pid": {"type": "string", "description": "Process ID or 'all'"}
                },
                "required": ["pid"]
            }),
        },
    }
}

pub fn bash_background_kill_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "bash_background_kill".into(),
            description: "Kills a running background process. Use pid='all' to kill all tracked processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pid": {"type": "string", "description": "Process ID to kill, or 'all'"}
                },
                "required": ["pid"]
            }),
        },
    }
}

pub async fn handle_bash(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str().ok_or("command required")?;
    check_shell_guards(command)?;
    let workdir = args["workdir"].as_str();
    let timeout = args["timeout"].as_u64().unwrap_or(60).clamp(1, 300);

    // tokio::process::Command runs cooperatively in the async runtime, so the
    // timeout below can actually interrupt the wait. The old std::process
    // variant blocked the runtime thread and made the timeout cosmetic.
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.args(["-c", command]);
        c
    };

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        cmd.output(),
    ).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("Command execution failed: {}", e)),
        Err(_) => return Err(format!("Command timed out after {} seconds", timeout)),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();
    if !stdout.is_empty() { result.push_str(&stdout); }
    if !stderr.is_empty() {
        if !result.is_empty() { result.push('\n'); }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }
    if !output.status.success() {
        result.push_str(&format!("\n[exit code: {}]", output.status.code().unwrap_or(-1)));
    }
    if result.is_empty() { result = "(no output)".to_string(); }

    let trimmed = result.trim().to_string();
    Ok(truncate_utf8(&trimmed, 8000, "\n\n[output truncated at 8000 chars]"))
}

/// Truncate a string to at most `max_bytes` while respecting UTF-8
/// character boundaries. Appends `suffix` if truncation actually
/// happened. Used by tool output formatters where the body can contain
/// TR/emoji/CJK bytes that would otherwise panic a naive `&s[..n]`.
fn truncate_utf8(s: &str, max_bytes: usize, suffix: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...{}", &s[..end], suffix)
}

pub async fn handle_bash_background(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str().ok_or("command required")?;
    check_shell_guards(command)?;
    let workdir = args["workdir"].as_str();

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("bash");
        c.args(["-c", command]);
        c
    };

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if let Some(dir) = workdir { cmd.current_dir(dir); }

    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn background process: {}", e))?;
    let pid = child.id();

    // Detach reader threads on each pipe *before* parking the child in
    // the registry. Without this, a process that writes more than
    // ~64 KB of stdout (the macOS default pipe buffer) blocks on its
    // next write and never finishes.
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(s) = child.stdout.take() {
        spawn_drainer(s, stdout_buf.clone());
    }
    if let Some(s) = child.stderr.take() {
        spawn_drainer(s, stderr_buf.clone());
    }

    let mut guard = bg_registry();

    if guard.as_ref().unwrap().len() >= 50 {
        let _ = child.kill();
        return Err("Maximum 50 background processes reached. Kill some or wait for completion.".to_string());
    }
    guard.as_mut().unwrap().insert(pid, BgProc { child, stdout_buf, stderr_buf });

    Ok(format!("Background process started.\nPID: {}\nCommand: {}", pid, command))
}

pub async fn handle_bash_background_check(args: serde_json::Value) -> Result<String, String> {
    let pid_str = args["pid"].as_str().ok_or("pid required")?;
    let mut guard = bg_registry();
    let procs = guard.as_mut().unwrap();

    if pid_str == "all" {
        if procs.is_empty() {
            return Ok("No background processes tracked.".to_string());
        }
        let mut lines = Vec::new();
        let mut done: Vec<u32> = Vec::new();
        for (pid, proc) in procs.iter_mut() {
            let status = match proc.child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    let stdout = snapshot_buf(&proc.stdout_buf);
                    let snippet = stdout.trim();
                    done.push(*pid);
                    if snippet.is_empty() {
                        format!("completed (exit: {})", code)
                    } else {
                        format!("completed (exit: {}) {}", code, snippet)
                    }
                }
                Ok(None) => "running".to_string(),
                Err(e) => format!("error: {}", e),
            };
            lines.push(format!("  PID {}: {}", pid, status));
        }

        for pid in done {
            procs.remove(&pid);
        }
        Ok(format!("Background processes:\n{}", lines.join("\n")))
    } else {
        let pid: u32 = pid_str.parse().map_err(|_| "Invalid PID")?;
        match procs.get_mut(&pid) {
            Some(proc) => match proc.child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    let stdout = snapshot_buf(&proc.stdout_buf);
                    let stderr = snapshot_buf(&proc.stderr_buf);

                    let mut output = String::new();
                    if !stdout.trim().is_empty() {
                        output.push_str(stdout.trim());
                    }
                    if !stderr.trim().is_empty() {
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&format!("[stderr]\n{}", stderr.trim()));
                    }
                    if !output.is_empty() { output.push('\n'); }
                    output.push_str(&format!("[exit code: {}]", code));
                    procs.remove(&pid);
                    Ok(format!("PID {}: completed\n{}", pid, output))
                }
                Ok(None) => Ok(format!("PID {}: still running", pid)),
                Err(e) => { procs.remove(&pid); Err(format!("PID {}: error: {}", pid, e)) }
            },
            None => Err(format!("PID {} not found in tracked processes", pid)),
        }
    }
}

pub async fn handle_bash_background_kill(args: serde_json::Value) -> Result<String, String> {
    let pid_str = args["pid"].as_str().ok_or("pid required")?;
    let mut guard = bg_registry();
    let procs = guard.as_mut().unwrap();

    if pid_str == "all" {
        let count = procs.len();
        for (_, proc) in procs.iter_mut() { let _ = proc.child.kill(); }
        procs.clear();
        Ok(format!("Killed {} background process(es)", count))
    } else {
        let pid: u32 = pid_str.parse().map_err(|_| "Invalid PID")?;
        match procs.get_mut(&pid) {
            Some(proc) => {
                proc.child.kill().map_err(|e| format!("Failed to kill PID {}: {}", pid, e))?;
                match proc.child.wait() {
                    Ok(status) => { procs.remove(&pid); Ok(format!("PID {} killed (exit code: {})", pid, status.code().unwrap_or(-1))) }
                    Err(e) => { procs.remove(&pid); Ok(format!("PID {} killed (wait error: {})", pid, e)) }
                }
            }
            None => Err(format!("PID {} not found in tracked processes", pid)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_echo() {
        let result = handle_bash(json!({"command": "echo hello"})).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn bash_with_stderr() {
        let result = handle_bash(json!({"command": "echo error >&2"})).await.unwrap();
        assert!(result.contains("[stderr]"));
    }

    #[tokio::test]
    async fn bash_exit_code() {
        let result = handle_bash(json!({"command": "exit 1"})).await.unwrap();
        assert!(result.contains("[exit code: 1]"));
    }

    #[tokio::test]
    async fn bash_success_no_exit_code_marker() {
        let result = handle_bash(json!({"command": "echo ok"})).await.unwrap();
        assert!(!result.contains("[exit code:"));
    }

    #[tokio::test]
    async fn bash_workdir() {
        let result = handle_bash(json!({"command": "pwd", "workdir": "/tmp"})).await.unwrap();
        assert!(result.contains("/tmp"));
    }

    #[tokio::test]
    async fn bash_no_output() {
        let result = handle_bash(json!({"command": "true"})).await.unwrap();
        assert!(result.contains("(no output)"));
    }

    #[tokio::test]
    async fn bash_background_start_and_check() {
        let start = handle_bash_background(json!({"command": "sleep 0.5 && echo done"})).await.unwrap();
        assert!(start.contains("PID:"));
        let pid: String = start.lines().find(|l| l.starts_with("PID:")).and_then(|l| l.split_whitespace().nth(1)).unwrap().to_string();
        let check = handle_bash_background_check(json!({"pid": &pid})).await;
        assert!(check.is_ok());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let final_check = handle_bash_background_check(json!({"pid": &pid})).await.unwrap();
        assert!(final_check.contains("done") || final_check.contains("not found"));
    }

    #[tokio::test]
    async fn bash_background_kill() {
        let start = handle_bash_background(json!({"command": "sleep 10"})).await.unwrap();
        let pid: String = start.lines().find(|l| l.starts_with("PID:")).and_then(|l| l.split_whitespace().nth(1)).unwrap().to_string();
        let kill = handle_bash_background_kill(json!({"pid": &pid})).await.unwrap();
        assert!(kill.contains("killed"));
    }

    #[tokio::test]
    async fn bash_background_check_all() {
        let result = handle_bash_background_check(json!({"pid": "all"})).await.unwrap();
        assert!(result.contains("processes") || result.contains("No background"));
    }

    #[tokio::test]
    async fn bash_background_invalid_pid() {
        let result = handle_bash_background_check(json!({"pid": "99999999"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bash_background_kill_all() {
        let result = handle_bash_background_kill(json!({"pid": "all"})).await.unwrap();
        assert!(result.contains("Killed"));
    }

    #[tokio::test]
    async fn bash_def_check() {
        assert_eq!(bash_def().function.name, "bash");
    }

    #[tokio::test]
    async fn bash_bg_def_check() {
        assert_eq!(bash_background_def().function.name, "bash_background");
    }

    // Regression: previously the truncation slice `&trimmed[..8000]` could
    // land in the middle of a multi-byte UTF-8 sequence and panic. We feed
    // the formatter a string with TR + emoji and assert it does not panic
    // and returns a valid string.
    #[test]
    fn truncate_utf8_respects_char_boundaries() {
        // 4-byte emoji repeated past the 8000-byte budget.
        let s = "ç".repeat(5000) + &"🦀".repeat(1000);
        let out = truncate_utf8(&s, 8000, "[trunc]");
        assert!(out.is_char_boundary(out.len()));
        assert!(out.ends_with("[trunc]"));
        // The reported byte count should still fit under the budget plus
        // suffix length, never exceed the input.
        assert!(out.len() <= s.len() + 32);
    }

    #[test]
    fn truncate_utf8_short_string_unchanged() {
        let s = "hello çay 🌱";
        assert_eq!(truncate_utf8(s, 8000, "[trunc]"), s);
    }

    // Regression: previously bash_background piped stdout into the
    // registry and only drained it on check_bg. A process emitting more
    // than the 64 KB macOS pipe buffer would block on its next write and
    // never finish. With reader threads attached at spawn time, even a
    // ~200 KB burst should complete cleanly.
    //
    // NOTE: this test exercises the global BG_PROCESSES registry, which
    // other tests in this module also touch (kill_all clears it). Cargo
    // runs tests in parallel by default, so we tolerate "not found" as a
    // valid completion signal: another test wiped the entry after our
    // process finished, which still proves the drain worked.
    #[tokio::test]
    async fn bash_background_handles_large_output() {
        // ~200 KB of `a`s via printf — well above the macOS pipe buffer.
        let start = handle_bash_background(json!({
            "command": "printf 'a%.0s' $(seq 1 204800); echo DONE"
        })).await.unwrap();
        let pid: String = start.lines()
            .find(|l| l.starts_with("PID:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap()
            .to_string();

        let mut completed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match handle_bash_background_check(json!({"pid": &pid})).await {
                Ok(check) => {
                    if check.contains("completed") || check.contains("DONE") {
                        completed = true;
                        break;
                    }
                }
                Err(e) if e.contains("not found") => {
                    // Reaped by a parallel test after we finished — still
                    // proves the process did not hang on a full pipe.
                    completed = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {}", e),
            }
        }
        assert!(completed, "Process never completed — likely blocked on full pipe");
    }

    #[test]
    fn shell_guards_allowlist_allows_matching_command() {
        let g = ShellGuards {
            allowlist: vec![Regex::new("^ls").unwrap()],
            blocklist: vec![],
        };
        assert!(g.check("ls -la").is_ok());
        assert!(g.check("rm -rf /").is_err());
    }

    #[test]
    fn shell_guards_blocklist_rejects_even_when_allowlisted() {
        let g = ShellGuards {
            allowlist: vec![Regex::new(".*").unwrap()],
            blocklist: vec![Regex::new("rm -rf /").unwrap()],
        };
        assert!(g.check("ls").is_ok());
        let err = g.check("rm -rf /").unwrap_err();
        assert!(err.contains("blocklist"));
    }

    #[test]
    fn shell_guards_empty_allowlist_permits_everything() {
        let g = ShellGuards { allowlist: vec![], blocklist: vec![] };
        assert!(g.check("ls").is_ok());
        assert!(g.check("rm -rf /").is_ok());
    }

    // The unit tests on the pure ShellGuards struct (above) already cover
    // allow/block semantics. An integration test that mutates the global
    // SHELL_GUARDS while handle_bash runs would race with every other
    // shell test in this module (cargo runs them in parallel), so we
    // exercise that path only via the struct-level tests and trust the
    // single call site `check_shell_guards` to wire them together.

    #[tokio::test]
    async fn bash_timeout_clamps_to_one_second_minimum() {
        // timeout=0 used to mean "no wait", which made every short command
        // race against an instant cancellation. We now clamp to >= 1s.
        let result = handle_bash(json!({"command": "echo hello", "timeout": 0})).await.unwrap();
        assert!(result.contains("hello"));
    }
}
