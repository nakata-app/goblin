//! Code-intelligence tools — repo_map, semantic_search, lsp.
//!
//! `repo_map` and `semantic_search` lean on the shared
//! `crate::repomap` index; `lsp` drives an external language-server
//! process over stdio and handles the JSON-RPC framing.

use std::io::Write as IoWrite;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------
// Repo-map tool
// ---------------------------------------------------------------------

pub struct RepoMap;

#[async_trait]
impl Tool for RepoMap {
    fn name(&self) -> &str {
        "repo_map"
    }
    fn description(&self) -> &str {
        "Generate a compact map of the codebase showing all functions, \
         structs, classes, and interfaces. Use this to understand the \
         project structure without reading every file. Supports Rust, \
         Python, JavaScript/TypeScript, and Go."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "max_files": {
                    "type": "integer",
                    "description": "Maximum number of source files to scan (default: 200).",
                    "default": 200
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let max = args
            .get("max_files")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;
        let map = crate::repomap::build_repo_map(&ctx.workspace_root, max);
        if map.is_empty() {
            Ok("(no source files found)\n".to_string())
        } else {
            Ok(map)
        }
    }
}

// ---------------------------------------------------------------------
// Semantic search tool
// ---------------------------------------------------------------------

pub struct SemanticSearch;

#[async_trait]
impl Tool for SemanticSearch {
    fn name(&self) -> &str {
        "semantic_search"
    }
    fn description(&self) -> &str {
        "Search the codebase by meaning, not just exact text. Finds \
         relevant functions, structs, and code blocks using TF-IDF \
         ranking. Use when grep misses conceptually related code."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural language or keyword query (e.g. 'error handling in agent loop')"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of results to return (default: 10)",
                    "default": 10
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing query".into()))?;
        let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let chunks = crate::search::build_index(&ctx.workspace_root, 200);
        let results = crate::search::search(&chunks, query, top_k);
        Ok(crate::search::format_results(&results))
    }
}

// ---------------------------------------------------------------------
// LSP tool — minimal Language Server Protocol client
// ---------------------------------------------------------------------

pub struct Lsp;

#[derive(Debug, Deserialize)]
struct LspArgs {
    /// Command: "goto_definition", "find_references", "hover", "diagnostics".
    command: String,
    /// File path (relative to workspace).
    path: String,
    /// 1-based line number.
    line: u32,
    /// 1-based column number.
    column: u32,
    /// Language for server selection (e.g. "rust", "python", "typescript").
    #[serde(default)]
    language: Option<String>,
}

#[async_trait]
impl Tool for Lsp {
    fn name(&self) -> &str {
        "lsp"
    }
    fn description(&self) -> &str {
        "Interact with a Language Server. Commands: goto_definition, \
         find_references, hover, diagnostics. Spawns the appropriate \
         language server based on file extension or language parameter."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["goto_definition", "find_references", "hover", "diagnostics"],
                    "description": "LSP operation to perform"
                },
                "path": {
                    "type": "string",
                    "description": "File path (relative to workspace)"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number"
                },
                "column": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based column number"
                },
                "language": {
                    "type": "string",
                    "description": "Language hint (e.g. 'rust', 'python')"
                }
            },
            "required": ["command", "path", "line", "column"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: LspArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let path = ctx.resolve_path(&args.path)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang = args.language.as_deref().unwrap_or(ext);

        let server_cmd = lsp_server_for(lang).ok_or_else(|| {
            ToolError::InvalidArgs(format!("no LSP server known for language: {lang}"))
        })?;

        // Check if server binary exists
        let which = std::process::Command::new("which")
            .arg(server_cmd.0)
            .output()
            .ok();
        let available = which.map(|o| o.status.success()).unwrap_or(false);
        if !available {
            return Err(ToolError::Spawn(format!(
                "LSP server `{}` not found in PATH. Install it first.",
                server_cmd.0
            )));
        }

        // Spawn LSP server
        let root = ctx.effective_root();
        let mut child = std::process::Command::new(server_cmd.0)
            .args(server_cmd.1)
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ToolError::Spawn(format!("spawn {}: {e}", server_cmd.0)))?;

        let stdin = child.stdin.as_mut().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);

        // Convert to 0-based for LSP protocol
        let lsp_line = args.line - 1;
        let lsp_col = args.column - 1;
        let file_uri = format!("file://{}", path.display());

        // Initialize
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{}", root.display()),
            "capabilities": {},
            "initializationOptions": {}
        });
        send_lsp_request(stdin, 1, "initialize", init_params)?;
        let _init_result = read_lsp_response(&mut reader)?;

        // Send initialized notification
        send_lsp_notification(stdin, "initialized", json!({}))?;

        // Open the file
        let file_content = std::fs::read_to_string(&path).unwrap_or_default();
        let lang_id = match lang {
            "rs" | "rust" => "rust",
            "py" | "python" => "python",
            "ts" | "typescript" => "typescript",
            "js" | "javascript" => "javascript",
            "go" => "go",
            other => other,
        };
        send_lsp_notification(
            stdin,
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": file_uri,
                    "languageId": lang_id,
                    "version": 1,
                    "text": file_content
                }
            }),
        )?;

        // Give server a moment to index
        std::thread::sleep(Duration::from_millis(500));

        // Execute the requested command
        let result = match args.command.as_str() {
            "goto_definition" => {
                send_lsp_request(
                    stdin,
                    2,
                    "textDocument/definition",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": { "line": lsp_line, "character": lsp_col }
                    }),
                )?;
                let resp = read_lsp_response(&mut reader)?;
                format_location_response(&resp, &root)
            }
            "find_references" => {
                send_lsp_request(
                    stdin,
                    2,
                    "textDocument/references",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": { "line": lsp_line, "character": lsp_col },
                        "context": { "includeDeclaration": true }
                    }),
                )?;
                let resp = read_lsp_response(&mut reader)?;
                format_location_response(&resp, &root)
            }
            "hover" => {
                send_lsp_request(
                    stdin,
                    2,
                    "textDocument/hover",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": { "line": lsp_line, "character": lsp_col }
                    }),
                )?;
                let resp = read_lsp_response(&mut reader)?;
                format_hover_response(&resp)
            }
            "diagnostics" => {
                // Diagnostics come as notifications — read a few
                let mut diags = String::new();
                for _ in 0..5 {
                    if let Ok(msg) =
                        read_lsp_message_timeout(&mut reader, Duration::from_millis(200))
                    {
                        if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                            if method == "textDocument/publishDiagnostics" {
                                if let Some(params) = msg.get("params") {
                                    diags.push_str(&format_diagnostics(params, &root));
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
                if diags.is_empty() {
                    "No diagnostics reported.\n".to_string()
                } else {
                    diags
                }
            }
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "unknown LSP command: {other}"
                )));
            }
        };

        // Shutdown
        let _ = send_lsp_request(stdin, 99, "shutdown", json!(null));
        let _ = send_lsp_notification(stdin, "exit", json!(null));
        let _ = child.kill();

        Ok(result)
    }
}

/// Map language to (binary, args) for the LSP server.
pub(super) fn lsp_server_for(lang: &str) -> Option<(&'static str, &'static [&'static str])> {
    match lang {
        "rust" | "rs" => Some(("rust-analyzer", &[])),
        "python" | "py" => Some(("pyright-langserver", &["--stdio"])),
        "typescript" | "ts" | "javascript" | "js" => {
            Some(("typescript-language-server", &["--stdio"]))
        }
        "go" => Some(("gopls", &["serve"])),
        "c" | "cpp" | "c++" => Some(("clangd", &[])),
        _ => None,
    }
}

/// Send a JSON-RPC request to the LSP server.
fn send_lsp_request(
    stdin: &mut std::process::ChildStdin,
    id: u32,
    method: &str,
    params: Value,
) -> Result<(), ToolError> {
    let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let body = serde_json::to_string(&msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .and_then(|_| stdin.write_all(body.as_bytes()))
        .and_then(|_| stdin.flush())
        .map_err(|e| ToolError::Spawn(format!("lsp write: {e}")))
}

/// Send a JSON-RPC notification (no id).
fn send_lsp_notification(
    stdin: &mut std::process::ChildStdin,
    method: &str,
    params: Value,
) -> Result<(), ToolError> {
    let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
    let body = serde_json::to_string(&msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .and_then(|_| stdin.write_all(body.as_bytes()))
        .and_then(|_| stdin.flush())
        .map_err(|e| ToolError::Spawn(format!("lsp write: {e}")))
}

/// Read a single LSP JSON-RPC message from stdout.
fn read_lsp_response(
    reader: &mut std::io::BufReader<std::process::ChildStdout>,
) -> Result<Value, ToolError> {
    read_lsp_message_timeout(reader, Duration::from_secs(10))
}

fn read_lsp_message_timeout(
    reader: &mut std::io::BufReader<std::process::ChildStdout>,
    timeout: Duration,
) -> Result<Value, ToolError> {
    use std::io::BufRead;
    let deadline = std::time::Instant::now() + timeout;

    // Read headers
    let mut content_length: usize = 0;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(ToolError::Spawn("LSP response timeout".to_string()));
        }
        let mut header_line = String::new();
        reader
            .read_line(&mut header_line)
            .map_err(|e| ToolError::Spawn(format!("lsp read header: {e}")))?;
        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break; // End of headers
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
            content_length = len_str.parse().unwrap_or(0);
        }
    }

    if content_length == 0 {
        return Err(ToolError::Spawn("LSP: no Content-Length".to_string()));
    }

    // Read body
    let mut body = vec![0u8; content_length];
    std::io::Read::read_exact(reader, &mut body)
        .map_err(|e| ToolError::Spawn(format!("lsp read body: {e}")))?;

    serde_json::from_slice(&body).map_err(|e| ToolError::Spawn(format!("lsp parse: {e}")))
}

/// Format a location/definition/references response.
pub(super) fn format_location_response(resp: &Value, root: &Path) -> String {
    let result = resp.get("result");
    match result {
        Some(Value::Null) | None => "No results found.\n".to_string(),
        Some(Value::Array(arr)) if arr.is_empty() => "No results found.\n".to_string(),
        Some(Value::Array(arr)) => {
            let mut out = String::new();
            for loc in arr {
                if let Some(line) = format_single_location(loc, root) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            if out.is_empty() {
                "No results found.\n".to_string()
            } else {
                out
            }
        }
        Some(obj) => {
            // Single location (not array)
            format_single_location(obj, root).unwrap_or_else(|| "No results found.\n".to_string())
        }
    }
}

fn format_single_location(loc: &Value, root: &Path) -> Option<String> {
    let uri = loc.get("uri")?.as_str()?;
    let range = loc.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? + 1; // back to 1-based
    let col = start.get("character")?.as_u64()? + 1;

    let file_path = uri.strip_prefix("file://").unwrap_or(uri);
    let rel = Path::new(file_path)
        .strip_prefix(root)
        .unwrap_or(Path::new(file_path));

    Some(format!("{}:{}:{}", rel.display(), line, col))
}

pub(super) fn format_hover_response(resp: &Value) -> String {
    let result = match resp.get("result") {
        Some(Value::Null) | None => return "No hover information.\n".to_string(),
        Some(r) => r,
    };
    let contents = result.get("contents");
    match contents {
        Some(Value::String(s)) => format!("{s}\n"),
        Some(Value::Object(obj)) => {
            // MarkedString or MarkupContent
            if let Some(value) = obj.get("value").and_then(|v| v.as_str()) {
                format!("{value}\n")
            } else {
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(contents.unwrap()).unwrap_or_default()
                )
            }
        }
        Some(Value::Array(arr)) => {
            let mut out = String::new();
            for item in arr {
                match item {
                    Value::String(s) => {
                        out.push_str(s);
                        out.push('\n');
                    }
                    Value::Object(obj) => {
                        if let Some(v) = obj.get("value").and_then(|v| v.as_str()) {
                            out.push_str(v);
                            out.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                "No hover information.\n".to_string()
            } else {
                out
            }
        }
        _ => "No hover information.\n".to_string(),
    }
}

pub(super) fn format_diagnostics(params: &Value, root: &Path) -> String {
    let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    let file_path = uri.strip_prefix("file://").unwrap_or(uri);
    let rel = Path::new(file_path)
        .strip_prefix(root)
        .unwrap_or(Path::new(file_path));

    let diags = match params.get("diagnostics").and_then(|d| d.as_array()) {
        Some(d) => d,
        None => return String::new(),
    };

    let mut out = String::new();
    for d in diags {
        let severity = match d.get("severity").and_then(|s| s.as_u64()) {
            Some(1) => "error",
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "?",
        };
        let message = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
        let range = d.get("range");
        let line = range
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0)
            + 1;
        out.push_str(&format!(
            "{}:{}: {}: {}\n",
            rel.display(),
            line,
            severity,
            message
        ));
    }
    out
}

// ---------------------------------------------------------------------
