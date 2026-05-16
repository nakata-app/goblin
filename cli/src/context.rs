use std::path::Path;
use std::process::Command;

pub fn build_project_context(cwd: &str) -> String {
    let mut parts = Vec::new();

    parts.push(format!("Working directory: {}", cwd));

    // Git branch + recent commit
    if let Ok(branch) = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
    {
        if branch.status.success() {
            let b = String::from_utf8_lossy(&branch.stdout).trim().to_string();
            parts.push(format!("Git branch: {}", b));
        }
    }

    if let Ok(log) = Command::new("git")
        .args(["log", "--oneline", "-5"])
        .current_dir(cwd)
        .output()
    {
        if log.status.success() {
            let l = String::from_utf8_lossy(&log.stdout).trim().to_string();
            if !l.is_empty() {
                parts.push(format!("Recent commits:\n{}", l));
            }
        }
    }

    // Changed files
    if let Ok(status) = Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output()
    {
        if status.status.success() {
            let s = String::from_utf8_lossy(&status.stdout).trim().to_string();
            if !s.is_empty() {
                parts.push(format!("Changed files:\n{}", s));
            }
        }
    }

    // Shallow file tree (2 levels, ignore hidden + node_modules + target)
    let tree = build_tree(Path::new(cwd), 0, 2);
    if !tree.is_empty() {
        parts.push(format!("Project structure:\n{}", tree));
    }

    parts.join("\n\n")
}

fn build_tree(path: &Path, depth: usize, max_depth: usize) -> String {
    if depth > max_depth { return String::new(); }
    let Ok(entries) = std::fs::read_dir(path) else { return String::new(); };
    let mut lines = Vec::new();
    let prefix = "  ".repeat(depth);

    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" || name == ".git" {
            continue;
        }
        if p.is_dir() {
            lines.push(format!("{}{}/", prefix, name));
            if depth < max_depth {
                let sub = build_tree(&p, depth + 1, max_depth);
                if !sub.is_empty() { lines.push(sub); }
            }
        } else {
            lines.push(format!("{}{}", prefix, name));
        }
    }
    lines.join("\n")
}
