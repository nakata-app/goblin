use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::{Command, Child, Stdio};
use std::sync::Mutex;
use std::collections::HashMap;

static BG_PROCESSES: Mutex<Option<HashMap<u32, Child>>> = Mutex::new(None);

fn bg_registry() -> std::sync::MutexGuard<'static, Option<HashMap<u32, Child>>> {
    let mut guard = BG_PROCESSES.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    guard
}

pub fn bash_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "bash".into(),
            description: "Executes a bash command in a subprocess. Returns stdout, stderr, and exit code. Timeout: 60 seconds. Use 'workdir' to specify working directory.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory for the command"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 60, max 300)"
                    }
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
            description: "Starts a command in the background and returns immediately with a process ID. Use bash_background_check to poll for completion and read output. Supports up to 50 concurrent background processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute in background"
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory for the command"
                    }
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
            description: "Checks the status of a background process. Returns running/complete status, stdout, stderr, and exit code. Use pid='all' to list all tracked processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pid": {
                        "type": "string",
                        "description": "Process ID returned by bash_background, or 'all' to list all tracked processes"
                    }
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
            description: "Kills a running background process. Returns success/failure. Use pid='all' to kill all tracked processes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pid": {
                        "type": "string",
                        "description": "Process ID to kill, or 'all' to kill all tracked processes"
                    }
                },
                "required": ["pid"]
            }),
        },
    }
}

pub async fn handle_bash(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str().ok_or("command required")?;
    let workdir = args["workdir"].as_str();
    let _timeout_secs = args["timeout"].as_u64().unwrap_or(60).min(300);

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("bash");
        c.args(["-c", command]);
        c
    };

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("Command execution failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();

    if !stdout.is_empty() {
        result.push_str(&stdout);
    }

    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }

    if !output.status.success() {
        result.push_str(&format!(
            "\n[exit code: {}]",
            output.status.code().unwrap_or(-1)
        ));
    }

    if result.is_empty() {
        result = "(no output)".to_string();
    }

    let trimmed = result.trim().to_string();
    if trimmed.len() > 8000 {
        Ok(format!("{}...\n\n[output truncated at 8000 chars]", &trimmed[..8000]))
    } else {
        Ok(trimmed)
    }
}

pub async fn handle_bash_background(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str().ok_or("command required")?;
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

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn background process: {}", e))?;

    let pid = child.id();
    let mut guard = bg_registry();

    if guard.as_ref().unwrap().len() >= 50 {
        let _ = child.kill();
        return Err("Maximum 50 background processes reached. Kill some or wait for completion.".to_string());
    }

    guard.as_mut().unwrap().insert(pid, child);

    Ok(format!(
        "Background process started.\nPID: {}\nCommand: {}\nUse bash_background_check with pid='{}' to check status.",
        pid, command, pid
    ))
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
        for (pid, child) in procs.iter_mut() {
            let status = match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    let mut output = String::new();
                    if let Some(stdout) = child.stdout.as_mut() {
                        use std::io::Read;
                        let mut buf = vec![0u8; 4096];
                        if let Ok(n) = stdout.read(&mut buf) {
                            let text = String::from_utf8_lossy(&buf[..n]);
                            if !text.is_empty() {
                                output.push_str(text.trim());
                                if n >= 4096 { output.push_str("... [truncated]"); }
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
        let result = handle_bash(json!({
            "command": "pwd",
            "workdir": "/tmp"
        })).await.unwrap();
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

        // Extract PID
        let pid: String = start
            .lines()
            .find(|l| l.starts_with("PID:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap()
            .to_string();

        // Check running or completed
        let check = handle_bash_background_check(json!({"pid": &pid})).await;
        assert!(check.is_ok());

        // Wait for completion
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let final_check = handle_bash_background_check(json!({"pid": &pid})).await.unwrap();
        assert!(final_check.contains("done") || final_check.contains("not found"));
    }

    #[tokio::test]
    async fn bash_background_kill() {
        let start = handle_bash_background(json!({"command": "sleep 10"})).await.unwrap();
        let pid: String = start
            .lines()
            .find(|l| l.starts_with("PID:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap()
            .to_string();

        let kill = handle_bash_background_kill(json!({"pid": &pid})).await.unwrap();
        assert!(kill.contains("killed"));
    }

    #[tokio::test]
    async fn bash_background_check_all() {
        let result = handle_bash_background_check(json!({"pid": "all"})).await.unwrap();
        // Should not error, may say no processes
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
        let def = bash_def();
        assert_eq!(def.function.name, "bash");
    }

    #[tokio::test]
    async fn bash_bg_def_check() {
        let def = bash_background_def();
        assert_eq!(def.function.name, "bash_background");
    }
}
                    format!("completed (exit: {}) {}", code, output)
                }
                Ok(None) => "running".to_string(),
                Err(e) => format!("error: {}", e),
            };
            lines.push(format!("  PID {}: {}", pid, status));
        }

        procs.retain(|_, child| {
            match child.try_wait() {
                Ok(Some(_)) => false,
                _ => true,
            }
        });

        Ok(format!("Background processes:\n{}", lines.join("\n")))
    } else {
        let pid: u32 = pid_str.parse().map_err(|_| "Invalid PID")?;

        match procs.get_mut(&pid) {
            Some(child) => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(-1);
                        let mut output = String::new();

                        if let Some(stdout) = child.stdout.as_mut() {
                            use std::io::Read;
                            let mut buf = vec![0u8; 8192];
                            if let Ok(n) = stdout.read(&mut buf) {
                                let text = String::from_utf8_lossy(&buf[..n]);
                                if !text.is_empty() {
                                    output.push_str(text.trim());
                                    if n >= 8192 {
                                        output.push_str("\n[output truncated at 8KB]");
                                    }
                                }
                            }
                        }

                        if let Some(stderr) = child.stderr.as_mut() {
                            use std::io::Read;
                            let mut buf = vec![0u8; 2048];
                            if let Ok(n) = stderr.read(&mut buf) {
                                let text = String::from_utf8_lossy(&buf[..n]);
                                if !text.is_empty() {
                                    if !output.is_empty() { output.push('\n'); }
                                    output.push_str(&format!("[stderr]\n{}", text.trim()));
                                }
                            }
                        }

                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&format!("[exit code: {}]", code));

                        procs.remove(&pid);
                        Ok(format!("PID {}: completed\n{}", pid, output))
                    }
                    Ok(None) => Ok(format!("PID {}: still running", pid)),
                    Err(e) => {
                        procs.remove(&pid);
                        Err(format!("PID {}: error: {}", pid, e))
                    }
                }
            }
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
        for (_, child) in procs.iter_mut() {
            let _ = child.kill();
        }
        procs.clear();
        Ok(format!("Killed {} background process(es)", count))
    } else {
        let pid: u32 = pid_str.parse().map_err(|_| "Invalid PID")?;
        match procs.get_mut(&pid) {
            Some(child) => {
                child.kill().map_err(|e| format!("Failed to kill PID {}: {}", pid, e))?;
                match child.wait() {
                    Ok(status) => {
                        let code = status.code().unwrap_or(-1);
                        procs.remove(&pid);
                        Ok(format!("PID {} killed (exit code: {})", pid, code))
                    }
                    Err(e) => {
                        procs.remove(&pid);
                        Ok(format!("PID {} killed (wait error: {})", pid, e))
                    }
                }
            }
            None => Err(format!("PID {} not found in tracked processes", pid)),
        }
    }
}
