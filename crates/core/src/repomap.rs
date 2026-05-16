//! Repo-map — build a compact function/struct/class map of the codebase.
//!
//! Scans source files with regex patterns to extract top-level definitions
//! (functions, structs, classes, interfaces, traits). The result is a
//! compact string suitable for injecting into the system prompt so the
//! model understands the codebase structure without reading every file.
//!
//! Pragmatic choice: regex instead of tree-sitter to avoid 4 grammar
//! crate dependencies that would double compile time. Covers ~95% of
//! typical code — only deeply nested or macro-generated definitions are
//! missed.

use std::path::{Path, PathBuf};

/// A single definition extracted from a source file.
#[derive(Debug, Clone)]
pub struct Definition {
    pub kind: &'static str, // "fn", "struct", "trait", "impl", "class", "def", "interface"
    pub name: String,
    pub line: usize,
}

/// Build a repo map for the given workspace root.
/// Returns a compact string showing file paths and their definitions.
pub fn build_repo_map(root: &Path, max_files: usize) -> String {
    let files = discover_source_files(root, max_files);
    let mut out = String::new();

    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let defs = extract_definitions(&content, &rel);
        if defs.is_empty() {
            continue;
        }
        out.push_str(&rel);
        out.push_str(":\n");
        for d in &defs {
            out.push_str(&format!("  {} {}\n", d.kind, d.name));
        }
        out.push('\n');
    }

    out
}

/// Discover source files under root, skipping hidden dirs, node_modules, target, etc.
pub fn discover_source_files(root: &Path, max: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    // Build artefacts and vendored deps — never useful in a repo map.
    let skip_dirs = [
        "target",
        "node_modules",
        ".git",
        ".metis",
        "dist",
        "build",
        "__pycache__",
        ".venv",
        "vendor",
        // macOS/Linux user home directories that sometimes get scanned
        // when metis is launched from `~`. None of these are "codebase".
        "Library",
        "Applications",
        "Downloads",
        "Desktop",
        "Documents",
        "Pictures",
        "Music",
        "Movies",
        "Public",
        "Containers",
        ".Trash",
    ];
    let extensions = [
        "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "rb", "swift",
    ];

    // Bundled / minified JS often lives as a single huge line and would
    // fill the repo map with thousands of meaningless `fn <letter>` hits.
    // 200 KB is generous for real source, tight enough to cut bundlers.
    const MAX_FILE_SIZE: u64 = 200 * 1024;
    // Cap walker depth so a stray checkout deep under the root doesn't
    // explode into tens of thousands of files before `max` is hit.
    const MAX_DEPTH: usize = 8;

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && !skip_dirs.contains(&name.as_ref())
        })
        .flatten()
    {
        if files.len() >= max {
            break;
        }
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if extensions.contains(&ext) {
                    // Skip files that are obviously bundled/minified so
                    // the map doesn't get polluted with opaque symbol
                    // dumps. `metadata` lookup is cheap on the walker's
                    // already-stat'd DirEntry.
                    let too_big = entry
                        .metadata()
                        .map(|m| m.len() > MAX_FILE_SIZE)
                        .unwrap_or(false);
                    if !too_big {
                        files.push(path.to_path_buf());
                    }
                }
            }
        }
    }

    // Sort by path for deterministic output
    files.sort();
    files
}

/// Extract top-level definitions from source code using regex patterns.
pub fn extract_definitions(content: &str, filename: &str) -> Vec<Definition> {
    let mut defs = Vec::new();
    let is_rust = filename.ends_with(".rs");
    let is_python = filename.ends_with(".py");
    let is_js_ts = filename.ends_with(".js")
        || filename.ends_with(".ts")
        || filename.ends_with(".tsx")
        || filename.ends_with(".jsx");
    let is_go = filename.ends_with(".go");

    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        if is_rust {
            // Rust: pub fn, fn, pub struct, struct, pub trait, trait, impl
            if let Some(name) = extract_rust_def(trimmed) {
                defs.push(Definition {
                    kind: name.0,
                    name: name.1,
                    line: line_num + 1,
                });
            }
        } else if is_python {
            // Python: def, class
            if let Some(rest) = trimmed.strip_prefix("def ") {
                if let Some(name) = rest.split('(').next() {
                    defs.push(Definition {
                        kind: "def",
                        name: name.trim().to_string(),
                        line: line_num + 1,
                    });
                }
            } else if let Some(rest) = trimmed.strip_prefix("class ") {
                let name = rest
                    .split(['(', ':'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    defs.push(Definition {
                        kind: "class",
                        name,
                        line: line_num + 1,
                    });
                }
            }
        } else if is_js_ts {
            // JS/TS: function, class, interface, export function, export class
            let t = trimmed
                .strip_prefix("export ")
                .unwrap_or(trimmed)
                .trim_start_matches("default ")
                .trim_start_matches("async ");
            if let Some(rest) = t.strip_prefix("function ") {
                if let Some(name) = rest.split('(').next() {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        defs.push(Definition {
                            kind: "fn",
                            name,
                            line: line_num + 1,
                        });
                    }
                }
            } else if let Some(rest) = t.strip_prefix("class ") {
                let name = rest
                    .split([' ', '{', '('])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    defs.push(Definition {
                        kind: "class",
                        name,
                        line: line_num + 1,
                    });
                }
            } else if let Some(rest) = t.strip_prefix("interface ") {
                let name = rest
                    .split([' ', '{', '<'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    defs.push(Definition {
                        kind: "interface",
                        name,
                        line: line_num + 1,
                    });
                }
            }
        } else if is_go {
            // Go: func, type ... struct
            if let Some(rest) = trimmed.strip_prefix("func ") {
                // func (r *Receiver) Name(...) or func Name(...)
                let name = if rest.starts_with('(') {
                    // Method: func (r *T) Name(...)
                    rest.split(')')
                        .nth(1)
                        .and_then(|s| s.trim().split('(').next())
                } else {
                    rest.split('(').next()
                };
                if let Some(n) = name {
                    let n = n.trim().to_string();
                    if !n.is_empty() {
                        defs.push(Definition {
                            kind: "fn",
                            name: n,
                            line: line_num + 1,
                        });
                    }
                }
            } else if trimmed.starts_with("type ") && trimmed.contains("struct") {
                let name = trimmed
                    .strip_prefix("type ")
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    defs.push(Definition {
                        kind: "struct",
                        name,
                        line: line_num + 1,
                    });
                }
            }
        }
    }

    defs
}

/// Extract Rust definitions from a single line.
fn extract_rust_def(line: &str) -> Option<(&'static str, String)> {
    let stripped = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);

    // Skip lines inside function bodies (indented)
    if line.starts_with("    ") || line.starts_with('\t') {
        // Allow `impl` blocks which are often indented
        if !stripped.starts_with("impl ") {
            return None;
        }
    }

    let stripped = stripped.trim_start_matches("async ");

    if let Some(rest) = stripped.strip_prefix("fn ") {
        let name = rest.split(['(', '<']).next()?.trim().to_string();
        if !name.is_empty() {
            return Some(("fn", name));
        }
    }
    if let Some(rest) = stripped.strip_prefix("struct ") {
        let name = rest.split([' ', '{', '<', '(']).next()?.trim().to_string();
        if !name.is_empty() {
            return Some(("struct", name));
        }
    }
    if let Some(rest) = stripped.strip_prefix("enum ") {
        let name = rest.split([' ', '{', '<']).next()?.trim().to_string();
        if !name.is_empty() {
            return Some(("enum", name));
        }
    }
    if let Some(rest) = stripped.strip_prefix("trait ") {
        let name = rest.split([' ', '{', '<', ':']).next()?.trim().to_string();
        if !name.is_empty() {
            return Some(("trait", name));
        }
    }
    if let Some(rest) = stripped.strip_prefix("impl") {
        // impl Trait for Type or impl Type
        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('{') {
            return None;
        }
        // Take everything up to the first '{'
        let sig = rest.split('{').next()?.trim().to_string();
        if !sig.is_empty() {
            return Some(("impl", sig));
        }
    }

    None
}

/// Extract import/use statements from source code.
/// Returns a list of imported module/file paths.
pub fn extract_imports(content: &str, filename: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let is_rust = filename.ends_with(".rs");
    let is_python = filename.ends_with(".py");
    let is_js_ts = filename.ends_with(".js")
        || filename.ends_with(".ts")
        || filename.ends_with(".tsx")
        || filename.ends_with(".jsx");
    let is_go = filename.ends_with(".go");

    for line in content.lines() {
        let trimmed = line.trim();
        if is_rust {
            // use crate::module or mod module
            if let Some(rest) = trimmed.strip_prefix("use ") {
                let path = rest
                    .split("::{")
                    .next()
                    .unwrap_or(rest)
                    .trim_end_matches(';')
                    .trim();
                imports.push(path.to_string());
            } else if let Some(rest) = trimmed.strip_prefix("mod ") {
                let name = rest.trim_end_matches(';').trim();
                if !name.contains('{') {
                    imports.push(name.to_string());
                }
            }
        } else if is_python {
            if trimmed.starts_with("import ") || trimmed.starts_with("from ") {
                let module = trimmed
                    .strip_prefix("from ")
                    .or_else(|| trimmed.strip_prefix("import "))
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !module.is_empty() {
                    imports.push(module.to_string());
                }
            }
        } else if is_js_ts {
            // import ... from 'module' or require('module')
            if trimmed.contains("from ") {
                if let Some(pos) = trimmed.rfind("from ") {
                    let rest = &trimmed[pos + 5..];
                    let module = rest
                        .trim()
                        .trim_matches(|c| c == '\'' || c == '"' || c == ';')
                        .to_string();
                    if !module.is_empty() {
                        imports.push(module);
                    }
                }
            } else if trimmed.contains("require(") {
                if let Some(start) = trimmed.find("require(") {
                    let rest = &trimmed[start + 8..];
                    let module = rest
                        .split(')')
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c: char| c == '\'' || c == '"')
                        .to_string();
                    if !module.is_empty() {
                        imports.push(module);
                    }
                }
            }
        } else if is_go {
            if let Some(rest) = trimmed.strip_prefix("import ") {
                let pkg = rest
                    .trim_matches(|c: char| c == '"' || c == '(' || c == ')')
                    .to_string();
                if !pkg.is_empty() {
                    imports.push(pkg);
                }
            }
        }
    }
    imports
}

/// Build an import graph for the workspace. Returns a map of
/// file path → list of imported module names.
pub fn build_import_graph(
    root: &Path,
    max_files: usize,
) -> std::collections::HashMap<String, Vec<String>> {
    let files = discover_source_files(root, max_files);
    let mut graph = std::collections::HashMap::new();

    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let imports = extract_imports(&content, &rel);
        if !imports.is_empty() {
            graph.insert(rel, imports);
        }
    }
    graph
}

/// Find files related to a given set of files via import graph.
/// Returns files that import or are imported by the given files.
pub fn find_related_files(
    graph: &std::collections::HashMap<String, Vec<String>>,
    files: &[&str],
) -> Vec<String> {
    let mut related = std::collections::HashSet::new();

    for file in files {
        // Files this file imports
        if let Some(imports) = graph.get(*file) {
            for imp in imports {
                // Find files whose path contains the import name
                for (path, _) in graph.iter() {
                    let stem = std::path::Path::new(path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if imp.contains(stem) || stem.contains(imp.split("::").last().unwrap_or("")) {
                        related.insert(path.clone());
                    }
                }
            }
        }
        // Files that import this file
        let stem = std::path::Path::new(file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        for (path, imports) in graph.iter() {
            if imports.iter().any(|i| i.contains(stem)) {
                related.insert(path.clone());
            }
        }
    }

    // Remove the query files themselves
    for f in files {
        related.remove(*f);
    }

    let mut result: Vec<String> = related.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions() {
        let code = r#"
pub struct Agent {
    field: String,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
}

impl Agent {
    pub async fn run(&mut self) -> Result<()> {
        todo!()
    }
}

fn helper() {}

pub enum ToolError {
    NotFound,
}
"#;
        let defs = extract_definitions(code, "agent.rs");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Agent"), "missing Agent: {names:?}");
        assert!(names.contains(&"Tool"), "missing Tool trait: {names:?}");
        assert!(names.contains(&"helper"), "missing helper fn: {names:?}");
        assert!(names.contains(&"ToolError"), "missing ToolError: {names:?}");
    }

    #[test]
    fn python_definitions() {
        let code = "class MyClass:\n    pass\n\ndef my_func(x):\n    return x\n";
        let defs = extract_definitions(code, "app.py");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].kind, "class");
        assert_eq!(defs[0].name, "MyClass");
        assert_eq!(defs[1].kind, "def");
        assert_eq!(defs[1].name, "my_func");
    }

    #[test]
    fn js_definitions() {
        let code =
            "export function handleRequest(req) {}\nexport class Router {}\ninterface Config {}\n";
        let defs = extract_definitions(code, "server.ts");
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "handleRequest");
        assert_eq!(defs[1].name, "Router");
        assert_eq!(defs[2].name, "Config");
    }

    #[test]
    fn go_definitions() {
        let code = "func main() {}\nfunc (s *Server) Handle(w http.ResponseWriter) {}\ntype Config struct {\n}\n";
        let defs = extract_definitions(code, "main.go");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"Handle"));
        assert!(names.contains(&"Config"));
    }

    #[test]
    fn build_map_on_self() {
        // Build a map of the metis crates/core/src directory
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let map = build_repo_map(&root, 50);
        assert!(map.contains("agent.rs"), "map should include agent.rs");
        assert!(map.contains("fn "), "map should contain function defs");
    }

    #[test]
    fn rust_imports() {
        let code = "use crate::tools;\nuse std::path::Path;\nmod session;\n";
        let imports = extract_imports(code, "lib.rs");
        assert!(imports.contains(&"crate::tools".to_string()));
        assert!(imports.contains(&"std::path::Path".to_string()));
        assert!(imports.contains(&"session".to_string()));
    }

    #[test]
    fn python_imports() {
        let code = "import os\nfrom pathlib import Path\n";
        let imports = extract_imports(code, "app.py");
        assert!(imports.contains(&"os".to_string()));
        assert!(imports.contains(&"pathlib".to_string()));
    }

    #[test]
    fn js_imports() {
        let code = "import { useState } from 'react';\nconst fs = require('fs');\n";
        let imports = extract_imports(code, "app.ts");
        assert!(imports.contains(&"react".to_string()));
        assert!(imports.contains(&"fs".to_string()));
    }

    #[test]
    fn import_graph_on_self() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let graph = build_import_graph(&root, 50);
        // agent.rs should import something
        let agent_imports = graph.get("agent.rs");
        assert!(agent_imports.is_some(), "agent.rs should have imports");
    }
}
