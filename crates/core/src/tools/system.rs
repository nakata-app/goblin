//! Interactive / system-level tools — `ask_user_question`, `remote_trigger`,
//! `tool_search`, and `screenshot`.
//!
//! These tools don't fit the read/write/execute grouping of the other
//! submodules; they exist to bridge the agent loop with the user, external
//! HTTP callers, tool discovery, and the host desktop.

use std::io::{Read, Write as IoWrite};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError, ToolOutput};

// ---------------------------------------------------------------------
// ask_user_question
// ---------------------------------------------------------------------

pub struct AskUserQuestion;

#[derive(Debug, Deserialize)]
struct AskUserQuestionArgs {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

#[async_trait]
impl Tool for AskUserQuestion {
    fn name(&self) -> &str {
        "ask_user_question"
    }
    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer. Optionally \
         provide a list of options for them to choose from. Use this when \
         you need clarification, confirmation, or a decision from the user \
         before proceeding."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of choices. If provided, the user picks one."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: AskUserQuestionArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let callback = ctx.user_input.as_ref().ok_or_else(|| {
            ToolError::InvalidArgs(
                "ask_user_question is not available in non-interactive mode".to_string(),
            )
        })?;

        // Delegate ALL rendering to the callback. The REPL/TUI callback owns
        // the display (crossterm menu in REPL, ratatui modal in TUI).
        // The tool does NO direct eprint — avoids corrupting the TUI alt-screen.

        match callback(&args.question, &args.options) {
            Some(answer) => Ok(format!("User answered: {answer}\n")),
            None => Ok("User declined to answer.\n".to_string()),
        }
    }
}

// ---------------------------------------------------------------------
// ask_user — alias of ask_user_question for model-name-confusion
// ---------------------------------------------------------------------
//
// Real-world models (Llama-3.3, smaller Qwen, GPT-4o-mini) frequently
// emit `ask_user` instead of `ask_user_question` because that's the
// canonical name in their training data (Claude Code, OpenAI Assistants).
// Without this alias every such call returns "unknown tool" and the
// model burns turns retrying with permutations until LoopDetected
// kills the run. Cheap fix: register both names; same impl.

pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        AskUserQuestion.description()
    }
    fn parameters_schema(&self) -> Value {
        AskUserQuestion.parameters_schema()
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        AskUserQuestion.execute(args, ctx).await
    }
}

// ---------------------------------------------------------------------
// RemoteTrigger — webhook-style external invoke
// ---------------------------------------------------------------------

pub struct RemoteTrigger;

#[derive(Debug, Deserialize)]
struct RemoteTriggerArgs {
    /// Port to listen on (default 9119).
    #[serde(default = "default_trigger_port")]
    port: u16,
    /// Bearer token required for authentication. If omitted, a random
    /// one is generated and printed to stderr.
    #[serde(default)]
    token: Option<String>,
    /// Timeout in seconds to wait for an incoming request (default 300).
    #[serde(default = "default_trigger_timeout")]
    timeout_secs: u64,
}

fn default_trigger_port() -> u16 {
    9119
}
fn default_trigger_timeout() -> u64 {
    300
}

#[async_trait]
impl Tool for RemoteTrigger {
    fn name(&self) -> &str {
        "remote_trigger"
    }
    fn description(&self) -> &str {
        "Start a temporary HTTP endpoint that waits for a single POST \
         request, then returns the request body. Useful for webhook-style \
         external triggers — CI pipelines, GitHub webhooks, or other \
         services can POST a payload to continue the agent conversation."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "port": {
                    "type": "integer",
                    "description": "Port to listen on (default 9119)",
                    "default": 9119
                },
                "token": {
                    "type": "string",
                    "description": "Bearer token for authentication. If omitted, a random token is generated."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Seconds to wait for a request (default 300)",
                    "default": 300
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: RemoteTriggerArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let token = args.token.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("metis-{t:x}")
        });

        let listener = std::net::TcpListener::bind(("127.0.0.1", args.port))
            .map_err(|e| ToolError::Spawn(format!("could not bind port {}: {e}", args.port)))?;
        listener
            .set_nonblocking(false)
            .map_err(|e| ToolError::Spawn(e.to_string()))?;

        eprintln!(
            "[aegis] remote_trigger listening on http://127.0.0.1:{} (token: {token})",
            args.port
        );

        let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);

        // Accept connections until we get a valid POST or timeout.
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ToolError::Spawn(format!(
                    "remote_trigger timed out after {}s with no request",
                    args.timeout_secs
                )));
            }
            // Use a short accept timeout so we can check the deadline.
            listener
                .set_nonblocking(true)
                .map_err(|e| ToolError::Spawn(e.to_string()))?;

            let accept = listener.accept();
            match accept {
                Ok((mut stream, _addr)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut buf = vec![0u8; 65536];
                    let n = stream
                        .read(&mut buf)
                        .map_err(|e| ToolError::Spawn(e.to_string()))?;
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();

                    // Parse minimal HTTP: check method and auth header.
                    let first_line = raw.lines().next().unwrap_or("");
                    if !first_line.starts_with("POST ") {
                        let resp = "HTTP/1.1 405 Method Not Allowed\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes());
                        continue;
                    }
                    // Check bearer token.
                    let auth_ok = raw.lines().any(|l| {
                        let l = l.trim();
                        l.to_lowercase().starts_with("authorization:")
                            && l.contains(&format!("Bearer {token}"))
                    });
                    if !auth_ok {
                        let resp = "HTTP/1.1 401 Unauthorized\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes());
                        continue;
                    }
                    // Extract body (after \r\n\r\n).
                    let body = raw
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_default();
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nok\n";
                    let _ = stream.write_all(resp.as_bytes());
                    return Ok(format!("Received webhook trigger:\n{body}"));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    return Err(ToolError::Spawn(format!("accept failed: {e}")));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// ToolSearch — discover tools by keyword
// ---------------------------------------------------------------------

pub struct ToolSearch;

#[derive(Debug, Deserialize)]
struct ToolSearchArgs {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    5
}

#[async_trait]
impl Tool for ToolSearch {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Search available tools by keyword. Returns matching tool names \
         and descriptions. Use 'select:name1,name2' to fetch exact tools \
         by name, or plain keywords for fuzzy search."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Use 'select:name1,name2' for exact match, or keywords."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max results to return (default 5).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: ToolSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Build a list of all known tool names + descriptions from the
        // registry. We access the full registry through a special field.
        // For now, return info about all built-in tools.
        let all_tools = builtin_tool_info();

        let query = args.query.trim();
        let matches: Vec<_> = if let Some(names) = query.strip_prefix("select:") {
            // Exact match mode
            let requested: Vec<&str> = names.split(',').map(|s| s.trim()).collect();
            all_tools
                .iter()
                .filter(|(name, _)| requested.contains(&name.as_str()))
                .cloned()
                .collect()
        } else {
            // Keyword search
            let lower_query = query.to_lowercase();
            let keywords: Vec<&str> = lower_query.split_whitespace().collect();
            let mut scored: Vec<(usize, &(String, String))> = all_tools
                .iter()
                .filter_map(|entry| {
                    let haystack = format!("{} {}", entry.0, entry.1).to_lowercase();
                    let score = keywords.iter().filter(|kw| haystack.contains(*kw)).count();
                    if score > 0 {
                        Some((score, entry))
                    } else {
                        None
                    }
                })
                .collect();
            scored.sort_by_key(|e| std::cmp::Reverse(e.0));
            scored
                .into_iter()
                .take(args.max_results)
                .map(|(_, e)| e.clone())
                .collect()
        };

        if matches.is_empty() {
            return Ok(format!("No tools matched query: {query}\n"));
        }

        let mut out = String::new();
        for (name, desc) in &matches {
            out.push_str(&format!("- **{name}**: {desc}\n"));
        }
        Ok(out)
    }
}

/// Static list of all built-in tool names and descriptions for search.
fn builtin_tool_info() -> Vec<(String, String)> {
    vec![
        // File system
        ("read_file".into(), "Read text/PDF/notebook/image files".into()),
        ("grep".into(), "Search file contents with regex".into()),
        ("glob".into(), "Find files by pattern".into()),
        ("write_file".into(), "Create or overwrite a file".into()),
        ("edit_file".into(), "Exact string replacement in files".into()),
        ("multi_edit".into(), "Apply multiple surgical edits to a file atomically".into()),
        // Shell
        ("bash".into(), "Run shell commands (foreground or background)".into()),
        // Memory
        ("save_memory".into(), "Save a memory entry to disk".into()),
        ("list_memories".into(), "List all stored memories".into()),
        ("read_memory".into(), "Read a specific memory file".into()),
        ("delete_memory".into(), "Delete a memory entry".into()),
        ("semantic_memory_search".into(), "Search memory entries by semantic similarity".into()),
        // Web
        ("web_fetch".into(), "Fetch a URL and extract text content".into()),
        ("web_search".into(), "Search the web and return results with titles and snippets".into()),
        // User interaction
        ("ask_user_question".into(), "Ask the user a question with optional choices".into()),
        ("ask_user".into(), "Ask the user a freeform question".into()),
        // Task tracking
        ("create_task".into(), "Create a new task for tracking work".into()),
        ("update_task".into(), "Update task status (pending/in_progress/completed)".into()),
        ("list_tasks".into(), "List all tasks and their status".into()),
        // Plan mode
        ("enter_plan_mode".into(), "Enter read-only plan mode".into()),
        ("exit_plan_mode".into(), "Exit plan mode, allow all tools".into()),
        // Tool discovery
        ("tool_search".into(), "Search available tools by keyword".into()),
        // Notebooks
        ("notebook_edit".into(), "Edit Jupyter notebook cells (edit/insert/delete)".into()),
        // Git worktrees
        ("enter_worktree".into(), "Create a temporary git worktree for isolated work".into()),
        ("exit_worktree".into(), "Exit git worktree and return to original workspace".into()),
        // Scheduling
        ("cron_create".into(), "Create a scheduled cron entry".into()),
        ("cron_list".into(), "List all cron entries".into()),
        ("cron_delete".into(), "Delete a cron entry by id".into()),
        ("monitor".into(), "Stream a shell command's output line by line with timeout".into()),
        ("schedule_wakeup".into(), "In an autonomous /loop, schedule next wakeup after N seconds".into()),
        // Code intelligence
        ("lsp".into(), "Language Server Protocol client (goto_definition, find_references, hover, diagnostics)".into()),
        ("repo_map".into(), "Generate a repository map showing symbols and structure".into()),
        ("semantic_search".into(), "Semantic code search across the repository".into()),
        // External / MCP
        ("remote_trigger".into(), "Start a temporary HTTP webhook endpoint for external triggers".into()),
        ("mcp_authenticate".into(), "OAuth authentication helper for MCP servers".into()),
        // Agents
        ("agent".into(), "Spawn a subagent with isolated context (general-purpose, explore, plan)".into()),
        ("parallel_agents".into(), "Run multiple agents concurrently on the same question".into()),
        // Vision / security
        ("screenshot".into(), "Capture the screen or a window and return as image".into()),
        ("check_hallucination".into(), "Verify a claim against source documents".into()),
        ("scan_input".into(), "Scan user input for prompt injection attempts".into()),
    ]
}

// ---------------------------------------------------------------------
// Screenshot tool — captures the screen (macOS/Linux) and returns it
// as a multimodal image block so the model can analyze UI, errors, etc.
// ---------------------------------------------------------------------

pub struct Screenshot;

#[derive(Debug, Deserialize)]
struct ScreenshotArgs {
    /// Optional: capture a specific window by title (macOS only).
    #[serde(default)]
    window_title: Option<String>,
    /// Optional: capture a specific screen region "x,y,width,height".
    #[serde(default)]
    region: Option<String>,
    /// Optional delay in seconds before capturing (default 0).
    #[serde(default)]
    delay: Option<u32>,
}

#[async_trait]
impl Tool for Screenshot {
    fn name(&self) -> &str {
        "screenshot"
    }
    fn description(&self) -> &str {
        "Capture a screenshot of the screen (or a region/window) and return it as an image. \
         Useful for analyzing UI state, error dialogs, terminal output, etc."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "window_title": {
                    "type": "string",
                    "description": "Capture a specific window by its title (macOS only). Omit for full screen."
                },
                "region": {
                    "type": "string",
                    "description": "Capture a specific region: \"x,y,width,height\" in pixels. Omit for full screen."
                },
                "delay": {
                    "type": "integer",
                    "description": "Delay in seconds before capture (0-10). Default: 0."
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        // Fallback for non-multimodal providers: just report that the screenshot was taken.
        let _ = self.execute_multimodal(args, ctx).await?;
        Ok("[screenshot captured — use a vision-capable model to view it]".to_string())
    }
    async fn execute_multimodal(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: ScreenshotArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let delay = args.delay.unwrap_or(0).min(10);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay as u64)).await;
        }

        // Build a temp file path for the screenshot
        let tmp_dir = ctx.workspace_root.join(".metis").join("tmp");
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| ToolError::Spawn(format!("failed to create tmp dir: {e}")))?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let out_path = tmp_dir.join(format!("screenshot_{ts}.png"));

        // Validate region format upfront if provided
        if let Some(ref region) = args.region {
            let parts: Vec<&str> = region.split(',').collect();
            if parts.len() != 4 || !parts.iter().all(|p| p.trim().parse::<i32>().is_ok()) {
                return Err(ToolError::InvalidArgs(
                    "region must be 'x,y,width,height' with valid integers".to_string(),
                ));
            }
        }

        // Platform-specific capture
        let status = if cfg!(target_os = "macos") {
            let mut cmd = std::process::Command::new("screencapture");
            cmd.arg("-x"); // no sound
            if let Some(ref title) = args.window_title {
                // Sanitize title to prevent AppleScript injection
                let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
                let script = format!(
                    r#"tell application "System Events" to get id of first window of (first process whose name is "{escaped}")"#
                );
                if let Ok(output) = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output()
                {
                    let wid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !wid.is_empty() {
                        cmd.args(["-l", &wid]);
                    }
                }
            } else if let Some(ref region) = args.region {
                cmd.args(["-R", region]);
            }
            cmd.arg(out_path.to_str().unwrap_or("/tmp/metis_screenshot.png"));
            cmd.output()
        } else {
            // Linux: try gnome-screenshot, then import (ImageMagick)
            let mut cmd = std::process::Command::new("gnome-screenshot");
            cmd.args([
                "-f",
                out_path.to_str().unwrap_or("/tmp/metis_screenshot.png"),
            ]);
            match cmd.output() {
                Ok(o) if o.status.success() => Ok(o),
                _ => {
                    let mut cmd2 = std::process::Command::new("import");
                    cmd2.args(["-window", "root"]);
                    cmd2.arg(out_path.to_str().unwrap_or("/tmp/metis_screenshot.png"));
                    cmd2.output()
                }
            }
        };

        // Helper: clean up temp file on error paths
        let cleanup = || {
            let _ = std::fs::remove_file(&out_path);
        };

        match status {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                cleanup();
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(ToolError::Spawn(format!(
                    "screenshot capture failed: {stderr}"
                )));
            }
            Err(e) => {
                cleanup();
                return Err(ToolError::Spawn(format!(
                    "could not run screenshot command: {e}"
                )));
            }
        }

        // Read the PNG and return as multimodal
        if !out_path.exists() {
            return Err(ToolError::Spawn(
                "screenshot file was not created".to_string(),
            ));
        }
        let data = std::fs::read(&out_path)
            .map_err(|e| ToolError::Spawn(format!("failed to read screenshot: {e}")))?;
        // Clean up temp file
        let _ = std::fs::remove_file(&out_path);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        Ok(ToolOutput::Multimodal {
            fallback_text: format!("[screenshot captured: {} bytes]", data.len()),
            blocks: vec![aegis_api::ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: b64,
            }],
        })
    }
}
