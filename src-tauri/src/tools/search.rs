use crate::provider::ToolDefinition;
use serde_json::json;
use std::fs;
use std::path::Path;

pub fn grep_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "grep".into(),
            description: "Searches file contents using regular expressions. Returns matching file paths and line numbers.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to cwd)"
                    },
                    "include": {
                        "type": "string",
                        "description": "File pattern filter, e.g. '*.rs' or '*.{ts,tsx}'"
                    },
                    "maxResults": {
                        "type": "integer",
                        "description": "Maximum results to return (default 50)"
                    }
                },
                "required": ["pattern"]
            }),
        },
    }
}

pub fn glob_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "glob".into(),
            description: "Finds files matching a glob pattern. Returns sorted file paths.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern, e.g. '**/*.rs' or 'src/**/*.tsx'"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to cwd)"
                    },
                    "maxResults": {
                        "type": "integer",
                        "description": "Maximum results to return (default 100)"
                    }
                },
                "required": ["pattern"]
            }),
        },
    }
}

pub async fn handle_grep(args: serde_json::Value) -> Result<String, String> {
    let pattern = args["pattern"].as_str().ok_or("pattern required")?;
    let search_path = args["path"].as_str().unwrap_or(".");
    let include = args["include"].as_str();
    let max_results = args["maxResults"].as_u64().unwrap_or(50) as usize;

    let regex = regex::Regex::new(pattern)
        .map_err(|e| format!("Invalid regex: {}", e))?;

    let mut results: Vec<String> = Vec::new();

    let include_exts: Option<Vec<&str>> = include.map(|inc| {
        inc.split(',')
            .flat_map(|s| {
                let s = s.trim();
                if s.starts_with("*.") { vec![&s[2..]] }
                else if s.starts_with("*.{") {
                    let inner = &s[3..s.len()-1];
                    inner.split(',').map(|x| x.trim()).collect()
                } else { vec![s] }
            })
            .collect()
    });

    walk_dir(Path::new(search_path), &mut |entry_path| {
        if results.len() >= max_results { return; }

        if let Some(ref exts) = include_exts {
            if let Some(ext) = entry_path.extension() {
                if !exts.iter().any(|e| ext.to_str() == Some(e)) {
                    return;
                }
            } else if !exts.contains(&"") {
                return;
            }
        }

        if let Ok(content) = fs::read_to_string(entry_path) {
            for (line_num, line) in content.lines().enumerate() {
                if results.len() >= max_results { break; }
                if regex.is_match(line) {
                    let display = entry_path.to_string_lossy();
                    let trunc_line = if line.len() > 200 {
                        let mut end = 200;
                        while end > 0 && !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &line[..end])
                    } else {
                        line.to_string()
                    };
                    results.push(format!("{}:{}: {}", display, line_num + 1, trunc_line));
                }
            }
        }
    });

    if results.is_empty() {
        Ok(format!("No matches found for '{}'", pattern))
    } else {
        Ok(results.join("\n"))
    }
}

pub async fn handle_glob(args: serde_json::Value) -> Result<String, String> {
    let pattern = args["pattern"].as_str().ok_or("pattern required")?;
    let search_path = args["path"].as_str().unwrap_or(".");
    let max_results = args["maxResults"].as_u64().unwrap_or(100) as usize;

    let pattern_full = format!("{}/{}", search_path.trim_end_matches('/'), pattern.trim_start_matches('/'));
    let paths = glob::glob(&pattern_full)
        .map_err(|e| format!("Glob pattern error: {}", e))?;

    let mut results: Vec<String> = Vec::new();
    for entry in paths {
        if results.len() >= max_results { break; }
        if let Ok(p) = entry {
            let display = p.to_string_lossy().to_string();
            if !display.contains("/.git/") && !display.contains("/node_modules/") {
                results.push(display);
            }
        }
    }

    if results.is_empty() {
        Ok(format!("No files found matching '{}'", pattern))
    } else {
        let total = results.len();
        results.push(format!("\n--- {} files total ---", total));
        Ok(results.join("\n"))
    }
}

fn walk_dir(dir: &Path, cb: &mut dyn FnMut(&Path)) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy();

            if name.contains("/.git/") || name.contains("/node_modules/") || name.contains("/target/") {
                continue;
            }

            if path.is_dir() {
                walk_dir(&path, cb);
            } else if path.is_file() {
                cb(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("goblin_search_{}_{}", pid, id));
        let _ = fs::create_dir_all(&p);
        p
    }

    fn setup_test_files() -> (PathBuf, PathBuf) {
        let dir = unique_dir();
        fs::write(dir.join("a.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        fs::write(dir.join("b.rs"), "fn helper() {\n    return 42;\n}\n").unwrap();
        fs::write(dir.join("readme.md"), "# Test Project\n\nSome markdown.\n").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/c.rs"), "fn sub_func() {}\n").unwrap();
        let cleanup_dir = dir.clone();
        (dir, cleanup_dir)
    }

    fn cleanup(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn grep_finds_match() {
        let (dir, cd) = setup_test_files();
        let result = handle_grep(json!({
            "pattern": "println",
            "path": dir.to_str().unwrap()
        })).await.unwrap();

        assert!(result.contains("a.rs"));
        assert!(result.contains("println"));
        assert!(!result.contains("b.rs"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn grep_with_include_filter() {
        let (dir, cd) = setup_test_files();
        let result = handle_grep(json!({
            "pattern": "fn",
            "path": dir.to_str().unwrap(),
            "include": "*.rs"
        })).await.unwrap();

        assert!(result.contains("a.rs") || result.contains("b.rs"));
        assert!(!result.contains("readme.md"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn grep_no_match() {
        let (dir, cd) = setup_test_files();
        let result = handle_grep(json!({
            "pattern": "nonexistent_pattern_xyz",
            "path": dir.to_str().unwrap()
        })).await.unwrap();

        assert!(result.contains("No matches"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn grep_invalid_regex() {
        let result = handle_grep(json!({
            "pattern": "[invalid",
            "path": "."
        })).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("regex"));
    }

    #[tokio::test]
    async fn grep_max_results() {
        let (dir, cd) = setup_test_files();
        let result = handle_grep(json!({
            "pattern": "fn",
            "path": dir.to_str().unwrap(),
            "maxResults": 1
        })).await.unwrap();

        let lines: Vec<&str> = result.lines().collect();
        assert!(lines.len() <= 1);

        cleanup(&cd);
    }

    #[tokio::test]
    async fn glob_finds_files() {
        let (dir, cd) = setup_test_files();
        let result = handle_glob(json!({
            "pattern": "*.rs",
            "path": dir.to_str().unwrap()
        })).await.unwrap();

        assert!(result.contains("a.rs"));
        assert!(result.contains("b.rs"));
        assert!(!result.contains("readme.md"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn glob_no_match() {
        let (dir, cd) = setup_test_files();
        let result = handle_glob(json!({
            "pattern": "*.py",
            "path": dir.to_str().unwrap()
        })).await.unwrap();

        assert!(result.contains("No files"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn glob_subdirectory() {
        let (dir, cd) = setup_test_files();
        let result = handle_glob(json!({
            "pattern": "**/*.rs",
            "path": dir.to_str().unwrap()
        })).await.unwrap();

        assert!(result.contains("c.rs"));

        cleanup(&cd);
    }

    #[tokio::test]
    async fn grep_def_check() {
        let def = grep_def();
        assert_eq!(def.function.name, "grep");
    }

    #[tokio::test]
    async fn glob_def_check() {
        let def = glob_def();
        assert_eq!(def.function.name, "glob");
    }
}
