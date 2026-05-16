use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;

pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command. Use for running tests, git, builds, etc.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file's contents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "description": "Start line (1-based)" },
                        "limit": { "type": "integer", "description": "Max lines to read" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file (creates or overwrites).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace old_string with new_string in a file. old_string must be unique in the file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a pattern in files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "File or directory to search" },
                        "recursive": { "type": "boolean", "default": true }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "List files matching a glob pattern.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" }
                    },
                    "required": ["pattern"]
                }
            }
        }),
    ]
}

pub async fn call_tool(name: &str, args: &Value, cwd: &str) -> (bool, String) {
    match name {
        "bash" => tool_bash(args, cwd).await,
        "read_file" => tool_read_file(args, cwd),
        "write_file" => tool_write_file(args, cwd),
        "edit_file" => tool_edit_file(args, cwd),
        "grep" => tool_grep(args, cwd),
        "glob" => tool_glob(args, cwd),
        _ => (false, format!("unknown tool: {}", name)),
    }
}

async fn tool_bash(args: &Value, cwd: &str) -> (bool, String) {
    let cmd = match args["command"].as_str() {
        Some(c) => c,
        None => return (false, "bash: missing command".to_string()),
    };
    let timeout_secs = args["timeout"].as_u64().unwrap_or(30);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let success = out.status.success();
            let mut combined = String::new();
            if !stdout.is_empty() { combined.push_str(&stdout); }
            if !stderr.is_empty() {
                if !combined.is_empty() { combined.push('\n'); }
                combined.push_str("[stderr] ");
                combined.push_str(&stderr);
            }
            if combined.is_empty() { combined = "(no output)".to_string(); }
            // Trim large outputs
            if combined.len() > 8000 {
                combined = format!("{}\n...[truncated]", &combined[..8000]);
            }
            (success, combined)
        }
        Ok(Err(e)) => (false, format!("bash exec error: {}", e)),
        Err(_) => (false, format!("bash: timed out after {}s", timeout_secs)),
    }
}

fn resolve_path(path_str: &str, cwd: &str) -> std::path::PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(cwd).join(p) }
}

fn tool_read_file(args: &Value, cwd: &str) -> (bool, String) {
    let path_str = match args["path"].as_str() {
        Some(p) => p,
        None => return (false, "read_file: missing path".to_string()),
    };
    let path = resolve_path(path_str, cwd);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return (false, format!("read_file: {}: {}", path.display(), e)),
    };

    let lines: Vec<&str> = content.lines().collect();
    let offset = args["offset"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

    let slice: Vec<String> = lines.iter()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(i, l)| format!("{}\t{}", i + 1, l))
        .collect();

    (true, slice.join("\n"))
}

fn tool_write_file(args: &Value, cwd: &str) -> (bool, String) {
    let path_str = match args["path"].as_str() {
        Some(p) => p,
        None => return (false, "write_file: missing path".to_string()),
    };
    let content = args["content"].as_str().unwrap_or("");
    let path = resolve_path(path_str, cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, content) {
        Ok(_) => (true, format!("wrote {} bytes to {}", content.len(), path.display())),
        Err(e) => (false, format!("write_file: {}", e)),
    }
}

fn tool_edit_file(args: &Value, cwd: &str) -> (bool, String) {
    let path_str = match args["path"].as_str() {
        Some(p) => p,
        None => return (false, "edit_file: missing path".to_string()),
    };
    let old = match args["old_string"].as_str() {
        Some(s) => s,
        None => return (false, "edit_file: missing old_string".to_string()),
    };
    let new = args["new_string"].as_str().unwrap_or("");
    let path = resolve_path(path_str, cwd);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return (false, format!("edit_file read: {}", e)),
    };
    let count = content.matches(old).count();
    if count == 0 {
        return (false, format!("edit_file: old_string not found in {}", path.display()));
    }
    if count > 1 {
        return (false, format!("edit_file: old_string matches {} times, must be unique", count));
    }
    let updated = content.replacen(old, new, 1);
    match std::fs::write(&path, &updated) {
        Ok(_) => (true, format!("edited {}", path.display())),
        Err(e) => (false, format!("edit_file write: {}", e)),
    }
}

fn tool_grep(args: &Value, cwd: &str) -> (bool, String) {
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => return (false, "grep: missing pattern".to_string()),
    };
    let search_path = args["path"].as_str().unwrap_or(".");
    let full_path = resolve_path(search_path, cwd);
    let recursive = args["recursive"].as_bool().unwrap_or(true);

    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return (false, format!("grep: invalid pattern: {}", e)),
    };

    let mut results = Vec::new();
    search_in_path(&full_path, &re, recursive, &mut results, 0);

    if results.is_empty() {
        return (true, "no matches".to_string());
    }
    let out = results.join("\n");
    if out.len() > 8000 {
        (true, format!("{}\n...[truncated]", &out[..8000]))
    } else {
        (true, out)
    }
}

fn search_in_path(
    path: &Path,
    re: &regex::Regex,
    recursive: bool,
    results: &mut Vec<String>,
    depth: usize,
) {
    if depth > 10 || results.len() > 500 { return; }

    if path.is_file() {
        let Ok(content) = std::fs::read_to_string(path) else { return };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}: {}", path.display(), i + 1, line));
            }
        }
    } else if path.is_dir() && recursive {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "node_modules" || name == "target" { continue; }
            search_in_path(&p, re, recursive, results, depth + 1);
        }
    }
}

fn tool_glob(args: &Value, cwd: &str) -> (bool, String) {
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => return (false, "glob: missing pattern".to_string()),
    };
    let full_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        format!("{}/{}", cwd, pattern)
    };

    let mut matches = Vec::new();
    if let Ok(paths) = glob::glob(&full_pattern) {
        for entry in paths.flatten() {
            matches.push(entry.display().to_string());
            if matches.len() >= 200 { break; }
        }
    }

    if matches.is_empty() {
        (true, "no matches".to_string())
    } else {
        (true, matches.join("\n"))
    }
}
