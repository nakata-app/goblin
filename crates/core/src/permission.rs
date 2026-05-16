//! Permission gate for tool execution.
//!
//! The agent loop runs every tool call through a [`Permission`]
//! implementation before invoking the tool itself. This keeps the
//! "should this happen?" policy separate from the "how does this
//! happen?" mechanics of the tool, so we can swap in a noninteractive
//! `--yes` gate for CI, a stricter policy for shared environments, or
//! an interactive prompt for humans — without touching the loop.
//!
//! Design choices:
//!
//! * **Stateless trait, stateful impls.** `Permission::check` takes
//!   `&self` so impls that need to remember grants (interactive prompt
//!   with "always allow") use interior mutability. This keeps the
//!   agent loop from needing a `&mut` borrow on the permission object.
//! * **Two flavours of deny.** `Deny(String)` is *soft*: the reason is
//!   fed back to the model as a tool reply so it can try another
//!   approach (e.g. an allowlist rejecting `bash` so the model picks
//!   `read_file`). `HardDeny(String)` is a direct user "no" from the
//!   interactive prompt — the agent loop ends the current turn
//!   without consulting the model again, because any continuation
//!   would be the agent working around the user's explicit refusal.
//!   Both carry a reason; without one the model would retry the same
//!   thing forever.
//! * **Arguments visible to the policy.** `check` sees the parsed
//!   JSON arguments so later policies can do path-scoped decisions
//!   ("allow `edit_file` under `src/` but not `.github/`"). v0.1 does
//!   not exercise that, but the shape is ready for it.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

#[cfg(unix)]
use libc;

use serde_json::Value;

/// The outcome of a permission check.
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    /// Run the tool.
    Allow,
    /// Refuse the tool. The reason is fed back to the model in the
    /// tool reply, so write it as something the model can act on —
    /// this variant is meant for programmatic denies (allowlists,
    /// policy rules) where "try another approach" is the desired
    /// outcome.
    Deny(String),
    /// User-initiated refusal at the interactive prompt. The agent
    /// loop ends the current turn immediately: the tool reply is
    /// still persisted (so the transcript and the next user turn
    /// remain consistent), but no follow-up model call is made.
    /// This keeps the agent from "working around" an explicit `no`.
    HardDeny(String),
}

/// The policy the agent loop consults before every tool call.
pub trait Permission: Send + Sync {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision;
}

impl Permission for Box<dyn Permission> {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        (**self).check(tool, args)
    }
}

impl Permission for Arc<dyn Permission> {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        (**self).check(tool, args)
    }
}

/// Unconditional allow. Used by the `--yes` CLI flag and smoke tests.
///
/// # Security
/// `Agent::new` uses this as its default, so any code that calls
/// `Agent::new(...)` without a subsequent `.with_permission(...)` call
/// will run every tool call without any user gating. Always call
/// `.with_permission(interactive_gate)` in production code paths.
pub struct AllowAll;

impl Permission for AllowAll {
    fn check(&self, _tool: &str, _args: &Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// Unconditional deny with a fixed reason. Useful for tests that want
/// to verify the deny path.
pub struct DenyAll(pub String);

impl Permission for DenyAll {
    fn check(&self, _tool: &str, _args: &Value) -> PermissionDecision {
        PermissionDecision::Deny(self.0.clone())
    }
}

/// `acceptEdits` permission mode: auto-approves file edits and writes
/// without prompting, but still asks before running shell commands.
///
/// This mirrors Claude Code's `acceptEdits` permission level — useful
/// when you trust the model's edits but want a gate on arbitrary
/// shell execution.
///
/// Auto-allowed without prompt:
/// - All read-only tools (read_file, grep, glob, …)
/// - File mutation tools (edit_file, write_file, multi_edit, notebook_edit)
///
/// Still prompted interactively:
/// - bash, shell, sandbox_exec, run_command, and similar
pub struct AcceptEditsPermission {
    inner: PolicyPermission,
    auto_edit: HashSet<&'static str>,
}

impl AcceptEditsPermission {
    pub fn new() -> Self {
        let mut auto_edit = HashSet::new();
        auto_edit.insert("edit_file");
        auto_edit.insert("write_file");
        auto_edit.insert("multi_edit");
        auto_edit.insert("notebook_edit");
        auto_edit.insert("create_file");
        Self {
            inner: PolicyPermission::new(),
            auto_edit,
        }
    }

    pub fn with_workspace(mut self, workspace: impl Into<std::path::PathBuf>) -> Self {
        self.inner = self.inner.with_workspace(workspace);
        self
    }

    pub fn with_preview_enabled(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_preview_enabled(enabled);
        self
    }
}

impl Default for AcceptEditsPermission {
    fn default() -> Self {
        Self::new()
    }
}

impl Permission for AcceptEditsPermission {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        if self.auto_edit.contains(tool) {
            return PermissionDecision::Allow;
        }
        self.inner.check(tool, args)
    }
}

/// The default interactive policy:
///
/// * Read-only tools (`read_file`, `grep`, `glob`) run without asking.
/// * Mutating tools (`edit_file`, `bash`) prompt the user on stdin
///   with `[y]es / [n]o / [a]lways`. `a` grants a session-wide pass
///   for that tool name so the model can work through a multi-step
///   refactor without ten prompts.
///
/// The prompt is deliberately written against generic `Read`/`Write`
/// handles so tests can drive it with in-memory buffers.
pub struct PolicyPermission {
    always_allowed: Mutex<HashSet<String>>,
    read_only: HashSet<&'static str>,
    /// Workspace root used to resolve relative paths when rendering a
    /// pre-write diff preview. `None` disables preview rendering
    /// (e.g. tests, non-REPL embeds). Paths the model passes are
    /// typically relative to the workspace, mirroring the tool layer.
    workspace_root: Option<PathBuf>,
    /// Whether to show the diff preview on destructive edits. Controlled
    /// by `edit_diff_preview` in `.metis/config.toml` (default true).
    preview_enabled: bool,
}

impl PolicyPermission {
    pub fn new() -> Self {
        let mut read_only = HashSet::new();
        read_only.insert("read_file");
        read_only.insert("grep");
        read_only.insert("glob");
        // `web_fetch` + `web_search` don't mutate the local filesystem
        // and can't run shell commands; the worst they do is exfil the
        // URL + query to Tavily / DuckDuckGo. Prompting for each one
        // turns a multi-result research turn into a wall of permission
        // prompts, and in TUI mode the prompt overlay bug meant a
        // 2-hour blocked turn for the user. Treat them like `read_file`
        // — visible in the tool log, but no interactive gate.
        read_only.insert("web_fetch");
        read_only.insert("web_search");
        Self {
            always_allowed: Mutex::new(HashSet::new()),
            read_only,
            workspace_root: None,
            preview_enabled: true,
        }
    }

    /// Enable per-edit diff previews in the interactive prompt. The
    /// workspace root is needed to resolve the relative paths that the
    /// model passes as `edit_file` / `write_file` / `multi_edit` args.
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(workspace.into());
        self
    }

    /// Explicitly toggle diff previews. Combined with `with_workspace`
    /// to respect the `edit_diff_preview = false` config flag.
    pub fn with_preview_enabled(mut self, enabled: bool) -> Self {
        self.preview_enabled = enabled;
        self
    }

    /// Render a single argument value as one or more "key: value" lines,
    /// truncated to keep the box from sprawling. Multi-line strings
    /// (e.g. an `edit_file` `new_string`) are shown with their first
    /// line and a `(+N more lines)` suffix.
    fn render_arg_lines(args: &Value, max_value_len: usize) -> Vec<String> {
        let mut out = Vec::new();
        match args {
            Value::Object(map) if !map.is_empty() => {
                for (k, v) in map {
                    let v_str = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let lines: Vec<&str> = v_str.split('\n').collect();
                    let first = lines[0];
                    let truncated = if first.chars().count() > max_value_len {
                        let cut: String = first.chars().take(max_value_len).collect();
                        format!("{cut}…")
                    } else {
                        first.to_string()
                    };
                    if lines.len() > 1 {
                        out.push(format!(
                            "{k}: {truncated}  (+{} more line{})",
                            lines.len() - 1,
                            if lines.len() == 2 { "" } else { "s" }
                        ));
                    } else {
                        out.push(format!("{k}: {truncated}"));
                    }
                }
            }
            _ => {
                let s = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
                let trimmed = if s.chars().count() > max_value_len {
                    let cut: String = s.chars().take(max_value_len).collect();
                    format!("{cut}…")
                } else {
                    s
                };
                out.push(trimmed);
            }
        }
        out
    }

    /// Public re-export of `render_header` for tests.
    #[doc(hidden)]
    pub fn preview_box(tool: &str, args: &Value, colored: bool) -> String {
        Self::render_header(tool, args, colored, None, false)
    }

    /// Renders the static header section: top border + tool name + args,
    /// plus an optional colored diff preview for destructive file tools.
    /// Only top border, no sides/bottom (lean box style).
    ///
    /// Preview is only attempted when `workspace_root` is `Some` and
    /// `preview_enabled` is `true`. Failures to read the target file
    /// (e.g. a brand-new `write_file`) fall back gracefully — the user
    /// still sees the arg lines, just without a diff.
    fn render_header(
        tool: &str,
        args: &Value,
        colored: bool,
        workspace_root: Option<&Path>,
        preview_enabled: bool,
    ) -> String {
        let (bld, dim, rst) = if colored {
            ("\x1b[1m", "\x1b[2m", "\x1b[0m")
        } else {
            ("", "", "")
        };
        let arg_lines = Self::render_arg_lines(args, 72);
        // Top border width — just long enough for the title
        let title = format!(" {tool} ");
        let bar_len = title.chars().count().clamp(40, 72);
        let right_bar = "─".repeat(bar_len.saturating_sub(title.chars().count()));
        let mut s = String::new();
        s.push('\n');
        s.push_str(&format!("╭─{bld}{title}{rst}─{right_bar}╮\n"));
        if !arg_lines.is_empty() {
            for line in &arg_lines {
                s.push_str(&format!("  {dim}{line}{rst}\n"));
            }
            s.push('\n');
        }
        if preview_enabled {
            if let Some(root) = workspace_root {
                if let Some(preview) = build_edit_preview(tool, args, root, colored) {
                    s.push_str(&preview);
                    s.push('\n');
                }
            }
        }
        s
    }

    /// Render the options list for the given focused index.
    /// Returns the rendered string and number of lines printed.
    fn render_options(focused: usize, colored: bool) -> String {
        let (cyan, dim, rst) = if colored {
            ("\x1b[36m", "\x1b[2m", "\x1b[0m")
        } else {
            ("", "", "")
        };
        let options = [("Yes", "1"), ("Yes, and don't ask again", "2"), ("No", "3")];
        let mut s = String::new();
        for (i, (label, key)) in options.iter().enumerate() {
            if i == focused {
                s.push_str(&format!("{cyan}❯ {key} {label}{rst}\n"));
            } else {
                s.push_str(&format!("{dim}  {key} {label}{rst}\n"));
            }
        }
        s.push_str(&format!("{dim}  Esc to cancel{rst}\n"));
        s
    }

    /// Internal prompt driven by any reader/writer. Used by unit tests
    /// to drive the prompt against in-memory buffers; production calls
    /// Test-only: drive prompt with in-memory buffers.
    #[cfg(test)]
    fn prompt<R: BufRead, W: Write>(
        &self,
        tool: &str,
        args: &Value,
        input: &mut R,
        output: &mut W,
    ) -> PermissionDecision {
        // Print header
        let header = Self::render_header(
            tool,
            args,
            false,
            self.workspace_root.as_deref(),
            self.preview_enabled,
        );
        let _ = output.write_all(header.as_bytes());
        // Read line-based for tests
        let mut line = String::new();
        let bytes = input.read_line(&mut line).unwrap_or(0);
        // EOF (no user input) must not map to Allow. Mirrors the production
        // fallback so tests exercise the same deny-on-EOF contract.
        if bytes == 0 {
            return PermissionDecision::HardDeny(format!(
                "no permission input available (stdin EOF) for `{tool}`"
            ));
        }
        match line.trim() {
            "1" | "" => PermissionDecision::Allow,
            "2" => {
                if let Ok(mut set) = self.always_allowed.lock() {
                    set.insert(tool.to_string());
                }
                PermissionDecision::Allow
            }
            _ => PermissionDecision::HardDeny(format!("user denied `{tool}`")),
        }
    }

    /// Interactive select: top border, ❯ pointer, arrow-key
    /// navigation, single-key shortcuts, Enter to confirm.
    fn run_interactive(
        tool: &str,
        args: &Value,
        workspace_root: Option<&Path>,
        preview_enabled: bool,
    ) -> Option<usize> {
        use io::IsTerminal;
        if !io::stdout().is_terminal() {
            return None;
        }
        let colored = std::env::var_os("NO_COLOR").is_none();
        let header = Self::render_header(tool, args, colored, workspace_root, preview_enabled);
        let mut focused: usize = 0;
        const N: usize = 3;

        // Convert newlines to CR+LF so the prompt renders correctly when
        // raw mode is already on (e.g. when metis is running in TUI mode
        // and the alt-screen is active — bare `\n` only advances the row
        // and leaves the cursor in the previous column, which shows up
        // visually as each subsequent option indented further right).
        // In cooked mode the terminal collapses any redundant `\r` that
        // gets re-expanded by ONLCR, so this is safe in both contexts.
        // Prepend `\r` to the first block so we land at column 0 even if
        // ratatui left the cursor mid-line.
        let crlf = |s: String| s.replace('\n', "\r\n");

        print!("\r{}", crlf(header));
        print!("{}", crlf(Self::render_options(focused, colored)));
        let _ = io::stdout().flush();

        // Enter raw mode
        #[cfg(unix)]
        let result = unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) != 0 {
                return None;
            }
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);

            let mut choice = None;
            loop {
                let mut buf = [0u8; 3];
                let n = libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut _, 3);
                if n <= 0 {
                    break;
                }

                let redraw = match (buf[0], n) {
                    // Escape sequence (arrow keys)
                    (0x1b, 3) if buf[1] == b'[' => {
                        match buf[2] {
                            b'A' => {
                                focused = focused.saturating_sub(1);
                                true
                            } // Up
                            b'B' => {
                                if focused < N - 1 {
                                    focused += 1;
                                }
                                true
                            } // Down
                            _ => false,
                        }
                    }
                    // Escape alone = cancel
                    (0x1b, 1) => {
                        choice = None;
                        break;
                    }
                    // Enter = confirm
                    (b'\r', _) | (b'\n', _) => {
                        choice = Some(focused);
                        break;
                    }
                    // Single-key shortcuts — digits only, no letters.
                    (b'1', _) => {
                        choice = Some(0);
                        break;
                    }
                    (b'2', _) => {
                        choice = Some(1);
                        break;
                    }
                    (b'3', _) => {
                        choice = Some(2);
                        break;
                    }
                    // Ctrl-C
                    (0x03, _) => {
                        choice = None;
                        break;
                    }
                    _ => false,
                };

                if redraw {
                    // Move cursor up N+1 lines (options + "Esc to cancel"),
                    // carriage-return to col 0, clear to end of screen, redraw.
                    // The explicit `\r` matters because when raw mode is on
                    // (e.g. TUI alt-screen) the cursor's column isn't reset
                    // by `\x1b[A`, leading to the indented-options visual
                    // glitch the user hit in TUI mode.
                    print!("\x1b[{}A\r\x1b[J", N + 1);
                    print!("{}", crlf(Self::render_options(focused, colored)));
                    let _ = io::stdout().flush();
                }
            }

            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &saved);
            // Clear the options and move to clean line (col 0 then wipe).
            print!("\x1b[{}A\r\x1b[J", N + 1);
            let _ = io::stdout().flush();
            choice
        };
        #[cfg(not(unix))]
        let result: Option<usize> = None;

        result
    }
}

impl Default for PolicyPermission {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a pre-write diff preview for a destructive file tool.
///
/// Returns `None` for tools that don't touch the filesystem, when
/// arguments don't parse cleanly, or when the preview would be empty
/// (e.g. edit_file arguments don't match the current file content).
/// Callers render the returned string straight into the permission
/// prompt so the user can see exactly what's about to be written
/// before approving. The preview is purely advisory — actual writes
/// still go through the normal tool execute path.
///
/// Supported tools:
///
/// * `edit_file { path, old_string, new_string, replace_all? }`
/// * `write_file { path, content }` — diffs against the current file
///   or the empty string for new files
/// * `multi_edit { edits: [{ path, old_string, new_string, replace_all? }, ...] }`
///   — produces a stacked diff, one hunk per edit, applied sequentially
///   against an in-memory snapshot
pub fn build_edit_preview(
    tool: &str,
    args: &Value,
    workspace_root: &Path,
    colored: bool,
) -> Option<String> {
    match tool {
        "edit_file" => build_edit_file_preview(args, workspace_root, colored),
        "write_file" => build_write_file_preview(args, workspace_root, colored),
        "multi_edit" => build_multi_edit_preview(args, workspace_root, colored),
        _ => None,
    }
}

fn resolve_preview_path(workspace_root: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace_root.join(p)
    }
}

fn build_edit_file_preview(args: &Value, workspace_root: &Path, colored: bool) -> Option<String> {
    let path_str = args.get("path")?.as_str()?;
    let old_string = args.get("old_string")?.as_str()?;
    let new_string = args.get("new_string")?.as_str()?;
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full = resolve_preview_path(workspace_root, path_str);
    let original = std::fs::read_to_string(&full).ok()?;
    if !original.contains(old_string) {
        return None;
    }
    let updated = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };
    if updated == original {
        return None;
    }
    let diff = crate::tools::unified_diff(&original, &updated, path_str);
    Some(format_diff_block(path_str, &diff, colored))
}

fn build_write_file_preview(args: &Value, workspace_root: &Path, colored: bool) -> Option<String> {
    let path_str = args.get("path")?.as_str()?;
    let content = args.get("content")?.as_str()?;
    let full = resolve_preview_path(workspace_root, path_str);
    let original = std::fs::read_to_string(&full).unwrap_or_default();
    if original == content {
        return None;
    }
    let label = if original.is_empty() {
        format!("{path_str} (new file)")
    } else {
        path_str.to_string()
    };
    let diff = crate::tools::unified_diff(&original, content, &label);
    Some(format_diff_block(&label, &diff, colored))
}

fn build_multi_edit_preview(args: &Value, workspace_root: &Path, colored: bool) -> Option<String> {
    let edits = args.get("edits")?.as_array()?;
    if edits.is_empty() {
        return None;
    }
    // Group edits by path and apply them sequentially in memory, then
    // diff each final state against the on-disk original. This mirrors
    // what MultiEdit does at execute time and keeps the preview honest
    // when the model chains several replacements on the same file.
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for edit in edits {
        let path = edit.get("path").and_then(Value::as_str)?.to_string();
        groups.entry(path).or_default().push(edit);
    }
    let mut out = String::new();
    for (path_str, edits) in groups {
        let full = resolve_preview_path(workspace_root, &path_str);
        let original = std::fs::read_to_string(&full).unwrap_or_default();
        let mut current = original.clone();
        for edit in edits {
            let old_string = edit.get("old_string")?.as_str()?;
            let new_string = edit.get("new_string")?.as_str()?;
            let replace_all = edit
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !current.contains(old_string) {
                // One stale hunk — skip it but keep rendering the rest,
                // so the user can still see what the other edits do.
                continue;
            }
            current = if replace_all {
                current.replace(old_string, new_string)
            } else {
                current.replacen(old_string, new_string, 1)
            };
        }
        if current == original {
            continue;
        }
        let diff = crate::tools::unified_diff(&original, &current, &path_str);
        out.push_str(&format_diff_block(&path_str, &diff, colored));
        out.push('\n');
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Wrap a unified-diff body in a compact "preview" block and optionally
/// colorize `+`/`-` lines (green/red) while leaving headers and context
/// untouched. Matches the palette the markdown renderer uses for inline
/// diffs.
fn format_diff_block(label: &str, diff: &str, colored: bool) -> String {
    let (dim, rst) = if colored {
        ("\x1b[2m", "\x1b[0m")
    } else {
        ("", "")
    };
    let colored_body = if colored {
        colorize_diff(diff)
    } else {
        diff.to_string()
    };
    format!("  {dim}preview · {label}{rst}\n{colored_body}")
}

fn colorize_diff(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len());
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            // File headers — keep dim so they don't steal focus from
            // the actual +/- payload below.
            out.push_str("\x1b[2m");
            out.push_str(line);
            out.push_str("\x1b[0m");
        } else if line.starts_with("@@") {
            out.push_str("\x1b[36m");
            out.push_str(line);
            out.push_str("\x1b[0m");
        } else if line.starts_with('+') {
            out.push_str("\x1b[32m");
            out.push_str(line);
            out.push_str("\x1b[0m");
        } else if line.starts_with('-') {
            out.push_str("\x1b[31m");
            out.push_str(line);
            out.push_str("\x1b[0m");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Decorator that wraps any [`Permission`] and appends a JSONL audit
/// entry for every call to `check`. One line per decision, suitable
/// for `tail -f` during a session or post-hoc compliance review.
///
/// Each entry has the shape:
///
/// ```json
/// {"tool": "edit_file", "args": {...}, "allowed": false, "reason": "user denied"}
/// ```
///
/// Failure to open or write the log file is non-fatal — the inner
/// permission's decision is still returned. We never want a logging
/// hiccup to block real tool execution.
///
/// Added in Session 23 as part of the permission-gate hardening
/// milestone.
pub struct AuditingPermission<P: Permission> {
    inner: P,
    log_path: PathBuf,
}

impl<P: Permission> AuditingPermission<P> {
    pub fn new(inner: P, log_path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            log_path: log_path.into(),
        }
    }

    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }
}

impl<P: Permission> Permission for AuditingPermission<P> {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        let decision = self.inner.check(tool, args);
        let (allowed, reason) = match &decision {
            PermissionDecision::Allow => (true, Value::Null),
            PermissionDecision::Deny(r) | PermissionDecision::HardDeny(r) => {
                (false, Value::String(r.clone()))
            }
        };
        let entry = serde_json::json!({
            "tool": tool,
            "args": args,
            "allowed": allowed,
            "reason": reason,
        });
        if let Some(parent) = self.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{entry}");
        }
        decision
    }
}

impl Permission for PolicyPermission {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        if self.read_only.contains(tool) {
            return PermissionDecision::Allow;
        }
        if let Ok(set) = self.always_allowed.lock() {
            if set.contains(tool) {
                return PermissionDecision::Allow;
            }
        }

        // Interactive TTY path: select with arrow keys.
        if let Some(idx) = Self::run_interactive(
            tool,
            args,
            self.workspace_root.as_deref(),
            self.preview_enabled,
        ) {
            return match idx {
                0 => PermissionDecision::Allow,
                1 => {
                    if let Ok(mut set) = self.always_allowed.lock() {
                        set.insert(tool.to_string());
                    }
                    PermissionDecision::Allow
                }
                _ => PermissionDecision::HardDeny(format!("user denied `{tool}`")),
            };
        }

        // Fallback: non-TTY or raw mode unavailable (piped stdin, CI, etc.)
        let header = Self::render_header(
            tool,
            args,
            false,
            self.workspace_root.as_deref(),
            self.preview_enabled,
        );
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        print!("{header}  y=Yes  a=Always  n=No\n> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let bytes = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => return PermissionDecision::Deny("could not read permission prompt".into()),
        };
        // EOF on stdin (bytes == 0) means there's no user behind the keyboard
        // to consult. Reported as a hard deny so the agent loop halts the
        // turn instead of silently approving a mutating tool. Running
        // `metis 'edit foo'` with `</dev/null` or any pipeline that closes
        // its output early used to sail straight through this branch via
        // the empty-string → Allow arm below.
        if bytes == 0 {
            return PermissionDecision::HardDeny(format!(
                "no permission input available (stdin EOF) for `{tool}`"
            ));
        }
        match line.trim() {
            "1" | "" => PermissionDecision::Allow,
            "2" => {
                if let Ok(mut set) = self.always_allowed.lock() {
                    set.insert(tool.to_string());
                }
                PermissionDecision::Allow
            }
            _ => PermissionDecision::HardDeny(format!("user denied `{tool}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allow_all_allows() {
        assert!(matches!(
            AllowAll.check("edit_file", &json!({})),
            PermissionDecision::Allow
        ));
    }

    #[test]
    fn deny_all_denies_with_reason() {
        let p = DenyAll("nope".into());
        match p.check("bash", &json!({})) {
            PermissionDecision::Deny(r) => assert_eq!(r, "nope"),
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn accept_edits_auto_allows_file_mutations() {
        let p = AcceptEditsPermission::new();
        for tool in ["edit_file", "write_file", "multi_edit", "notebook_edit"] {
            assert!(
                matches!(p.check(tool, &json!({})), PermissionDecision::Allow),
                "{tool} should be auto-allowed in acceptEdits mode"
            );
        }
    }

    #[test]
    fn accept_edits_still_gates_bash() {
        // bash is NOT in the auto-allow list — it goes through PolicyPermission.
        // With no TTY (test env), run_interactive returns None and falls through
        // to the stdin path which hits EOF → HardDeny.
        let p = AcceptEditsPermission::new();
        let result = p.check("bash", &json!({"command": "rm -rf /"}));
        // Either Deny or HardDeny is correct here — the key is it's not Allow.
        assert!(
            !matches!(result, PermissionDecision::Allow),
            "bash must not be auto-allowed in acceptEdits mode"
        );
    }

    #[test]
    fn policy_auto_allows_readonly_tools() {
        let p = PolicyPermission::new();
        for t in ["read_file", "grep", "glob"] {
            assert!(
                matches!(p.check(t, &json!({})), PermissionDecision::Allow),
                "{t}"
            );
        }
    }

    #[test]
    fn policy_prompt_yes_allows() {
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"1\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt("edit_file", &json!({"path": "a"}), &mut input, &mut output);
        assert!(matches!(d, PermissionDecision::Allow));
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("edit_file"));
        assert!(s.contains("a"));
    }

    /// Security regression: an empty stdin (EOF — e.g. `</dev/null`,
    /// a closed pipe, a scripted caller that forgets the newline)
    /// used to trim into the literal `""`, which the match arm in
    /// this very function silently mapped onto `Allow`. The fix
    /// intercepts `read_line` returning 0 bytes and issues a
    /// `HardDeny` so mutating tools can't be auto-approved without
    /// a human at the keyboard. Proven live with `metis … </dev/null`
    /// before the fix: edit_file applied with zero input; after the
    /// fix: `no permission input available (stdin EOF)`.
    #[test]
    fn policy_prompt_empty_stdin_hard_denies() {
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(Vec::<u8>::new()); // EOF from byte 0
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt(
            "edit_file",
            &json!({"path": "/tmp/victim"}),
            &mut input,
            &mut output,
        );
        match d {
            PermissionDecision::HardDeny(reason) => {
                assert!(reason.contains("EOF"), "unexpected reason: {reason}");
                assert!(
                    reason.contains("edit_file"),
                    "should name the tool: {reason}"
                );
            }
            other => panic!("expected HardDeny on EOF, got {other:?}"),
        }
    }

    #[test]
    fn policy_prompt_no_denies() {
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"3\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt(
            "bash",
            &json!({"command": "rm -rf /"}),
            &mut input,
            &mut output,
        );
        // HardDeny — the user pressed "no" at the interactive prompt,
        // which must halt the agent loop's current turn. A soft Deny
        // here would silently let the loop keep going.
        assert!(matches!(d, PermissionDecision::HardDeny(_)));
    }

    #[test]
    fn policy_prompt_always_grants_session_wide() {
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"2\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt("edit_file", &json!({}), &mut input, &mut output);
        assert!(matches!(d, PermissionDecision::Allow));
        // Second call must not prompt — it should hit the session grant.
        assert!(matches!(
            p.check("edit_file", &json!({})),
            PermissionDecision::Allow
        ));
    }

    /// Digits 1/2/3 are the only accepted keys. Letters (y/a/n) used to
    /// alias them; now they must HardDeny so the prompt never echoes or
    /// silently acts on stray alpha keystrokes.
    #[test]
    fn policy_prompt_numeric_options_only() {
        // 1 → Allow once, no session grant.
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"1\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt(
            "edit_file",
            &json!({"path": "a.rs"}),
            &mut input,
            &mut output,
        );
        assert!(matches!(d, PermissionDecision::Allow));
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("edit_file"), "missing tool name: {s}");
        assert!(p.always_allowed.lock().unwrap().is_empty());

        // 2 → Allow + session grant.
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"2\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt("edit_file", &json!({}), &mut input, &mut output);
        assert!(matches!(d, PermissionDecision::Allow));
        assert!(p.always_allowed.lock().unwrap().contains("edit_file"));

        // 3 → HardDeny.
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"3\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt(
            "bash",
            &json!({"command": "rm -rf /"}),
            &mut input,
            &mut output,
        );
        match d {
            PermissionDecision::HardDeny(r) => assert!(r.contains("bash")),
            _ => panic!("expected deny on 3"),
        }

        // Letter keys must no longer be accepted — they must deny.
        let p = PolicyPermission::new();
        let mut input = io::Cursor::new(b"y\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let d = p.prompt("edit_file", &json!({}), &mut input, &mut output);
        assert!(
            matches!(d, PermissionDecision::HardDeny(_)),
            "letter shortcuts must no longer allow"
        );
    }

    /// The compact prompt must show the tool name, arg keys, and
    /// choice shortcuts. Styled variant must include ANSI codes.
    #[test]
    fn rendered_prompt_contains_tool_and_arg_keys() {
        let args = json!({
            "path": "src/main.rs",
            "old_string": "fn old()",
            "new_string": "fn new()",
        });
        let plain = PolicyPermission::render_header("edit_file", &args, false, None, false);
        let options = PolicyPermission::render_options(0, false);
        assert!(plain.contains("edit_file"), "missing tool name: {plain}");
        for key in ["path:", "old_string:", "new_string:"] {
            assert!(plain.contains(key), "missing arg key {key}: {plain}");
        }
        assert!(
            options.contains("Yes") && options.contains("No"),
            "missing key hints: {options}"
        );
        // Unstyled form has no ANSI escapes.
        assert!(
            !plain.contains('\x1b'),
            "unstyled prompt must not emit ANSI escapes: {plain:?}"
        );

        // Styled form must include ANSI codes.
        let styled = PolicyPermission::render_header("edit_file", &args, true, None, false);
        assert!(styled.contains("\x1b["), "missing ANSI: {styled}");
    }

    // ---------------------------------------------------------------------
    // Diff preview tests (v0.8 — "trust & continuity" feature)
    // ---------------------------------------------------------------------

    fn preview_sandbox(name: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("metis-preview-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn edit_preview_shows_expected_hunks() {
        let dir = preview_sandbox("edit");
        let path = dir.join("foo.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();
        let args = json!({
            "path": "foo.rs",
            "old_string": "fn old()",
            "new_string": "fn new()",
        });
        let preview = build_edit_preview("edit_file", &args, &dir, false).unwrap();
        assert!(
            preview.contains("-fn old() {}"),
            "missing removal: {preview}"
        );
        assert!(
            preview.contains("+fn new() {}"),
            "missing addition: {preview}"
        );
        assert!(preview.contains("foo.rs"), "missing path label: {preview}");
    }

    #[test]
    fn write_preview_treats_missing_file_as_new() {
        let dir = preview_sandbox("write-new");
        let args = json!({
            "path": "brand_new.rs",
            "content": "fn hello() {}\n",
        });
        let preview = build_edit_preview("write_file", &args, &dir, false).unwrap();
        assert!(
            preview.contains("new file"),
            "should label new file: {preview}"
        );
        assert!(preview.contains("+fn hello() {}"), "missing add: {preview}");
    }

    #[test]
    fn write_preview_is_none_when_content_matches() {
        let dir = preview_sandbox("write-noop");
        let path = dir.join("same.rs");
        std::fs::write(&path, "same\n").unwrap();
        let args = json!({ "path": "same.rs", "content": "same\n" });
        assert!(build_edit_preview("write_file", &args, &dir, false).is_none());
    }

    #[test]
    fn multi_edit_preview_applies_edits_sequentially() {
        let dir = preview_sandbox("multi");
        let path = dir.join("m.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        let args = json!({
            "edits": [
                {"path": "m.rs", "old_string": "fn a()", "new_string": "fn aa()"},
                {"path": "m.rs", "old_string": "fn b()", "new_string": "fn bb()"},
            ]
        });
        let preview = build_edit_preview("multi_edit", &args, &dir, false).unwrap();
        assert!(
            preview.contains("-fn a() {}"),
            "missing first hunk: {preview}"
        );
        assert!(
            preview.contains("+fn aa() {}"),
            "missing first add: {preview}"
        );
        assert!(
            preview.contains("-fn b() {}"),
            "missing second hunk: {preview}"
        );
        assert!(
            preview.contains("+fn bb() {}"),
            "missing second add: {preview}"
        );
    }

    #[test]
    fn preview_returns_none_for_non_destructive_tools() {
        let dir = preview_sandbox("nope");
        for tool in ["read_file", "grep", "bash", "repo_map"] {
            assert!(
                build_edit_preview(tool, &json!({"path": "x"}), &dir, false).is_none(),
                "{tool} should not have a preview"
            );
        }
    }

    #[test]
    fn preview_is_colorized_when_requested() {
        let dir = preview_sandbox("color");
        let path = dir.join("c.rs");
        std::fs::write(&path, "old\n").unwrap();
        let args = json!({
            "path": "c.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let preview = build_edit_preview("edit_file", &args, &dir, true).unwrap();
        // green addition, red removal, dim file headers.
        assert!(preview.contains("\x1b[32m"), "missing green: {preview}");
        assert!(preview.contains("\x1b[31m"), "missing red: {preview}");
    }

    #[test]
    fn render_header_embeds_preview_when_enabled() {
        let dir = preview_sandbox("embed");
        let path = dir.join("e.rs");
        std::fs::write(&path, "old\n").unwrap();
        let args = json!({
            "path": "e.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let header =
            PolicyPermission::render_header("edit_file", &args, false, Some(dir.as_path()), true);
        assert!(
            header.contains("preview · e.rs"),
            "preview label missing: {header}"
        );
        assert!(header.contains("+new"), "preview diff missing: {header}");
    }

    #[test]
    fn render_header_skips_preview_when_disabled() {
        let dir = preview_sandbox("disabled");
        let path = dir.join("d.rs");
        std::fs::write(&path, "old\n").unwrap();
        let args = json!({
            "path": "d.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let header =
            PolicyPermission::render_header("edit_file", &args, false, Some(dir.as_path()), false);
        assert!(!header.contains("preview ·"), "preview leaked: {header}");
    }
}
