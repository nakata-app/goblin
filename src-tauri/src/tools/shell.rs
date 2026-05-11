use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::Command;

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
