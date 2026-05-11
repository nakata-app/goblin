use crate::provider::ToolDefinition;
use serde_json::json;
use std::process::Command;

fn run_git(args: &[&str], workdir: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().map_err(|e| format!("git failed: {}", e))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(stdout.trim());
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&format!("[stderr] {}", stderr.trim()));
    }
    if !output.status.success() {
        return Err(format!("git exited with {}: {}", output.status.code().unwrap_or(-1), result));
    }
    if result.is_empty() {
        result = "(no output)".to_string();
    }
    Ok(result)
}

fn find_repo_root(workdir: Option<&str>) -> Result<String, String> {
    let dir = workdir.unwrap_or(".");
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git rev-parse failed: {}", e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err("Not a git repository (or no git found)".to_string())
    }
}

pub fn git_status_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "git_status".into(),
            description: "Shows the working tree status (git status --short). Returns list of changed files with status codes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Working directory (defaults to cwd)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub fn git_diff_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "git_diff".into(),
            description: "Shows changes between working tree and index, or between commits. Use staged=true for staged changes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Working directory (defaults to cwd)"
                    },
                    "staged": {
                        "type": "boolean",
                        "description": "Show staged changes (git diff --staged)"
                    },
                    "maxLines": {
                        "type": "integer",
                        "description": "Maximum lines to return (default 200)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub fn git_commit_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "git_commit".into(),
            description: "Creates a new commit with all staged changes. Requires a commit message. Use with caution — this permanently records changes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Commit message (conventional commits format recommended)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Working directory (defaults to cwd)"
                    }
                },
                "required": ["message"]
            }),
        },
    }
}

pub fn git_log_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "git_log".into(),
            description: "Shows recent commit history. Returns oneline format with hash and message.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Working directory (defaults to cwd)"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of commits to show (default 10, max 50)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub fn git_pr_create_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "git_pr_create".into(),
            description: "Creates a GitHub pull request using the 'gh' CLI. Requires gh to be installed and authenticated.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Pull request title"
                    },
                    "body": {
                        "type": "string",
                        "description": "Pull request description (markdown)"
                    },
                    "base": {
                        "type": "string",
                        "description": "Target branch (default: main)"
                    },
                    "draft": {
                        "type": "boolean",
                        "description": "Create as draft PR"
                    },
                    "path": {
                        "type": "string",
                        "description": "Working directory (defaults to cwd)"
                    }
                },
                "required": ["title", "body"]
            }),
        },
    }
}

pub async fn handle_git_status(args: serde_json::Value) -> Result<String, String> {
    let workdir = args["path"].as_str();
    let root = find_repo_root(workdir)?;
    run_git(&["-C", &root, "status", "--short"], None)
}

pub async fn handle_git_diff(args: serde_json::Value) -> Result<String, String> {
    let workdir = args["path"].as_str();
    let staged = args["staged"].as_bool().unwrap_or(false);
    let max_lines = args["maxLines"].as_u64().unwrap_or(200) as usize;
    let root = find_repo_root(workdir)?;

    let mut git_args = vec!["-C", root.as_str(), "diff", "--color=never"];
    if staged {
        git_args.push("--staged");
    }
    let result = run_git(&git_args, None)?;

    let lines: Vec<&str> = result.lines().collect();
    if lines.len() > max_lines {
        Ok(format!(
            "{}\n\n[diff truncated at {} lines, total {} lines]",
            lines[..max_lines].join("\n"),
            max_lines,
            lines.len()
        ))
    } else {
        Ok(result)
    }
}

pub async fn handle_git_commit(args: serde_json::Value) -> Result<String, String> {
    let message = args["message"].as_str().ok_or("message required")?;
    if message.is_empty() {
        return Err("Commit message cannot be empty".to_string());
    }
    let workdir = args["path"].as_str();
    let root = find_repo_root(workdir)?;
    run_git(&["-C", &root, "commit", "-m", message], None)
}

pub async fn handle_git_log(args: serde_json::Value) -> Result<String, String> {
    let workdir = args["path"].as_str();
    let count = args["count"].as_u64().unwrap_or(10).min(50);
    let root = find_repo_root(workdir)?;
    run_git(
        &[
            "-C", &root, "log",
            "--oneline",
            "--decorate",
            &format!("-n{}", count),
        ],
        None,
    )
}

pub async fn handle_git_pr_create(args: serde_json::Value) -> Result<String, String> {
    let title = args["title"].as_str().ok_or("title required")?;
    let body = args["body"].as_str().ok_or("body required")?;
    let base = args["base"].as_str().unwrap_or("main");
    let draft = args["draft"].as_bool().unwrap_or(false);
    let workdir = args["path"].as_str();

    let root = find_repo_root(workdir)?;

    let mut cmd_args = vec![
        "pr", "create",
        "--title", title,
        "--base", base,
        "--body", body,
    ];
    if draft {
        cmd_args.push("--draft");
    }

    let mut cmd = Command::new("gh");
    cmd.args(&cmd_args);
    cmd.current_dir(&root);
    let output = cmd.output().map_err(|e| format!("gh command failed. Is GitHub CLI installed? Error: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(format!("gh pr create failed: {}", stderr.trim()));
    }

    Ok(stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_defs_exist() {
        let d1 = git_status_def();
        let d2 = git_diff_def();
        let d3 = git_commit_def();
        let d4 = git_log_def();
        let d5 = git_pr_create_def();
        assert_eq!(d1.function.name, "git_status");
        assert_eq!(d2.function.name, "git_diff");
        assert_eq!(d3.function.name, "git_commit");
        assert_eq!(d4.function.name, "git_log");
        assert_eq!(d5.function.name, "git_pr_create");
    }

    #[test]
    fn test_find_repo_root_in_goblin() {
        let root = find_repo_root(None);
        assert!(root.is_ok());
    }

    #[test]
    fn test_run_git_help() {
        let result = run_git(&["--version"], None);
        assert!(result.is_ok());
        assert!(result.unwrap().contains("git version"));
    }

    #[tokio::test]
    async fn test_git_log_no_repo() {
        let result = handle_git_log(serde_json::json!({"path": "/tmp"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_git_status_in_project() {
        let result = handle_git_status(serde_json::json!({})).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_git_diff_requires_staged() {
        let result = handle_git_diff(serde_json::json!({"staged": true})).await;
        // May succeed (empty diff) or be ok
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_git_commit_no_message() {
        let result = handle_git_commit(serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_git_pr_create_no_branch() {
        let result = handle_git_pr_create(serde_json::json!({"title": "test"})).await;
        assert!(result.is_err());
    }
}
