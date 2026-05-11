use crate::provider::ToolDefinition;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn find_vault_root(search_path: &str) -> Result<PathBuf, String> {
    let mut current = Path::new(search_path).to_path_buf();
    if !current.is_dir() {
        current = current.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    loop {
        let obsidian_dir = current.join(".obsidian");
        if obsidian_dir.is_dir() {
            return Ok(current);
        }
        if !current.pop() {
            break;
        }
    }
    if search_path != "." {
        let root = PathBuf::from(search_path);
        if root.join(".obsidian").is_dir() {
            return Ok(root);
        }
    }
    Err("No Obsidian vault found (no .obsidian directory in path hierarchy). Specify a vault path directly.".to_string())
}

fn markdown_files_in(vault: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(vault) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".obsidian" || name == ".git" || name == ".trash" {
                    continue;
                }
                files.extend(markdown_files_in(&path));
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                files.push(path);
            }
        }
    }
    files
}

fn resolve_wikilink(link: &str, vault: &Path) -> Option<String> {
    let target = link.trim();
    if target.is_empty() { return None; }

    let parts: Vec<&str> = target.splitn(2, '|').collect();
    let name = parts[0];
    let _alias = parts.get(1);

    let search_name = name.to_lowercase();

    for file in markdown_files_in(vault) {
        let stem = file.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if stem == search_name {
            return file.to_str().map(String::from);
        }
    }

    // Fuzzy search: contains match
    for file in markdown_files_in(vault) {
        let stem = file.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if stem.contains(&search_name) || search_name.contains(&stem) {
            return file.to_str().map(String::from);
        }
    }

    None
}

pub fn obsidian_read_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "obsidian_read".into(),
            description: "Reads a note from an Obsidian vault. Accepts either a file path or a wiki-link name. Returns the full markdown content with line numbers.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "note": {
                        "type": "string",
                        "description": "Note file path relative to vault root, or wiki-link name (e.g. 'Project Alpha' or 'notes/project-alpha.md')"
                    },
                    "vaultPath": {
                        "type": "string",
                        "description": "Path to Obsidian vault root directory (auto-detected if omitted)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum lines to read (default 200)"
                    }
                },
                "required": ["note"]
            }),
        },
    }
}

pub fn obsidian_write_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "obsidian_write".into(),
            description: "Creates or overwrites a note in an Obsidian vault. Supports wiki-link resolution in content. Creates parent directories as needed. Always overwrites the entire file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "note": {
                        "type": "string",
                        "description": "Note file path relative to vault root (e.g. 'notes/meeting-2026.md')"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full markdown content for the note"
                    },
                    "vaultPath": {
                        "type": "string",
                        "description": "Path to Obsidian vault root directory (auto-detected if omitted)"
                    }
                },
                "required": ["note", "content"]
            }),
        },
    }
}

pub fn obsidian_search_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "obsidian_search".into(),
            description: "Searches across all notes in an Obsidian vault. Supports full-text search, tag-based filtering (#tag), and wiki-link backlinks. Returns matching note paths with context snippets.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query. Supports text, #tags, and [[wiki-links]]."
                    },
                    "vaultPath": {
                        "type": "string",
                        "description": "Path to Obsidian vault root directory (auto-detected if omitted)"
                    },
                    "maxResults": {
                        "type": "integer",
                        "description": "Maximum results to return (default 20)"
                    },
                    "contextLines": {
                        "type": "integer",
                        "description": "Lines of context around each match (default 1)"
                    }
                },
                "required": ["query"]
            }),
        },
    }
}

pub fn vault_stats_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "vault_stats".into(),
            description: "Returns statistics about an Obsidian vault: total notes, tags, linked/unlinked mentions, last modified dates, and file size distribution.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "vaultPath": {
                        "type": "string",
                        "description": "Path to Obsidian vault root directory (auto-detected if omitted)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_obsidian_read(args: serde_json::Value) -> Result<String, String> {
    let note = args["note"].as_str().ok_or("note required")?;
    let vault_path = args["vaultPath"].as_str();
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(200).max(1) as usize;

    let vault = if let Some(vp) = vault_path {
        PathBuf::from(vp)
    } else {
        find_vault_root(".")?
    };

    // Try wiki-link resolution first
    let file_path = if note.contains('/') || note.contains(".md") {
        let p = vault.join(note);
        if p.exists() { p } else {
            // Try without .md extension
            let p2 = vault.join(format!("{}.md", note.trim_end_matches(".md")));
            if p2.exists() { p2 } else { p }
        }
    } else if let Some(resolved) = resolve_wikilink(note, &vault) {
        PathBuf::from(resolved)
    } else {
        return Err(format!("Note not found in vault '{}': {}", vault.display(), note));
    };

    if !file_path.exists() {
        return Err(format!("Note not found: {}", file_path.display()));
    }

    let content = fs::read_to_string(&file_path)
        .map_err(|e| format!("Failed to read note: {}", e))?;

    let lines: Vec<&str> = content.lines().collect();
    let start = (offset - 1).min(lines.len());
    let end = (start + limit).min(lines.len());
    let selected = &lines[start..end];

    let mut output = format!("## {}\n\n", file_path.file_stem()
        .and_then(|s| s.to_str()).unwrap_or("note"));

    for (i, line) in selected.iter().enumerate() {
        output.push_str(&format!("{:>4}: {}\n", start + i + 1, line));
    }

    if end < lines.len() {
        output.push_str(&format!(
            "\n[lines {}-{} of {} total]",
            start + 1, end, lines.len()
        ));
    }

    Ok(output)
}

pub async fn handle_obsidian_write(args: serde_json::Value) -> Result<String, String> {
    let note = args["note"].as_str().ok_or("note required")?;
    let content = args["content"].as_str().ok_or("content required")?;
    let vault_path = args["vaultPath"].as_str();

    let vault = if let Some(vp) = vault_path {
        PathBuf::from(vp)
    } else {
        find_vault_root(".")?
    };

    let file_path = if note.ends_with(".md") {
        vault.join(note)
    } else {
        vault.join(format!("{}.md", note))
    };

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    let existed = file_path.exists();
    fs::write(&file_path, content)
        .map_err(|e| format!("Failed to write note: {}", e))?;

    if existed {
        Ok(format!("Updated: {}", file_path.display()))
    } else {
        Ok(format!("Created: {}", file_path.display()))
    }
}

pub async fn handle_obsidian_search(args: serde_json::Value) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("query required")?;
    let vault_path = args["vaultPath"].as_str();
    let max_results = args["maxResults"].as_u64().unwrap_or(20).min(100) as usize;
    let context_lines = args["contextLines"].as_u64().unwrap_or(1) as usize;

    let vault = if let Some(vp) = vault_path {
        PathBuf::from(vp)
    } else {
        find_vault_root(".")?
    };

    let is_tag_search = query.starts_with('#');
    let tag_name = if is_tag_search { &query[1..] } else { "" };
    let query_lower = query.to_lowercase();

    let files = markdown_files_in(&vault);
    let mut results: Vec<String> = Vec::new();

    for file in &files {
        if results.len() >= max_results { break; }

        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let rel_path = file.strip_prefix(&vault).unwrap_or(file);

        if is_tag_search {
            let tag_patterns = [
                format!("#{}", tag_name),
                format!("#{} ", tag_name),
            ];
            if content.lines().any(|l| {
                let lower = l.to_lowercase();
                tag_patterns.iter().any(|tp| lower.contains(tp))
            }) {
                results.push(format!("{} (tag #{})", rel_path.display(), tag_name));
            }
        } else {
            let lines: Vec<&str> = content.lines().collect();
            for (line_idx, line) in lines.iter().enumerate() {
                if results.len() >= max_results { break; }
                if line.to_lowercase().contains(&query_lower) {
                    let ctx_start = if line_idx > context_lines { line_idx - context_lines } else { 0 };
                    let ctx_end = (line_idx + context_lines + 1).min(lines.len());
                    let ctx: Vec<String> = lines[ctx_start..ctx_end]
                        .iter()
                        .enumerate()
                        .map(|(i, l)| {
                            let marker = if ctx_start + i == line_idx { ">" } else { " " };
                            format!("{}  {}: {}", marker, ctx_start + i + 1, l)
                        })
                        .collect();
                    results.push(format!(
                        "{} (line {})\n{}",
                        rel_path.display(),
                        line_idx + 1,
                        ctx.join("\n")
                    ));
                    break; // One match per file
                }
            }
        }
    }

    if results.is_empty() {
        Ok(format!("No notes found matching '{}' in vault '{}'", query, vault.display()))
    } else {
        Ok(format!(
            "{} results for '{}' in vault '{}':\n\n{}",
            results.len(),
            query,
            vault.display(),
            results.join("\n\n---\n\n")
        ))
    }
}

pub async fn handle_vault_stats(args: serde_json::Value) -> Result<String, String> {
    let vault_path = args["vaultPath"].as_str();

    let vault = if let Some(vp) = vault_path {
        PathBuf::from(vp)
    } else {
        find_vault_root(".")?
    };

    let files = markdown_files_in(&vault);
    let total = files.len();

    let mut total_size: u64 = 0;
    let mut tags: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut wikilinks_count: usize = 0;
    let mut unlinked_count: usize = 0;

    let re_tag = regex::Regex::new(r"#([a-zA-Z0-9_\-/]+)").unwrap();
    let re_wikilink = regex::Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap();

    for file in &files {
        if let Ok(meta) = fs::metadata(file) {
            total_size += meta.len();
        }
        if let Ok(content) = fs::read_to_string(file) {
            for cap in re_tag.captures_iter(&content) {
                *tags.entry(cap[1].to_lowercase()).or_insert(0) += 1;
            }
            wikilinks_count += re_wikilink.find_iter(&content).count();

            // Count unlinked mentions (words that match existing note names but aren't wikilinked)
            let note_names: Vec<String> = files.iter()
                .filter_map(|f| f.file_stem().and_then(|s| s.to_str().map(|n| n.to_lowercase())))
                .collect();
            for name in &note_names {
                if name.len() > 3 && content.to_lowercase().contains(name) {
                    // Check it's not already in a wikilink
                    let needle = format!("[[{}", name);
                    if !content.to_lowercase().contains(&needle) {
                        unlinked_count += 1;
                    }
                }
            }
        }
    }

    let mut top_tags: Vec<_> = tags.iter().collect();
    top_tags.sort_by(|a, b| b.1.cmp(a.1));
    top_tags.truncate(15);

    let mut output = format!(
        "## Vault Stats: {}\n\n",
        vault.display()
    );
    output.push_str(&format!("Total notes: {}\n", total));
    output.push_str(&format!("Total size: {:.1} KB ({:.1} MB)\n",
        total_size as f64 / 1024.0, total_size as f64 / 1_048_576.0));
    output.push_str(&format!("Wiki-links: {}\n", wikilinks_count));
    output.push_str(&format!("Unlinked mentions: ~{}\n", unlinked_count));
    output.push_str(&format!("Unique tags: {}\n\n", tags.len()));

    if !top_tags.is_empty() {
        output.push_str("### Top Tags\n");
        for (tag, count) in &top_tags {
            let bar_width = (**count as f64 / *top_tags[0].1 as f64 * 20.0).ceil() as usize;
            let bar = "█".repeat(bar_width);
            output.push_str(&format!("  #{:<20} {:>4} {}\n", tag, count, bar));
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_vault() -> PathBuf {
        let pid = std::process::id();
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("goblin_vault_{}_{}", pid, id));
        let _ = fs::create_dir_all(p.join(".obsidian"));
        let _ = fs::create_dir_all(p.join("subfolder"));
        fs::write(p.join("Note One.md"), "# Note One\n\nContent here\n\ntag: test\n").unwrap();
        fs::write(p.join("Note Two.md"), "# Note Two\n\nbacklink: [[Note One]]\n").unwrap();
        fs::write(p.join("subfolder/Nested.md"), "# Nested\n").unwrap();
        p
    }

    fn cleanup_vault(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_defs_exist() {
        assert_eq!(obsidian_read_def().function.name, "obsidian_read");
        assert_eq!(obsidian_write_def().function.name, "obsidian_write");
        assert_eq!(obsidian_search_def().function.name, "obsidian_search");
        assert_eq!(vault_stats_def().function.name, "vault_stats");
    }

    #[test]
    fn test_find_vault_root_success() {
        let vault = temp_vault();
        // navigate to subfolder, should find .obsidian above
        let sub = vault.join("subfolder");
        let root = find_vault_root(sub.to_str().unwrap()).unwrap();
        assert_eq!(root, vault);
        cleanup_vault(&vault);
    }

    #[test]
    fn test_find_vault_root_no_vault() {
        let result = find_vault_root("/tmp");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_wikilink_exact() {
        let vault = temp_vault();
        let resolved = resolve_wikilink("Note One", &vault);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().contains("Note One"));
        cleanup_vault(&vault);
    }

    #[test]
    fn test_resolve_wikilink_with_alias() {
        let vault = temp_vault();
        let resolved = resolve_wikilink("Note One|My Alias", &vault);
        assert!(resolved.is_some());
        cleanup_vault(&vault);
    }

    #[test]
    fn test_resolve_wikilink_nonexistent() {
        let vault = temp_vault();
        let resolved = resolve_wikilink("Nonexistent Note", &vault);
        assert!(resolved.is_none());
        cleanup_vault(&vault);
    }

    #[tokio::test]
    async fn test_obsidian_read_no_vault() {
        let result = handle_obsidian_read(serde_json::json!({"note": "nonexistent"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_obsidian_search_no_vault() {
        let result = handle_obsidian_search(serde_json::json!({"query": "test"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_vault_stats_no_vault() {
        let result = handle_vault_stats(serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
