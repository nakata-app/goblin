use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::Command;

pub fn sandbox_exec_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "sandbox_exec".into(),
            description: "Executes a command in an isolated Docker container. The container has no network access, limited CPU/memory, and a temporary workspace. Results are returned after container exits. Requires Docker to be installed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute inside the sandbox container"
                    },
                    "image": {
                        "type": "string",
                        "description": "Docker image to use (default: 'ubuntu:22.04')"
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory inside the container (default: '/workspace')"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 60, max 300)"
                    },
                    "memory": {
                        "type": "string",
                        "description": "Memory limit (e.g. '256m', '512m', default '512m')"
                    },
                    "network": {
                        "type": "boolean",
                        "description": "Allow network access (default: false)"
                    },
                    "mounts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Host directories to mount as read-only (e.g. '/path/on/host:/path/in/container:ro')"
                    }
                },
                "required": ["command"]
            }),
        },
    }
}

pub fn sandbox_list_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "sandbox_list".into(),
            description: "Lists running sandbox containers and their status.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    }
}

pub async fn handle_sandbox_exec(args: serde_json::Value) -> Result<String, String> {
    let command = args["command"].as_str()
        .or_else(|| args["cmd"].as_str())
        .ok_or("command required — pass {\"command\": \"...\"}")?;
    let image = args["image"].as_str().unwrap_or("ubuntu:22.04");
    let workdir = args["workdir"].as_str().unwrap_or("/workspace");
    let timeout = args["timeout"].as_u64().unwrap_or(60).min(300);
    let memory = args["memory"].as_str().unwrap_or("512m");
    let network = args["network"].as_bool().unwrap_or(false);

    // Verify Docker is available
    let docker_check = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output();

    if docker_check.is_err() || !docker_check.unwrap().status.success() {
        return Err("Docker is not installed or not running. Install Docker from https://docker.com".to_string());
    }

    let container_name = format!("goblin-sandbox-{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("0"));

    let mut docker_args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(), container_name.clone(),
        "--memory".into(), memory.to_string(),
        "--cpus".into(), "1".to_string(),
        "--workdir".into(), workdir.to_string(),
    ];

    if !network {
        docker_args.push("--network".into());
        docker_args.push("none".into());
    }

    // Mount host directories if specified
    if let Some(mounts) = args["mounts"].as_array() {
        for mount in mounts {
            if let Some(m) = mount.as_str() {
                docker_args.push("-v".into());
                docker_args.push(m.to_string());
            }
        }
    }

    // Use a temporary volume for workspace
    docker_args.push("-v".into());
    docker_args.push(format!("{}_vol:{}", container_name, workdir));

    docker_args.push(image.to_string());
    docker_args.push("bash".into());
    docker_args.push("-c".into());

    // Wrap command with timeout
    let wrapped = format!("timeout {} bash -c '{}'", timeout, command.replace('\'', "'\\''"));
    docker_args.push(wrapped);

    let start = std::time::Instant::now();

    let output = Command::new("docker")
        .args(&docker_args)
        .output()
        .map_err(|e| format!("Docker execution failed: {}", e))?;

    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();
    result.push_str(&format!("Sandbox: {} ({} sec timeout, {} memory)\n", image, timeout, memory));

    if !stdout.is_empty() {
        result.push_str(&stdout);
    }

    if !stderr.is_empty() {
        if !result.ends_with('\n') { result.push('\n'); }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }

    if !output.status.success() {
        result.push_str(&format!(
            "\n[exit code: {} | elapsed: {:.1}s]",
            output.status.code().unwrap_or(-1),
            elapsed.as_secs_f64()
        ));
    } else {
        result.push_str(&format!("\n[completed in {:.1}s]", elapsed.as_secs_f64()));
    }

    let trimmed = result.trim().to_string();
    if trimmed.len() > 8000 {
        let mut end = 8000;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        Ok(format!("{}...\n\n[output truncated at 8000 chars]", &trimmed[..end]))
    } else {
        Ok(trimmed)
    }
}

pub async fn handle_sandbox_list(_args: serde_json::Value) -> Result<String, String> {
    let output = Command::new("docker")
        .args(["ps", "--filter", "name=goblin-sandbox-", "--format", "{{.ID}}\t{{.Names}}\t{{.Status}}\t{{.CreatedAt}}"])
        .output()
        .map_err(|e| format!("Docker command failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        Ok("No sandbox containers running.".to_string())
    } else {
        Ok(format!("Running sandbox containers:\n{}", stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_defs_exist() {
        assert_eq!(sandbox_exec_def().function.name, "sandbox_exec");
        assert_eq!(sandbox_list_def().function.name, "sandbox_list");
    }

    #[tokio::test]
    async fn sandbox_missing_command() {
        let result = handle_sandbox_exec(serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
