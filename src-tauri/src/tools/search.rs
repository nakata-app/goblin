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
                        format!("{}...", &line[..200])
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
