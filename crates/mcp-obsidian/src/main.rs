//! MCP server for Obsidian vaults.
//!
//! Exposes tools for reading, writing, searching, and navigating
//! wiki-linked markdown notes. Designed to be spawned by Aegis via
//! `--mcp 'mcp-obsidian /path/to/vault'`.
//!
//! Wire protocol: JSON-RPC 2.0 over stdio (one JSON object per line).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use walkdir::WalkDir;

// ---------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------

#[derive(Deserialize)]
struct RpcRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn err(id: Value, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({ "code": code, "message": message })),
        }
    }
}

// ---------------------------------------------------------------
// MCP tool result helpers
// ---------------------------------------------------------------

fn tool_ok(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn tool_err(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

// ---------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------

fn tool_specs() -> Value {
    json!({
        "tools": [
            {
                "name": "vault_search",
                "description": "Full-text search across all notes in the vault. Returns matching file paths and lines.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Text or regex pattern to search for"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of matching files to return (default 20)"
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "vault_read",
                "description": "Read the full content of a note by its path (relative to vault root, e.g. 'folder/note.md').",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the note (e.g. 'Projects/idea.md')"
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "vault_write",
                "description": "Create or overwrite a note. Parent directories are created automatically.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path for the note (e.g. 'Journal/2026-04-12.md')"
                        },
                        "content": {
                            "type": "string",
                            "description": "Full markdown content to write"
                        }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "vault_append",
                "description": "Append text to an existing note (creates the note if it doesn't exist).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path for the note"
                        },
                        "content": {
                            "type": "string",
                            "description": "Text to append"
                        }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "vault_list",
                "description": "List notes in the vault, optionally filtered by folder prefix.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "folder": {
                            "type": "string",
                            "description": "Folder prefix to filter (e.g. 'Projects/'). Omit for all notes."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of notes to return (default 100)"
                        }
                    }
                }
            },
            {
                "name": "vault_tags",
                "description": "List all tags used in the vault, or find notes containing a specific tag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tag": {
                            "type": "string",
                            "description": "Tag to search for (e.g. '#project'). Omit to list all tags with counts."
                        }
                    }
                }
            },
            {
                "name": "vault_links",
                "description": "Get forward links (outgoing [[links]]) and backlinks (notes that link to this one) for a note.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path of the note to inspect"
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "vault_recent",
                "description": "List recently modified notes in the vault.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "count": {
                            "type": "integer",
                            "description": "Number of recent notes to return (default 20)"
                        }
                    }
                }
            }
        ]
    })
}

// ---------------------------------------------------------------
// Vault operations
// ---------------------------------------------------------------

struct Vault {
    root: PathBuf,
}

impl Vault {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve(&self, rel: &str) -> PathBuf {
        let cleaned = rel.trim_start_matches('/');
        self.root.join(cleaned)
    }

    fn rel_path(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root)
            .unwrap_or(abs)
            .to_string_lossy()
            .to_string()
    }

    /// Iterate all .md files in the vault, skipping .obsidian and hidden dirs.
    fn walk_notes(&self) -> Vec<PathBuf> {
        WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.') && name != "node_modules"
            })
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path().extension().map(|ext| ext == "md").unwrap_or(false)
            })
            .map(|e| e.into_path())
            .collect()
    }

    fn search(&self, query: &str, max: usize) -> String {
        let re = Regex::new(&format!("(?i){}", regex::escape(query)));
        let notes = self.walk_notes();
        let mut results = Vec::new();

        for path in &notes {
            if results.len() >= max {
                break;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut matches = Vec::new();
            for (i, line) in content.lines().enumerate() {
                let hit = match &re {
                    Ok(r) => r.is_match(line),
                    Err(_) => line.to_lowercase().contains(&query.to_lowercase()),
                };
                if hit {
                    matches.push(format!("  L{}: {}", i + 1, truncate(line, 120)));
                }
                if matches.len() >= 5 {
                    break;
                }
            }
            if !matches.is_empty() {
                results.push(format!("{}:\n{}", self.rel_path(path), matches.join("\n")));
            }
        }

        if results.is_empty() {
            format!("No matches for '{query}'")
        } else {
            let total = results.len();
            results.join("\n\n") + &format!("\n\n({total} file(s) matched)")
        }
    }

    fn read(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel);
        std::fs::read_to_string(&path).map_err(|e| format!("error reading {rel}: {e}"))
    }

    fn write(&self, rel: &str, content: &str) -> Result<String, String> {
        let path = self.resolve(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("error creating dirs for {rel}: {e}"))?;
        }
        std::fs::write(&path, content).map_err(|e| format!("error writing {rel}: {e}"))?;
        Ok(format!("wrote {rel} ({} bytes)", content.len()))
    }

    fn append(&self, rel: &str, content: &str) -> Result<String, String> {
        let path = self.resolve(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("error creating dirs for {rel}: {e}"))?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("error opening {rel}: {e}"))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("error appending to {rel}: {e}"))?;
        Ok(format!("appended {} bytes to {rel}", content.len()))
    }

    fn list(&self, folder: Option<&str>, max: usize) -> String {
        let notes = self.walk_notes();
        let mut paths: Vec<String> = notes
            .iter()
            .map(|p| self.rel_path(p))
            .filter(|p| match folder {
                Some(f) => p.starts_with(f),
                None => true,
            })
            .collect();
        paths.sort();
        let total = paths.len();
        paths.truncate(max);
        let list = paths.join("\n");
        if total > max {
            format!("{list}\n\n(showing {max} of {total} notes)")
        } else {
            format!("{list}\n\n({total} notes)")
        }
    }

    fn tags(&self, filter: Option<&str>) -> String {
        let tag_re = Regex::new(r"(?:^|\s)#([a-zA-Z][\w/-]*)").unwrap();
        let notes = self.walk_notes();
        let mut tag_counts: HashMap<String, Vec<String>> = HashMap::new();

        for path in &notes {
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            for cap in tag_re.captures_iter(&content) {
                let tag = format!("#{}", &cap[1]);
                tag_counts.entry(tag).or_default().push(self.rel_path(path));
            }
        }

        // Deduplicate file lists per tag.
        for files in tag_counts.values_mut() {
            files.sort();
            files.dedup();
        }

        match filter {
            Some(t) => {
                let normalized = if t.starts_with('#') {
                    t.to_string()
                } else {
                    format!("#{t}")
                };
                match tag_counts.get(&normalized) {
                    Some(files) => {
                        format!(
                            "{normalized} ({} notes):\n{}",
                            files.len(),
                            files.join("\n")
                        )
                    }
                    None => format!("tag '{normalized}' not found"),
                }
            }
            None => {
                let mut tags: Vec<_> = tag_counts
                    .iter()
                    .map(|(t, f)| (t.clone(), f.len()))
                    .collect();
                tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                if tags.is_empty() {
                    "no tags found in vault".to_string()
                } else {
                    tags.iter()
                        .map(|(t, c)| format!("{t} ({c})"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
        }
    }

    fn links(&self, rel: &str) -> Result<String, String> {
        let content = self.read(rel)?;
        let link_re = Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap();

        // Forward links from this note.
        let forward: Vec<String> = link_re
            .captures_iter(&content)
            .map(|c| c[1].to_string())
            .collect();

        // Backlinks: notes that link to this one.
        let stem = Path::new(rel)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let notes = self.walk_notes();
        let mut backlinks = Vec::new();
        for path in &notes {
            let p = self.rel_path(path);
            if p == rel {
                continue;
            }
            let Ok(c) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel_no_ext = rel.trim_end_matches(".md");
            for cap in link_re.captures_iter(&c) {
                let target = &cap[1];
                if target == stem || target == rel || target == rel_no_ext {
                    backlinks.push(p.clone());
                    break;
                }
            }
        }

        let mut out = String::new();
        out.push_str(&format!("=== {rel} ===\n\n"));
        out.push_str(&format!(
            "Forward links ({}):\n{}\n\n",
            forward.len(),
            if forward.is_empty() {
                "  (none)".to_string()
            } else {
                forward
                    .iter()
                    .map(|l| format!("  [[{l}]]"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        ));
        out.push_str(&format!(
            "Backlinks ({}):\n{}",
            backlinks.len(),
            if backlinks.is_empty() {
                "  (none)".to_string()
            } else {
                backlinks.join("\n")
            }
        ));
        Ok(out)
    }

    fn recent(&self, count: usize) -> String {
        let mut notes: Vec<(PathBuf, std::time::SystemTime)> = self
            .walk_notes()
            .into_iter()
            .filter_map(|p| {
                let meta = std::fs::metadata(&p).ok()?;
                let mtime = meta.modified().ok()?;
                Some((p, mtime))
            })
            .collect();
        notes.sort_by_key(|n| std::cmp::Reverse(n.1));
        notes.truncate(count);

        if notes.is_empty() {
            return "no notes in vault".to_string();
        }

        notes
            .iter()
            .map(|(p, t)| {
                let age = t
                    .elapsed()
                    .map(format_duration)
                    .unwrap_or_else(|_| "?".to_string());
                format!("{} ({})", self.rel_path(p), age)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ---------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------

fn handle_request(vault: &Vault, req: &RpcRequest) -> Option<RpcResponse> {
    let id = req.id.clone()?;

    let resp = match req.method.as_str() {
        "initialize" => RpcResponse::ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mcp-obsidian",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        ),
        "tools/list" => RpcResponse::ok(id, tool_specs()),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or(json!({}));
            let result = dispatch_tool(vault, name, &args);
            RpcResponse::ok(id, result)
        }
        "notifications/initialized" => return None, // no response for notifications
        _ => RpcResponse::err(id, -32601, &format!("unknown method: {}", req.method)),
    };
    Some(resp)
}

fn dispatch_tool(vault: &Vault, name: &str, args: &Value) -> Value {
    match name {
        "vault_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            tool_ok(&vault.search(query, max))
        }
        "vault_read" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match vault.read(path) {
                Ok(content) => tool_ok(&content),
                Err(e) => tool_err(&e),
            }
        }
        "vault_write" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            match vault.write(path, content) {
                Ok(msg) => tool_ok(&msg),
                Err(e) => tool_err(&e),
            }
        }
        "vault_append" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            match vault.append(path, content) {
                Ok(msg) => tool_ok(&msg),
                Err(e) => tool_err(&e),
            }
        }
        "vault_list" => {
            let folder = args.get("folder").and_then(|v| v.as_str());
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as usize;
            tool_ok(&vault.list(folder, max))
        }
        "vault_tags" => {
            let tag = args.get("tag").and_then(|v| v.as_str());
            tool_ok(&vault.tags(tag))
        }
        "vault_links" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match vault.links(path) {
                Ok(msg) => tool_ok(&msg),
                Err(e) => tool_err(&e),
            }
        }
        "vault_recent" => {
            let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            tool_ok(&vault.recent(count))
        }
        _ => tool_err(&format!("unknown tool: {name}")),
    }
}

// ---------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mcp-obsidian <vault-path>");
        std::process::exit(1);
    }

    let raw = &args[0];
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => raw.clone(),
        }
    } else {
        raw.clone()
    };
    let vault_path = PathBuf::from(&expanded);
    if !vault_path.is_dir() {
        eprintln!("error: '{}' is not a directory", vault_path.display());
        std::process::exit(1);
    }

    let vault = Vault::new(vault_path.canonicalize().unwrap_or(vault_path));

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break, // EOF or error
            Ok(_) => {}
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("mcp-obsidian: bad JSON-RPC: {e}");
                continue;
            }
        };

        if let Some(resp) = handle_request(&vault, &req) {
            let mut out = stdout.lock();
            let _ = serde_json::to_writer(&mut out, &resp);
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
}
