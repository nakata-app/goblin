//! Context priming — automatically identify relevant files before the first
//! LLM turn so the model knows where to look without reading the whole repo.
//!
//! Three tiers of relevance (same logic a human reviewer uses):
//!
//!   1. **Direct match** — TF-IDF on task description vs code chunks.
//!      These are files the task is most likely talking about.
//!
//!   2. **Import neighbour** — files imported by a direct match. One level
//!      of the dependency graph: callers/callees the model will need to
//!      understand the context.
//!
//!   3. **Directory peer** — other source files in the same directory as a
//!      direct match. Useful when the relevant code is spread across a small
//!      module folder (e.g. `src/tools/`).
//!
//! The result is injected as a system message before the user's first
//! prompt so the model sees "these files are probably relevant" without
//! pre-reading them (token-efficient).

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Why a file ended up in the primed context.
#[derive(Debug, Clone)]
pub enum ContextReason {
    /// Matched the task description via TF-IDF. Score = relevance.
    DirectMatch(f64),
    /// Imported/used by a directly-matched file.
    ImportNeighbour(String),
    /// Lives in the same directory as a directly-matched file.
    DirectoryPeer(String),
}

/// One file recommended for the model's attention.
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Workspace-relative path (e.g. `src/agent.rs`).
    pub path: String,
    pub reason: ContextReason,
}

/// Identify up to `max_files` relevant files for the given task in `root`.
/// Returns an empty vec if the workspace has no source files or the task
/// is too short to produce useful search tokens.
pub fn prime_context(task: &str, root: &Path, max_files: usize) -> Vec<ContextFile> {
    if task.split_whitespace().count() < 3 {
        return Vec::new();
    }

    // Tier 1: TF-IDF search over code chunks.
    let chunks = crate::search::build_index(root, 300);
    if chunks.is_empty() {
        return Vec::new();
    }
    let top_chunks = crate::search::search(&chunks, task, 10);
    if top_chunks.is_empty() {
        return Vec::new();
    }

    // Deduplicate to file level, keep highest score per file.
    let mut direct: HashMap<String, f64> = HashMap::new();
    for r in &top_chunks {
        let e = direct.entry(r.chunk.file.clone()).or_insert(0.0);
        if r.score > *e {
            *e = r.score;
        }
    }

    let mut result: Vec<ContextFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Add direct matches (top 5 unique files).
    let mut direct_sorted: Vec<(String, f64)> = direct.into_iter().collect();
    direct_sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    direct_sorted.truncate(5);

    for (path, score) in &direct_sorted {
        result.push(ContextFile {
            path: path.clone(),
            reason: ContextReason::DirectMatch(*score),
        });
        seen.insert(path.clone());
    }

    // Tier 2: parse imports from direct-match files.
    let repo_files: HashSet<String> = chunks.iter().map(|c| c.file.clone()).collect();
    for (path, _) in &direct_sorted {
        let abs = root.join(path);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for neighbour in extract_import_neighbours(&content, path, &repo_files) {
            if !seen.contains(&neighbour) && result.len() < max_files {
                seen.insert(neighbour.clone());
                result.push(ContextFile {
                    path: neighbour,
                    reason: ContextReason::ImportNeighbour(path.clone()),
                });
            }
        }
    }

    // Tier 3: directory peers (max 2 per direct-match dir, only if room left).
    if result.len() < max_files {
        let direct_dirs: HashSet<String> = direct_sorted
            .iter()
            .filter_map(|(p, _)| {
                Path::new(p)
                    .parent()
                    .map(|d| d.to_string_lossy().to_string())
            })
            .collect();

        let mut peers_added: HashMap<String, usize> = HashMap::new();
        for file in &repo_files {
            let dir = Path::new(file)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            if direct_dirs.contains(&dir)
                && !seen.contains(file)
                && *peers_added.get(&dir).unwrap_or(&0) < 2
                && result.len() < max_files
            {
                *peers_added.entry(dir.clone()).or_default() += 1;
                seen.insert(file.clone());
                result.push(ContextFile {
                    path: file.clone(),
                    reason: ContextReason::DirectoryPeer(dir),
                });
            }
        }
    }

    result
}

/// Format the primed context as a compact system-message hint.
pub fn format_hint(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "<context-hints>\n\
         Files pre-identified as likely relevant to this task \
         (check these first before exploring the repo):\n\n",
    );

    for f in files {
        match &f.reason {
            ContextReason::DirectMatch(score) => {
                out.push_str(&format!("  {} (direct, score: {:.2})\n", f.path, score));
            }
            ContextReason::ImportNeighbour(from) => {
                out.push_str(&format!("  {} (imported by {})\n", f.path, from));
            }
            ContextReason::DirectoryPeer(dir) => {
                out.push_str(&format!("  {} (peer in {})\n", f.path, dir));
            }
        }
    }

    out.push_str("</context-hints>");
    out
}

// ---------------------------------------------------------------------------
// Import parsing — extracts module identifiers from common import styles and
// resolves them against the known file list.
// ---------------------------------------------------------------------------

fn extract_import_neighbours(
    content: &str,
    source_path: &str,
    repo_files: &HashSet<String>,
) -> Vec<String> {
    let tokens = collect_import_tokens(content);
    if tokens.is_empty() {
        return Vec::new();
    }

    let source_dir = Path::new(source_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut neighbours = Vec::new();
    for file in repo_files {
        if file == source_path {
            continue;
        }
        let stem = Path::new(file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        // A file qualifies if any import token matches its stem.
        if tokens.iter().any(|t| t == &stem) {
            // Prefer files in the same directory or an ancestor directory.
            let file_dir = Path::new(file)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let is_related = file_dir == source_dir
                || source_dir.starts_with(&file_dir)
                || file_dir.starts_with(&source_dir);
            if is_related {
                neighbours.push(file.clone());
            } else {
                // Still add non-local matches, but they'll be lower priority
                // because the outer loop already caps total results.
                neighbours.push(file.clone());
            }
        }
    }
    neighbours
}

/// Extract lowercase module/import tokens from a source file.
/// Covers Rust `use`, Python `import`/`from`, JS/TS `import`/`require`.
fn collect_import_tokens(content: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();

        // Rust: `use crate::foo::Bar;` or `use super::foo;`
        if let Some(rest) = trimmed
            .strip_prefix("use crate::")
            .or_else(|| trimmed.strip_prefix("use super::"))
            .or_else(|| trimmed.strip_prefix("use self::"))
        {
            if let Some(first) = rest.split("::").next() {
                let tok = first
                    .trim_end_matches(';')
                    .trim_end_matches('{')
                    .trim()
                    .to_lowercase();
                if !tok.is_empty() && tok != "{" {
                    tokens.insert(tok);
                }
            }
            continue;
        }

        // Python: `from foo import Bar` or `import foo`
        if let Some(rest) = trimmed.strip_prefix("from ") {
            if let Some(mod_name) = rest.split_whitespace().next() {
                let tok = mod_name
                    .trim_start_matches('.')
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if !tok.is_empty() {
                    tokens.insert(tok);
                }
            }
            continue;
        }
        // JS/TS: `import ... from './foo'` — must come BEFORE the generic
        // `import X` handler because the `continue` there would swallow it.
        if (trimmed.starts_with("import ") && trimmed.contains(" from "))
            || trimmed.contains("require(")
        {
            // Walk the line char-by-char looking for a quoted path.
            let chars: Vec<char> = trimmed.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '\'' || chars[i] == '"' {
                    let q = chars[i];
                    let start = i + 1;
                    let mut end = start;
                    while end < chars.len() && chars[end] != q {
                        end += 1;
                    }
                    if end > start && end < chars.len() {
                        let path_str: String = chars[start..end].iter().collect();
                        // Extract the final segment, strip leading dots/slashes.
                        let seg = path_str
                            .split('/')
                            .last()
                            .unwrap_or(&path_str)
                            .trim_start_matches('.');
                        // Drop any file extension (e.g. .ts, .js, .py).
                        let tok = seg
                            .split('.')
                            .next()
                            .unwrap_or(seg)
                            .to_lowercase();
                        if !tok.is_empty() && tok != "index" {
                            tokens.insert(tok);
                        }
                        break; // take first quoted string per line
                    }
                }
                i += 1;
            }
            continue;
        }

        // Python/Node: `import foo` or `import requests` (no `from`)
        if let Some(rest) = trimmed.strip_prefix("import ") {
            if let Some(mod_name) = rest.split_whitespace().next() {
                let tok = mod_name.split('.').next().unwrap_or("").to_lowercase();
                if !tok.is_empty() && !tok.starts_with('{') {
                    tokens.insert(tok);
                }
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_rust_imports() {
        let src = r#"
use crate::permission::AllowAll;
use crate::session::SessionStore;
use super::tools::ToolRegistry;
use std::sync::Arc;
"#;
        let tokens = collect_import_tokens(src);
        assert!(tokens.contains("permission"), "missing permission");
        assert!(tokens.contains("session"), "missing session");
        assert!(tokens.contains("tools"), "missing tools");
        // std is external — intentionally not captured
        assert!(!tokens.contains("std"), "external crates should be excluded");
    }

    #[test]
    fn collect_python_imports() {
        let src = r#"
from utils import helper
import os
from .models import User
import requests
"#;
        let tokens = collect_import_tokens(src);
        assert!(tokens.contains("utils"));
        assert!(tokens.contains("models"));
    }

    #[test]
    fn collect_js_imports() {
        let src = r#"
import { foo } from './helpers';
const bar = require('./config');
import type { Baz } from '../types/api';
"#;
        let tokens = collect_import_tokens(src);
        assert!(tokens.contains("helpers"));
        assert!(tokens.contains("config"));
        assert!(tokens.contains("api"));
    }

    #[test]
    fn short_task_returns_empty() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = prime_context("fix", root, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn prime_on_self_finds_relevant_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let result = prime_context("fix agent run loop tool execution", &root, 10);
        // agent.rs should be in the results
        let found = result.iter().any(|f| f.path.contains("agent"));
        assert!(found, "expected agent.rs in results, got: {:?}", result.iter().map(|f| &f.path).collect::<Vec<_>>());
    }

    #[test]
    fn format_hint_non_empty() {
        let files = vec![
            ContextFile {
                path: "src/agent.rs".into(),
                reason: ContextReason::DirectMatch(1.23),
            },
            ContextFile {
                path: "src/permission.rs".into(),
                reason: ContextReason::ImportNeighbour("src/agent.rs".into()),
            },
        ];
        let hint = format_hint(&files);
        assert!(hint.contains("agent.rs"));
        assert!(hint.contains("permission.rs"));
        assert!(hint.contains("direct"));
        assert!(hint.contains("imported by"));
    }
}
