//! Bash tool — process-level safeguards + OS sandboxing.
//!
//! Three failure modes hurt the agent loop: hangs, runaway output, and
//! silently leaked secrets. We guard all three with a wall-clock
//! timeout, a byte cap on combined stdout+stderr, and an env-var scrub
//! of known API-key / token patterns. OS-level isolation
//! (`sandbox-exec` on macOS, `bwrap` on Linux) is opt-in via
//! `ToolContext::bash.sandbox`.

use std::process::{Command, Stdio};
use std::sync::atomic::Ordering as AO;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{SandboxMode, Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------

pub struct Bash;

#[derive(Debug, Deserialize)]
struct BashArgs {
    #[serde(default)]
    command: Option<String>,
    /// If true, spawn the command in the background and return
    /// immediately with a process ID.
    #[serde(default)]
    run_in_background: bool,
    /// Optional timeout override in milliseconds (max 600000).
    #[serde(default)]
    timeout: Option<u64>,
    /// Rerun a previous command by alias (e.g. "b3").
    #[serde(default)]
    rerun: Option<String>,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command through the user's `$SHELL`. Working directory \
         persists between calls (a `cd src && ls` in one call leaves the next \
         call inside `src/`) but shell state does not (variable exports, \
         aliases, and functions reset each call since every run is a fresh \
         subshell). Returns combined stdout/stderr and the exit code. \
         Process-level safeguards: wall-clock timeout, output cap, and \
         secret-env scrubbing — the model otherwise has the authority of \
         the user running metis."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line." },
                "run_in_background": { "type": "boolean", "description": "If true, run in background and return process ID." },
                "timeout": { "type": "integer", "description": "Timeout in milliseconds (max 600000).", "maximum": 600000 },
                "rerun": { "type": "string", "description": "Rerun a prior command by alias (e.g. 'b3'). Mutually exclusive with command." }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: BashArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Resolve the command: either from `command` or `rerun` alias
        let command = if let Some(alias) = &args.rerun {
            let idx = parse_rerun_alias(alias)?;
            let history = ctx.bash_history.lock().unwrap();
            history.get(idx).cloned().ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "rerun alias `{alias}` not found (history has {} entries)",
                    history.len()
                ))
            })?
        } else {
            args.command.clone().ok_or_else(|| {
                ToolError::InvalidArgs("either `command` or `rerun` is required".to_string())
            })?
        };

        // Background mode: spawn and return immediately with PID
        if args.run_in_background {
            return run_bash_background(&command, ctx).await;
        }

        // Optionally compress output via RTK if it's on PATH
        let command = try_rtk_rewrite(&command).await.unwrap_or(command);

        // Wrap the user command so we can capture the final `pwd` on a
        // stable sentinel line. After the child exits we parse this and
        // stick it into `ctx.bash_cwd` — next `bash` call picks it up.
        // Contract: working directory persists between commands, but
        // shell state does not. Variable exports / aliases / functions
        // still reset per call since each run is a fresh subshell —
        // only CWD carries over, by design.
        const CWD_SENTINEL: &str = "__METIS_CWD__";
        let wrapped_cmd = format!(
            "{{ {command}\n}}; __metis_exit=$?; printf '\\n{CWD_SENTINEL}=%s\\n' \"$(pwd)\"; exit $__metis_exit"
        );

        // Pick the user's shell so aliases/profile-driven PATH tweaks
        // work the way the user expects (zsh vs bash vs dash). $SHELL
        // falls back to /bin/sh for non-interactive environments.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let mut cmd = match &ctx.bash.sandbox {
            SandboxMode::None => {
                let mut c = tokio::process::Command::new(&shell);
                c.arg("-c").arg(&wrapped_cmd);
                c
            }
            SandboxMode::SandboxExec => {
                // macOS sandbox-exec: generate a SBPL profile that allows
                // reads everywhere but restricts writes to the workspace.
                let ws = ctx.effective_root().display().to_string();
                let profile = format!(
                    "(version 1)\n\
                     (allow default)\n\
                     (deny file-write*)\n\
                     (allow file-write* (subpath \"{ws}\"))\n\
                     (allow file-write* (subpath \"/private/tmp\"))\n\
                     (allow file-write* (subpath \"/tmp\"))\n\
                     (allow file-write* (subpath \"/dev\"))"
                );
                let mut c = tokio::process::Command::new("sandbox-exec");
                c.args(["-p", &profile, &shell, "-c", &wrapped_cmd]);
                c
            }
            SandboxMode::Bubblewrap => {
                // Linux bwrap: mount workspace rw, everything else ro.
                let ws = ctx.effective_root().display().to_string();
                let mut c = tokio::process::Command::new("bwrap");
                c.args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--bind",
                    &ws,
                    &ws,
                    "--bind",
                    "/tmp",
                    "/tmp",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--unshare-net",
                    &shell,
                    "-c",
                    &wrapped_cmd,
                ]);
                c
            }
        };
        // Sticky cwd: use whatever the last bash call left us in, or
        // fall back to workspace root on first call / fresh session.
        let initial_cwd = ctx
            .bash_cwd
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| ctx.effective_root());
        cmd.current_dir(&initial_cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Put the child in its own process group so a timeout can wipe out
        // any grandchildren too.
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        // Strip secret-shaped env vars before the child sees them.
        if ctx.bash.scrub_secret_env {
            let to_strip: Vec<String> = std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| ctx.bash.should_scrub(k))
                .collect();
            for k in to_strip {
                cmd.env_remove(k);
            }
        }

        let mut child = cmd.spawn().map_err(|e| ToolError::Spawn(e.to_string()))?;

        // Take pipes for async reading
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let timeout = match args.timeout {
            Some(ms) => Duration::from_millis(ms.min(600_000)),
            None => ctx.bash.timeout,
        };

        // Wait for process with timeout + cancel flag, reading stdout/stderr concurrently.
        // The process is moved into a spawned subtask so we can abort it (and kill the
        // subprocess) cleanly when the cancel flag fires without borrow conflicts.
        let mut timed_out = false;
        let mut cancelled = false;
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();

        let child_pid = child.id(); // capture before move
        let cancel_flag = ctx.cancel_flag.clone();

        // Spawn the blocking wait so we can race it against cancel.
        type WaitResult = (Vec<u8>, Vec<u8>, std::io::Result<std::process::ExitStatus>);
        let bash_task: tokio::task::JoinHandle<Result<WaitResult, tokio::time::error::Elapsed>> =
            tokio::spawn(tokio::time::timeout(timeout, async move {
                let (stdout_res, stderr_res, wait_res) = tokio::join!(
                    async {
                        if let Some(ref mut pipe) = stdout_pipe {
                            use tokio::io::AsyncReadExt;
                            let mut buf = Vec::new();
                            let _ = pipe.read_to_end(&mut buf).await;
                            buf
                        } else {
                            Vec::new()
                        }
                    },
                    async {
                        if let Some(ref mut pipe) = stderr_pipe {
                            use tokio::io::AsyncReadExt;
                            let mut buf = Vec::new();
                            let _ = pipe.read_to_end(&mut buf).await;
                            buf
                        } else {
                            Vec::new()
                        }
                    },
                    child.wait()
                );
                (stdout_res, stderr_res, wait_res)
            }));

        // Poll cancel flag every 100ms alongside the process wait.
        let cancel_watch = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if cancel_flag.load(AO::Relaxed) {
                    return;
                }
            }
        };

        // Helper: kill process group by PID.
        let kill_by_pid = |pid: Option<u32>| {
            #[cfg(unix)]
            if let Some(pid) = pid {
                let pgid = pid as i32;
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pgid}")])
                    .status();
            }
            #[cfg(not(unix))]
            let _ = pid;
        };

        let abort_handle = bash_task.abort_handle();
        let exit_code = tokio::select! {
            task_result = bash_task => {
                match task_result {
                    Ok(Ok((so, se, wait_res))) => {
                        stdout_bytes = so;
                        stderr_bytes = se;
                        wait_res.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)
                    }
                    Ok(Err(_timeout)) => {
                        timed_out = true;
                        kill_by_pid(child_pid);
                        -1
                    }
                    Err(_join_err) => {
                        // Task was aborted (cancel path) or panicked
                        kill_by_pid(child_pid);
                        -1
                    }
                }
            }
            _ = cancel_watch => {
                cancelled = true;
                abort_handle.abort();
                kill_by_pid(child_pid);
                -1
            }
        };

        // Parse the CWD sentinel off the end of stdout, update sticky
        // cwd, and strip the sentinel line(s) before showing output.
        // Shells flush the wrapped `printf` on the last line, so a
        // successful command always produces at least one sentinel.
        // A syntax-error in the user's command skips the sentinel
        // entirely — in that case we leave bash_cwd alone.
        if let Ok(text) = std::str::from_utf8(&stdout_bytes) {
            if let Some(sentinel_start) = text.rfind(&format!("\n{CWD_SENTINEL}=")) {
                let after = &text[sentinel_start + 1..];
                if let Some(line_end) = after.find('\n') {
                    let value = &after[CWD_SENTINEL.len() + 1..line_end];
                    let new_cwd = std::path::PathBuf::from(value);
                    if new_cwd.is_dir() {
                        *ctx.bash_cwd.lock().unwrap() = Some(new_cwd);
                    }
                }
                // Strip everything from the preceding newline onwards
                // so the sentinel never leaks to the model.
                stdout_bytes.truncate(sentinel_start);
            } else if text.starts_with(&format!("{CWD_SENTINEL}=")) {
                // Edge case: command produced no stdout, so the
                // sentinel is the only line. Same stripping.
                if let Some(line_end) = text.find('\n') {
                    let value = &text[CWD_SENTINEL.len() + 1..line_end];
                    let new_cwd = std::path::PathBuf::from(value);
                    if new_cwd.is_dir() {
                        *ctx.bash_cwd.lock().unwrap() = Some(new_cwd);
                    }
                }
                stdout_bytes.clear();
            }
        }
        let total = stdout_bytes.len() + stderr_bytes.len();
        let cap = ctx.bash.max_output_bytes;

        // Apply the cap proportionally so a flood on one stream doesn't
        // starve the other entirely. Most well-behaved commands fit easily.
        let (stdout_show, stderr_show, truncated) = if total <= cap {
            (stdout_bytes.as_slice(), stderr_bytes.as_slice(), false)
        } else if stdout_bytes.is_empty() {
            (&[][..], &stderr_bytes[..cap.min(stderr_bytes.len())], true)
        } else if stderr_bytes.is_empty() {
            (&stdout_bytes[..cap.min(stdout_bytes.len())], &[][..], true)
        } else {
            let half = cap / 2;
            let so = &stdout_bytes[..half.min(stdout_bytes.len())];
            let se = &stderr_bytes[..(cap - so.len()).min(stderr_bytes.len())];
            (so, se, true)
        };

        let stdout = String::from_utf8_lossy(stdout_show);
        let stderr = String::from_utf8_lossy(stderr_show);
        let mut out = String::new();
        if !stdout.is_empty() {
            out.push_str(&format!("[stdout]\n{stdout}"));
        }
        if !stderr.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("[stderr]\n{stderr}"));
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if truncated {
            out.push_str(&format!(
                "[truncated: {} bytes total, capped at {} bytes]\n",
                total, cap
            ));
        }
        if timed_out {
            out.push_str(&format!(
                "[killed: exceeded {}s wall-clock budget]\n",
                timeout.as_secs()
            ));
        }
        if cancelled {
            out.push_str("[killed: interrupted by user]\n");
        }
        out.push_str(&format!("[exit] {exit_code}\n"));

        // Record in history and append rerun alias
        let mut history = ctx.bash_history.lock().unwrap();
        history.push(command);
        let alias_idx = history.len(); // 1-based for display
        out.push_str(&format!("[rerun: b{alias_idx}]\n"));

        Ok(out)
    }
}

async fn try_rtk_rewrite(cmd: &str) -> Option<String> {
    let output = tokio::process::Command::new("rtk")
        .args(["rewrite", cmd])
        .output()
        .await
        .ok()?;
    if output.status.success() {
        let rewritten = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !rewritten.is_empty() && rewritten != cmd {
            return Some(rewritten);
        }
    }
    None
}

/// Parse a rerun alias like "b3" into a 0-based index (2).
fn parse_rerun_alias(alias: &str) -> Result<usize, ToolError> {
    let alias = alias.trim();
    let num_str = alias.strip_prefix('b').unwrap_or(alias);
    let n: usize = num_str.parse().map_err(|_| {
        ToolError::InvalidArgs(format!("invalid rerun alias: `{alias}` (expected bN)"))
    })?;
    if n == 0 {
        return Err(ToolError::InvalidArgs("rerun alias is 1-based".to_string()));
    }
    Ok(n - 1) // convert to 0-based
}

/// Run a command in the background. Output is redirected to a temp file
/// that the model can read later.
async fn run_bash_background(command: &str, ctx: &ToolContext) -> Result<String, ToolError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static BG_COUNTER: AtomicU64 = AtomicU64::new(1);
    let id = BG_COUNTER.fetch_add(1, Ordering::Relaxed);

    let out_path = std::env::temp_dir().join(format!("metis-bg-{id}.out"));
    let out_path_str = out_path.display().to_string();

    // Wrap command: redirect stdout+stderr to file, run in background
    let wrapped = format!("({command}) > {out_path_str} 2>&1 &\necho $!",);

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&wrapped)
        .current_dir(ctx.effective_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if ctx.bash.scrub_secret_env {
        let to_strip: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| ctx.bash.should_scrub(k))
            .collect();
        for k in to_strip {
            cmd.env_remove(k);
        }
    }

    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Spawn(e.to_string()))?;
    let pid = String::from_utf8_lossy(&output.stdout).trim().to_string();

    Ok(format!(
        "[background] id={id} pid={pid}\n\
         Output file: {out_path_str}\n\
         Read output later: bash command=\"cat {out_path_str}\"\n"
    ))
}
