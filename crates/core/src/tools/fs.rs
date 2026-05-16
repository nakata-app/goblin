//! Filesystem tools — read, grep, glob, write, edit, multi_edit.
//!
//! All file ops route through `ToolContext::resolve_path` so the model
//! cannot escape the workspace via `..` or an absolute path. read_file
//! detects images/PDFs/notebooks and returns multimodal content blocks
//! where appropriate.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError, ToolOutput};

// ---------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------

pub struct ReadFile;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    /// Page range for PDF files (e.g. "1-5", "3", "10-20").
    #[serde(default)]
    pages: Option<String>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file from the workspace. Supports text files (line-numbered), \
         PDF files (text extraction, use `pages` param), and Jupyter notebooks \
         (.ipynb, renders cells with outputs). Use `offset` and `limit` for \
         large text files."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace root." },
                "offset": { "type": "integer", "description": "1-based line to start at (text files).", "minimum": 1 },
                "limit": { "type": "integer", "description": "Maximum number of lines to return (text files).", "minimum": 1 },
                "pages": { "type": "string", "description": "Page range for PDF files (e.g. '1-5', '3'). Required for large PDFs." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path)?;
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        // Dispatch by file type
        let mut out = match ext.as_str() {
            "pdf" => read_pdf(&path, args.pages.as_deref(), &args.path)?,
            "ipynb" => read_notebook(&path, &args.path)?,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => {
                read_image(&path, &args.path)?
            }
            _ => read_text_file(&path, &args.path, args.offset, args.limit)?,
        };

        // Record the file's mtime for state tracking
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(mtime) = meta.modified() {
                ctx.file_read_times
                    .lock()
                    .unwrap()
                    .insert(path.clone(), mtime);
            }
        }

        // Hard byte cap
        if out.len() > READ_FILE_MAX_BYTES {
            let original_len = out.len();
            let mut cut = READ_FILE_MAX_BYTES;
            while cut > 0 && !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
            out.push_str(&format!(
                "\n[truncated: showing first {cut} of {original_len} bytes — \
                 call read_file again with `offset`/`pages` to read further]\n"
            ));
        }
        Ok(out)
    }

    async fn execute_multimodal(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args_clone: ReadFileArgs = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args_clone.path)?;
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => {
                read_image_multimodal(&path, &args_clone.path)
            }
            "pdf" if args_clone.pages.is_none() => {
                // For PDFs without page selection, send as document block
                read_document_multimodal(&path, &args_clone.path)
            }
            _ => {
                // Fall through to text-only execute
                self.execute(args, ctx).await.map(ToolOutput::Text)
            }
        }
    }
}

/// Hard cap on the rendered output of a single `read_file` call.
/// Picked to keep one tool reply well below 10% of a 64k token
/// context window even after the chat envelope overhead. Tunable
/// here only — no CLI knob for v0.1.
pub const READ_FILE_MAX_BYTES: usize = 48_000;

/// Files larger than this trigger the stream-by-line path when a
/// `limit` is supplied, or an error when no `limit` is given.
/// large enough for any reasonable source file, small enough that
/// a stream-by-line fallback is bounded.
pub const READ_FILE_SLURP_LIMIT: u64 = 4 * 1024 * 1024;

/// Sniff length when checking for binary content at the head of a file.
const BINARY_SNIFF_BYTES: usize = 4096;

/// Decide whether a buffer looks like a binary blob: a NUL byte in the
/// first few KB is the cheap, classical heuristic (used by `grep`,
/// `git diff`, etc.).
fn looks_binary(buf: &[u8]) -> bool {
    buf.contains(&0)
}

/// Read a plain text file with line numbers.
///
/// Pre-flight protects against the "464 MB cat" pathology: if the
/// file is huge and the caller did not pass `limit`, we refuse with
/// a hint instead of slurping everything into RAM. Binary files are
/// rejected up front using a NUL-byte sniff over the first 4 KB.
fn read_text_file(
    path: &Path,
    display_path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    // 1. Stat first — file size drives the read strategy.
    let meta = std::fs::metadata(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;
    let size = meta.len();

    // 2. Binary sniff (cheap: read a single 4 KB window).
    {
        use std::io::Read as _;
        let mut head = [0u8; BINARY_SNIFF_BYTES];
        if let Ok(mut f) = std::fs::File::open(path) {
            let n = f.read(&mut head).unwrap_or(0);
            if looks_binary(&head[..n]) {
                return Err(ToolError::InvalidArgs(format!(
                    "{display_path} looks like a binary file (NUL byte in first \
                     {BINARY_SNIFF_BYTES} bytes). read_file is for text — use a \
                     dedicated viewer or pass the path to a binary-aware tool."
                )));
            }
        }
    }

    // 3. Pre-flight: refuse to slurp a huge file unless the caller
    //    asked for a bounded slice. With a `limit` we stream line by
    //    line; without one, error out with concrete guidance.
    if size > READ_FILE_SLURP_LIMIT && limit.is_none() {
        let mb = size as f64 / (1024.0 * 1024.0);
        return Err(ToolError::InvalidArgs(format!(
            "{display_path} is {mb:.1} MB — too large to read whole. Pass \
             `limit` (and optionally `offset`) to read a slice, e.g. \
             {{\"path\":\"{display_path}\",\"offset\":1,\"limit\":200}}."
        )));
    }

    let start = offset.unwrap_or(1).max(1);
    let cap = limit.unwrap_or(usize::MAX);
    let mut out = String::new();

    // 4. Bounded path: stream line-by-line so memory is O(longest line).
    if size > READ_FILE_SLURP_LIMIT {
        use std::io::{BufRead, BufReader};
        let f = std::fs::File::open(path).map_err(|source| ToolError::Io {
            path: display_path.to_string(),
            source,
        })?;
        let reader = BufReader::new(f);
        for (idx, line) in reader.lines().enumerate() {
            let lineno = idx + 1;
            if lineno < start {
                continue;
            }
            if lineno >= start.saturating_add(cap) {
                break;
            }
            let line = line.map_err(|source| ToolError::Io {
                path: display_path.to_string(),
                source,
            })?;
            out.push_str(&format!("{lineno:>6}\t{line}\n"));
        }
    } else {
        // Small file path: keep the original whole-file read.
        let contents = std::fs::read_to_string(path).map_err(|source| ToolError::Io {
            path: display_path.to_string(),
            source,
        })?;
        for (idx, line) in contents.lines().enumerate() {
            let lineno = idx + 1;
            if lineno < start {
                continue;
            }
            if lineno >= start.saturating_add(cap) {
                break;
            }
            out.push_str(&format!("{lineno:>6}\t{line}\n"));
        }
    }

    if out.is_empty() {
        out.push_str("(empty file or range)\n");
    }
    Ok(out)
}

/// Extract text from a PDF file, optionally limiting to a page range.
fn read_pdf(path: &Path, pages: Option<&str>, display_path: &str) -> Result<String, ToolError> {
    let bytes = std::fs::read(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .map_err(|e| ToolError::InvalidArgs(format!("PDF extraction failed: {e}")))?;

    // Split by form-feed (page break) or approximate by double-newlines
    let all_pages: Vec<&str> = text.split('\u{0C}').collect();
    let total_pages = all_pages.len();

    let (start, end) = match pages {
        Some(range) => parse_page_range(range, total_pages)?,
        None => {
            // Default: first 20 pages max
            if total_pages > 20 {
                return Err(ToolError::InvalidArgs(format!(
                    "PDF has {total_pages} pages — use `pages` param (e.g. \"1-5\") \
                     to read specific pages. Max 20 pages per request."
                )));
            }
            (1, total_pages)
        }
    };

    let mut out = format!("[PDF: {display_path}, {total_pages} pages, showing {start}-{end}]\n\n");
    for (i, page) in all_pages.iter().enumerate() {
        let page_num = i + 1;
        if page_num < start || page_num > end {
            continue;
        }
        out.push_str(&format!("--- Page {page_num} ---\n"));
        out.push_str(page.trim());
        out.push_str("\n\n");
    }
    Ok(out)
}

/// Parse "1-5", "3", "10-20" into (start, end) 1-based inclusive.
pub(super) fn parse_page_range(range: &str, total: usize) -> Result<(usize, usize), ToolError> {
    let range = range.trim();
    if let Some((a, b)) = range.split_once('-') {
        let start: usize = a
            .trim()
            .parse()
            .map_err(|_| ToolError::InvalidArgs(format!("invalid page range: {range}")))?;
        let end: usize = b
            .trim()
            .parse()
            .map_err(|_| ToolError::InvalidArgs(format!("invalid page range: {range}")))?;
        if start < 1 || end < start {
            return Err(ToolError::InvalidArgs(format!(
                "invalid page range: {range}"
            )));
        }
        let end = end.min(total);
        if end - start + 1 > 20 {
            return Err(ToolError::InvalidArgs(
                "max 20 pages per request".to_string(),
            ));
        }
        Ok((start, end))
    } else {
        let page: usize = range
            .parse()
            .map_err(|_| ToolError::InvalidArgs(format!("invalid page number: {range}")))?;
        if page < 1 || page > total {
            return Err(ToolError::InvalidArgs(format!(
                "page {page} out of range (1-{total})"
            )));
        }
        Ok((page, page))
    }
}

/// Read a Jupyter notebook, rendering cells with their outputs.
fn read_notebook(path: &Path, display_path: &str) -> Result<String, ToolError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;
    let nb: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| ToolError::InvalidArgs(format!("invalid notebook JSON: {e}")))?;

    let cells = nb
        .get("cells")
        .and_then(|c| c.as_array())
        .ok_or_else(|| ToolError::InvalidArgs("notebook has no cells array".to_string()))?;

    let mut out = format!("[Notebook: {display_path}, {} cells]\n\n", cells.len());
    for (i, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let source = cell_source(cell);

        out.push_str(&format!("--- Cell {} ({cell_type}) ---\n", i + 1));
        out.push_str(&source);
        if !source.ends_with('\n') {
            out.push('\n');
        }

        // Render outputs for code cells
        if cell_type == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                for output in outputs {
                    let output_type = output
                        .get("output_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match output_type {
                        "stream" => {
                            let text = join_string_or_array(output.get("text"));
                            if !text.is_empty() {
                                out.push_str(&format!("[output: stream]\n{text}"));
                                if !text.ends_with('\n') {
                                    out.push('\n');
                                }
                            }
                        }
                        "execute_result" | "display_data" => {
                            if let Some(data) = output.get("data") {
                                if let Some(text) = data.get("text/plain") {
                                    let t = join_string_or_array(Some(text));
                                    out.push_str(&format!("[output: {output_type}]\n{t}"));
                                    if !t.ends_with('\n') {
                                        out.push('\n');
                                    }
                                } else if data.get("image/png").is_some() {
                                    out.push_str("[output: image/png (binary, not shown)]\n");
                                }
                            }
                        }
                        "error" => {
                            let ename = output
                                .get("ename")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Error");
                            let evalue =
                                output.get("evalue").and_then(|v| v.as_str()).unwrap_or("");
                            out.push_str(&format!("[output: error] {ename}: {evalue}\n"));
                        }
                        _ => {}
                    }
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// Extract cell source (can be string or array of strings).
pub(super) fn cell_source(cell: &serde_json::Value) -> String {
    join_string_or_array(cell.get("source"))
}

/// Join a JSON value that's either a string or an array of strings.
fn join_string_or_array(val: Option<&serde_json::Value>) -> String {
    match val {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Read an image file — return metadata since we can't do vision inline.
fn read_image(path: &Path, display_path: &str) -> Result<String, ToolError> {
    let metadata = std::fs::metadata(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;
    let size = metadata.len();
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    Ok(format!(
        "[Image: {display_path} ({ext}, {size} bytes)]\n\
         Note: Image content cannot be displayed in text mode. \
         Use bash to inspect with image tools if needed."
    ))
}

/// Read an image file and return it as a multimodal ToolOutput with base64-encoded content block.
fn read_image_multimodal(path: &Path, display_path: &str) -> Result<ToolOutput, ToolError> {
    use base64::Engine;

    let data = std::fs::read(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;

    // Cap at 20MB to avoid memory issues
    if data.len() > 20_000_000 {
        return Ok(ToolOutput::Text(format!(
            "[Image: {display_path} ({} bytes) — too large for vision, max 20MB]",
            data.len()
        )));
    }

    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let media_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    let size = data.len();

    Ok(ToolOutput::Multimodal {
        fallback_text: format!("[Image: {display_path} ({ext}, {size} bytes)]"),
        blocks: vec![
            aegis_api::ContentBlock::Text {
                text: format!("[Image: {display_path}]"),
            },
            aegis_api::ContentBlock::Image {
                media_type: media_type.to_string(),
                data: b64,
            },
        ],
    })
}

/// Read a PDF file and return it as a document content block.
fn read_document_multimodal(path: &Path, display_path: &str) -> Result<ToolOutput, ToolError> {
    use base64::Engine;

    let data = std::fs::read(path).map_err(|source| ToolError::Io {
        path: display_path.to_string(),
        source,
    })?;

    // Cap at 32MB
    if data.len() > 32_000_000 {
        return Ok(ToolOutput::Text(format!(
            "[PDF: {display_path} ({} bytes) — too large, use `pages` parameter]",
            data.len()
        )));
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    let size = data.len();

    Ok(ToolOutput::Multimodal {
        fallback_text: format!(
            "[PDF: {display_path} ({size} bytes) — use text extraction for non-vision providers]"
        ),
        blocks: vec![
            aegis_api::ContentBlock::Text {
                text: format!("[Document: {display_path}]"),
            },
            aegis_api::ContentBlock::Document {
                media_type: "application/pdf".to_string(),
                data: b64,
            },
        ],
    })
}

// ---------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------

pub struct Grep;

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    /// File type filter (e.g. "rust", "py", "js"). Maps to common
    /// extensions so the model can say `type: "rust"` instead of
    /// `glob: "**/*.rs"`.
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    /// Output mode: "content" (matching lines, default),
    /// "files_with_matches" (file paths only), "count" (match counts).
    #[serde(default)]
    output_mode: Option<String>,
    /// Lines of context after each match.
    #[serde(default, rename = "-A")]
    after_context: Option<usize>,
    /// Lines of context before each match.
    #[serde(default, rename = "-B")]
    before_context: Option<usize>,
    /// Lines of context before and after each match.
    #[serde(default, rename = "-C")]
    context: Option<usize>,
    /// Case insensitive search.
    #[serde(default, rename = "-i")]
    case_insensitive: Option<bool>,
    /// Maximum number of results to return. Defaults to 200.
    #[serde(default)]
    head_limit: Option<usize>,
    /// Skip first N results before applying head_limit.
    #[serde(default)]
    offset: Option<usize>,
    /// Enable multiline mode where `.` matches newlines.
    #[serde(default)]
    multiline: Option<bool>,
}

/// Maps a short type name to file extensions.
fn type_to_extensions(t: &str) -> Option<&'static [&'static str]> {
    match t {
        "rust" | "rs" => Some(&["rs"]),
        "python" | "py" => Some(&["py", "pyi"]),
        "javascript" | "js" => Some(&["js", "jsx", "mjs", "cjs"]),
        "typescript" | "ts" => Some(&["ts", "tsx", "mts", "cts"]),
        "java" => Some(&["java"]),
        "go" => Some(&["go"]),
        "c" => Some(&["c", "h"]),
        "cpp" | "c++" => Some(&["cpp", "cxx", "cc", "hpp", "hxx", "h"]),
        "ruby" | "rb" => Some(&["rb"]),
        "swift" => Some(&["swift"]),
        "kotlin" | "kt" => Some(&["kt", "kts"]),
        "toml" => Some(&["toml"]),
        "yaml" | "yml" => Some(&["yaml", "yml"]),
        "json" => Some(&["json"]),
        "html" => Some(&["html", "htm"]),
        "css" => Some(&["css"]),
        "markdown" | "md" => Some(&["md", "markdown"]),
        "shell" | "sh" | "bash" => Some(&["sh", "bash", "zsh"]),
        "sql" => Some(&["sql"]),
        _ => None,
    }
}

const SEARCH_EXCLUDE_DIRS: &[&str] = &[
    // VCS / build caches
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".metis",
    ".parcel-cache",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    // Go / PHP / Ruby / Swift
    "vendor",
    "Pods",
    "DerivedData",
    ".bundle",
    // Coverage / tmp
    "coverage",
    ".nyc_output",
    ".gradle",
    // IDE
    ".idea",
    ".vscode",
    // Home-dir bloat (when workspace is `~` this is the big one)
    "Library",
    "Downloads",
    "Applications",
    ".rustup",
    ".cargo",
    ".npm",
    ".yarn",
    ".bun",
    ".nvm",
    ".pnpm-store",
    ".docker",
    ".Trash",
];

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regular expression. Supports output modes, \
         context lines, type filters, pagination, multiline, and case-insensitive search."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for." },
                "path": { "type": "string", "description": "Subdirectory or file to search. Defaults to workspace root." },
                "glob": { "type": "string", "description": "Glob pattern to filter files (e.g. `*.rs`, `**/*.tsx`)." },
                "type": { "type": "string", "description": "File type filter (e.g. `rust`, `py`, `js`, `ts`, `go`, `java`)." },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output: `content` (matching lines, default), `files_with_matches` (paths only), `count` (match counts)."
                },
                "-A": { "type": "integer", "description": "Lines of context after each match." },
                "-B": { "type": "integer", "description": "Lines of context before each match." },
                "-C": { "type": "integer", "description": "Lines of context before and after each match." },
                "-i": { "type": "boolean", "description": "Case insensitive search." },
                "head_limit": { "type": "integer", "description": "Max results to return. Default 200." },
                "offset": { "type": "integer", "description": "Skip first N results." },
                "multiline": { "type": "boolean", "description": "Enable multiline mode where `.` matches newlines." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: GrepArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Build regex with flags.
        let mut pattern = args.pattern.clone();
        if args.multiline.unwrap_or(false) {
            // Wrap in (?s) so `.` matches `\n`.
            pattern = format!("(?s){pattern}");
        }
        if args.case_insensitive.unwrap_or(false) {
            pattern = format!("(?i){pattern}");
        }
        let regex = regex::Regex::new(&pattern)?;

        let root = match &args.path {
            Some(p) => ctx.resolve_path(p)?,
            None => ctx.effective_root(),
        };
        let glob_matcher = args
            .glob
            .as_ref()
            .map(|g| glob::Pattern::new(g))
            .transpose()?;
        let type_exts = args.file_type.as_deref().and_then(type_to_extensions);

        let mode = args.output_mode.as_deref().unwrap_or("content");
        let head_limit = args.head_limit.unwrap_or(200);
        let offset = args.offset.unwrap_or(0);
        let ctx_before = args.before_context.or(args.context).unwrap_or(0);
        let ctx_after = args.after_context.or(args.context).unwrap_or(0);

        let mut out = String::new();
        let mut emitted = 0usize;
        let mut skipped = 0usize;

        let walker = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !SEARCH_EXCLUDE_DIRS.contains(&name.as_ref())
            });

        for entry in walker.flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            // Type filter: check extension.
            if let Some(exts) = type_exts {
                let ext = entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if !exts.contains(&ext) {
                    continue;
                }
            }
            // Glob filter.
            if let Some(g) = &glob_matcher {
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                if !g.matches_path(rel) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };

            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            let rel_display = rel.display().to_string();

            if args.multiline.unwrap_or(false) {
                // Multiline: search the entire file content at once.
                let has_match = regex.is_match(&text);
                if !has_match {
                    continue;
                }
                match mode {
                    "files_with_matches" => {
                        if skipped < offset {
                            skipped += 1;
                            continue;
                        }
                        out.push_str(&rel_display);
                        out.push('\n');
                        emitted += 1;
                    }
                    "count" => {
                        let count = regex.find_iter(&text).count();
                        if skipped < offset {
                            skipped += 1;
                            continue;
                        }
                        out.push_str(&format!("{rel_display}:{count}\n"));
                        emitted += 1;
                    }
                    _ => {
                        // Show each match with some surrounding context.
                        for mat in regex.find_iter(&text) {
                            if skipped < offset {
                                skipped += 1;
                                continue;
                            }
                            let start = mat.start().saturating_sub(100);
                            let end = (mat.end() + 100).min(text.len());
                            // Find safe char boundaries.
                            let start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(start);
                            let end = text[end..].find('\n').map(|i| end + i).unwrap_or(end);
                            out.push_str(&format!("{rel_display}: {}\n", &text[start..end]));
                            emitted += 1;
                            if emitted >= head_limit {
                                out.push_str(&format!("(truncated at {head_limit} matches)\n"));
                                return Ok(out);
                            }
                        }
                    }
                }
                if emitted >= head_limit {
                    out.push_str(&format!("(truncated at {head_limit})\n"));
                    return Ok(out);
                }
                continue;
            }

            // Line-by-line matching (default, non-multiline).
            let lines: Vec<&str> = text.lines().collect();
            let mut file_match_count = 0usize;
            let mut file_has_match = false;

            for (idx, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                file_has_match = true;
                file_match_count += 1;

                if mode == "files_with_matches" || mode == "count" {
                    continue; // Just counting / detecting.
                }

                // Content mode: emit with optional context.
                if skipped < offset {
                    skipped += 1;
                    continue;
                }

                // Before context.
                let b_start = idx.saturating_sub(ctx_before);
                if ctx_before > 0 && b_start < idx {
                    if emitted > 0 {
                        out.push_str("--\n");
                    }
                    for (bi, bline) in lines.iter().enumerate().take(idx).skip(b_start) {
                        out.push_str(&format!("{rel_display}-{}:{}\n", bi + 1, bline));
                    }
                }
                // The matching line itself.
                out.push_str(&format!("{rel_display}:{}:{}\n", idx + 1, line));
                emitted += 1;
                if emitted >= head_limit {
                    out.push_str(&format!("(truncated at {head_limit} matches)\n"));
                    return Ok(out);
                }
                // After context.
                if ctx_after > 0 {
                    let a_end = (idx + 1 + ctx_after).min(lines.len());
                    for (ai, aline) in lines.iter().enumerate().take(a_end).skip(idx + 1) {
                        out.push_str(&format!("{rel_display}-{}:{}\n", ai + 1, aline));
                    }
                }
            }

            if file_has_match {
                match mode {
                    "files_with_matches" => {
                        if skipped < offset {
                            skipped += 1;
                            continue;
                        }
                        out.push_str(&rel_display);
                        out.push('\n');
                        emitted += 1;
                        if emitted >= head_limit {
                            out.push_str(&format!("(truncated at {head_limit})\n"));
                            return Ok(out);
                        }
                    }
                    "count" => {
                        if skipped < offset {
                            skipped += 1;
                            continue;
                        }
                        out.push_str(&format!("{rel_display}:{file_match_count}\n"));
                        emitted += 1;
                        if emitted >= head_limit {
                            out.push_str(&format!("(truncated at {head_limit})\n"));
                            return Ok(out);
                        }
                    }
                    _ => {} // Already emitted above.
                }
            }
        }

        if out.is_empty() {
            out.push_str("(no matches)\n");
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------

pub struct GlobTool;

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "List file paths in the workspace that match a glob pattern \
         (e.g. `src/**/*.rs`). Returns one path per line."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern relative to workspace root." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: GlobArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let eff_root = ctx.effective_root();
        let pattern_str = args.pattern.clone();

        // Validate the pattern early.
        glob::Pattern::new(&pattern_str).map_err(ToolError::BadGlob)?;

        // Glob walks the filesystem; 10s was too aggressive for large
        // workspaces (crates/, monorepos, asset dirs). Reuse the
        // bash-tool budget so power users can bump both with one
        // setting in metis.toml.
        let glob_budget = ctx.bash.timeout.min(std::time::Duration::from_secs(60));
        let result = tokio::time::timeout(
            glob_budget,
            tokio::task::spawn_blocking(move || {
                let mut out = String::new();
                let mut count = 0usize;

                // Use glob::glob() with the full absolute path so it can prune
                // directory branches based on the pattern prefix — much faster than
                // walking everything and matching afterwards.
                let abs_pattern = eff_root.join(&pattern_str);
                let abs_pattern_str = abs_pattern.to_string_lossy();
                let entries = match glob::glob(&abs_pattern_str) {
                    Ok(paths) => paths,
                    Err(_) => return "(invalid glob pattern)\n".to_string(),
                };

                for path in entries.flatten() {
                    if path.is_dir() {
                        continue;
                    }
                    // Skip excluded dirs anywhere in the path.
                    if path.components().any(|c| {
                        SEARCH_EXCLUDE_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())
                    }) {
                        continue;
                    }
                    let rel = path.strip_prefix(&eff_root).unwrap_or(&path);
                    out.push_str(&format!("{}\n", rel.display()));
                    count += 1;
                    if count >= 500 {
                        out.push_str("(truncated at 500 paths)\n");
                        break;
                    }
                }

                if out.is_empty() {
                    out.push_str("(no matches)\n");
                }
                out
            }),
        )
        .await;

        match result {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(e)) => Err(ToolError::InvalidArgs(e.to_string())),
            Err(_) => Err(ToolError::Timeout(glob_budget)),
        }
    }
}

// ---------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------

pub struct WriteFile;

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file, creating it if it does not exist or \
         overwriting if it does. Creates parent directories as needed. \
         Prefer edit_file for modifying existing files — this tool is \
         for creating new files or full rewrites."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace root." },
                "content": { "type": "string", "description": "The full content to write." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path)?;
        // Create parent directories if needed.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ToolError::Io {
                path: args.path.clone(),
                source,
            })?;
        }
        std::fs::write(&path, &args.content).map_err(|source| ToolError::Io {
            path: args.path.clone(),
            source,
        })?;
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(now) = meta.modified() {
                ctx.file_read_times
                    .lock()
                    .unwrap()
                    .insert(path.clone(), now);
            }
        }
        // Show the file head numbered so the model has a fresh anchor
        // and won't try to re-read it on the next turn. Cap at 30
        // lines — write_file is for new/short files, longer ones get
        // edited in subsequent turns.
        let head = format_file_head_snippet(&args.content, 30);
        Ok(format!(
            "wrote {} ({} bytes)\n\
             FILE NOW (`{}`, head):\n{head}\n\
             (file state is current — do NOT re-read `{}`; trust the snippet above)",
            args.path,
            args.content.len(),
            args.path,
            args.path,
        ))
    }
}

/// Numbered head-of-file snippet (first `max_lines` lines). Used by
/// write_file so the model immediately sees what it just wrote and
/// won't burn a turn on read_file.
fn format_file_head_snippet(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().take(max_lines + 1).collect();
    let total_visible = lines.len().min(max_lines);
    let width = total_visible.to_string().len().max(1);
    let mut out = String::new();
    for (i, line) in lines.iter().take(max_lines).enumerate() {
        out.push_str(&format!("{:>width$}\t{}\n", i + 1, line, width = width));
    }
    if lines.len() > max_lines {
        out.push_str(&format!("(... file continues past line {max_lines} ...)"));
    }
    out
}

// ---------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------

pub struct EditFile;

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace `old_string` with `new_string` in a file. By default the \
         old string must occur exactly once; pass `replace_all: true` to \
         replace every occurrence. Use `read_file` first to confirm the \
         exact text."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: EditArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path)?;

        // State tracking: warn if file was modified externally since last read
        let mut stale_warning = String::new();
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(current_mtime) = meta.modified() {
                let read_times = ctx.file_read_times.lock().unwrap();
                if let Some(last_read) = read_times.get(&path) {
                    if current_mtime > *last_read {
                        stale_warning = format!(
                            "(warning: {} was modified since last read — verify old_string is still correct)\n",
                            args.path
                        );
                    }
                }
            }
        }

        let original = std::fs::read_to_string(&path).map_err(|source| ToolError::Io {
            path: args.path.clone(),
            source,
        })?;
        let count = original.matches(&args.old_string).count();
        if count == 0 {
            return Err(ToolError::EditNotFound(args.path));
        }
        let updated = if args.replace_all {
            original.replace(&args.old_string, &args.new_string)
        } else {
            if count > 1 {
                return Err(ToolError::EditNotUnique {
                    path: args.path,
                    count,
                });
            }
            original.replacen(&args.old_string, &args.new_string, 1)
        };
        std::fs::write(&path, &updated).map_err(|source| ToolError::Io {
            path: args.path.clone(),
            source,
        })?;
        // Refresh the read-time stamp so any future stale-warning
        // check uses *this* edit as the baseline, not an older read.
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(now) = meta.modified() {
                ctx.file_read_times
                    .lock()
                    .unwrap()
                    .insert(path.clone(), now);
            }
        }
        let diff = unified_diff(&original, &updated, &args.path);
        // Inject the actual post-edit snippet around the change so the
        // model trusts the on-disk state instead of any earlier
        // read_file output it cached. Diff alone wasn't enough — small
        // models read the diff but still pull the old content from
        // their context. The snippet shows the file as it is NOW,
        // numbered, with the affected region centered.
        let snippet = format_post_edit_snippet(&updated, &args.new_string);
        Ok(format!(
            "{stale_warning}edited {} ({} replacement{})\n{diff}\n\
             FILE NOW (`{}`, snippet around edit):\n{snippet}\n\
             (file state is current — do NOT re-read `{}`; trust the snippet above)",
            args.path,
            count,
            if count == 1 { "" } else { "s" },
            args.path,
            args.path,
        ))
    }
}

/// Render a numbered snippet of `content` centered on the first
/// occurrence of `anchor`. Returns up to ~25 lines: 10 before the
/// anchor, the anchor lines themselves, and 10 after. Falls back to
/// the head of the file if `anchor` isn't present (defensive — anchor
/// should always be the new_string just inserted).
fn format_post_edit_snippet(content: &str, anchor: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return "(empty file)".to_string();
    }
    // Locate the first line containing any part of the anchor. Anchors
    // can span multiple lines, so we look at each anchor line.
    let anchor_first = anchor.lines().next().unwrap_or("").trim();
    let anchor_idx = if anchor_first.is_empty() {
        0
    } else {
        lines
            .iter()
            .position(|l| l.contains(anchor_first))
            .unwrap_or(0)
    };
    let anchor_line_count = anchor.lines().count().max(1);
    let start = anchor_idx.saturating_sub(10);
    let end = (anchor_idx + anchor_line_count + 10).min(total);
    let width = end.to_string().len();
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("(... {start} earlier line(s) elided ...)\n"));
    }
    for (i, line) in lines[start..end].iter().enumerate() {
        let n = start + i + 1;
        out.push_str(&format!("{n:>width$}\t{line}\n", width = width));
    }
    if end < total {
        let trailing = total - end;
        out.push_str(&format!("(... {trailing} later line(s) elided ...)"));
    }
    out
}

// ── MultiEdit ────────────────────────────────────────────────────────

pub struct MultiEdit;

#[derive(Debug, Deserialize)]
struct MultiEditArgs {
    edits: Vec<SingleEditArg>,
}

#[derive(Debug, Deserialize)]
struct SingleEditArg {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "multi_edit"
    }
    fn description(&self) -> &str {
        "Apply multiple file edits atomically in a single call. All edits \
         are validated first; if any edit would fail, none are applied. \
         Each edit follows the same semantics as `edit_file`."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean", "default": false }
                        },
                        "required": ["path", "old_string", "new_string"],
                        "additionalProperties": false
                    },
                    "minItems": 1
                }
            },
            "required": ["edits"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: MultiEditArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        use std::collections::HashMap;
        let mut file_states: HashMap<PathBuf, String> = HashMap::new();
        let mut originals: HashMap<PathBuf, String> = HashMap::new();

        // Phase 1: Validate ALL edits against original disk state before touching anything.
        // Each old_string must exist in the ORIGINAL file — not the modified one.
        // This prevents cascading failures when the model sends edits in the wrong order.
        for edit in &args.edits {
            let path = ctx.resolve_path(&edit.path)?;

            // Read from disk only on first encounter
            if !file_states.contains_key(&path) {
                let content = std::fs::read_to_string(&path).map_err(|source| ToolError::Io {
                    path: edit.path.clone(),
                    source,
                })?;
                originals.insert(path.clone(), content.clone());
                file_states.insert(path.clone(), content);
            }

            // Validate: old_string must exist somewhere — either in the ORIGINAL file,
            // or in the in-memory state after earlier edits in this batch.
            // This allows the model to send edits in any order without cascading failures.
            let original = originals.get(&path).unwrap();
            let current = file_states.get(&path).unwrap();

            let exists_in_original = original.contains(&edit.old_string);
            let exists_in_current = current.contains(&edit.old_string);

            // old_string must exist in at least one state.
            if !exists_in_original && !exists_in_current {
                return Err(ToolError::EditNotFound(edit.path.clone()));
            }

            // Uniqueness check: only when NOT using replace_all.
            // With replace_all, multiple matches are expected and intended.
            if !edit.replace_all {
                let count_in_original = original.matches(&edit.old_string).count();
                if count_in_original > 1 {
                    return Err(ToolError::EditNotUnique {
                        path: edit.path.clone(),
                        count: count_in_original,
                    });
                }
            }
        }

        // Phase 2: Apply all edits to copies of originals (no cascading)
        for edit in &args.edits {
            let path = ctx.resolve_path(&edit.path)?;
            let current = file_states.get_mut(&path).unwrap();
            *current = if edit.replace_all {
                current.replace(&edit.old_string, &edit.new_string)
            } else {
                current.replacen(&edit.old_string, &edit.new_string, 1)
            };
        }

        // Phase 3: Build preview diffs against originals (for the reply output)
        let mut plans: Vec<(String, String, String, usize)> = Vec::new();
        for edit in &args.edits {
            let path = ctx.resolve_path(&edit.path)?;
            let original = originals.get(&path).unwrap();
            let modified = file_states.get(&path).unwrap();
            let count = if edit.replace_all {
                original.matches(&edit.old_string).count()
            } else {
                1
            };
            plans.push((edit.path.clone(), original.clone(), modified.clone(), count));
        }

        // Phase 4: Write all files atomically — rollback on failure
        let mut written: Vec<PathBuf> = Vec::new();
        for (path, final_content) in &file_states {
            if let Err(source) = std::fs::write(path, final_content) {
                // Rollback all previously written files
                for wb_path in written.iter().rev() {
                    if let Some(orig) = originals.get(wb_path) {
                        let _ = std::fs::write(wb_path, orig);
                    }
                }
                let display = args
                    .edits
                    .iter()
                    .find(|e| ctx.resolve_path(&e.path).ok().as_ref() == Some(path))
                    .map(|e| e.path.clone())
                    .unwrap_or_default();
                return Err(ToolError::Io {
                    path: display,
                    source,
                });
            }
            written.push(path.clone());
            if let Ok(meta) = std::fs::metadata(path) {
                if let Ok(now) = meta.modified() {
                    ctx.file_read_times
                        .lock()
                        .unwrap()
                        .insert(path.clone(), now);
                }
            }
        }

        // Phase 5: combined diffs + per-file post-edit snippets so the
        // model trusts the on-disk state without re-reading.
        let mut output = format!("multi_edit: {} edit(s) applied atomically\n", plans.len());
        for (display_path, before, after, count) in &plans {
            let diff = unified_diff(before, after, display_path);
            output.push_str(&format!(
                "\n── {} ({} replacement{}) ──\n{diff}",
                display_path,
                count,
                if *count == 1 { "" } else { "s" }
            ));
        }
        // Per-file snippets, anchored on the first edit's new_string for
        // that file (works because Phase 1 validated each old_string
        // existed; the edit landed and its replacement is now in `after`).
        let mut by_path: std::collections::BTreeMap<&str, (&str, &str)> =
            std::collections::BTreeMap::new();
        for (path, _before, after, _count) in &plans {
            // Anchor: pick the first edit targeting this path.
            let anchor = args
                .edits
                .iter()
                .find(|e| e.path == *path)
                .map(|e| e.new_string.as_str())
                .unwrap_or("");
            by_path
                .entry(path.as_str())
                .or_insert((after.as_str(), anchor));
        }
        for (path, (after, anchor)) in &by_path {
            let snippet = format_post_edit_snippet(after, anchor);
            output.push_str(&format!(
                "\n\nFILE NOW (`{path}`, snippet around edit):\n{snippet}"
            ));
        }
        let names: Vec<&str> = by_path.keys().copied().collect();
        output.push_str(&format!(
            "\n(file state is current for: {} — do NOT re-read; trust the snippets above)",
            names.join(", ")
        ));
        Ok(output)
    }
}

/// Generate a compact unified diff between two strings.
/// Context lines: 3 (same as `diff -u`). Output uses `---`/`+++`/`@@` markers
/// and `-`/`+` line prefixes. The REPL's stream callback can colorize these
/// (red for `-`, green for `+`). Also used by the permission gate's
/// pre-write preview (see `aegis_core::permission::build_edit_preview`).
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Simple LCS-based diff: find matching line pairs, emit hunks.
    // For large files a proper Myers diff would be better, but this
    // O(n*m) approach is fine for typical edit_file patches (small changes).
    let matches = lcs_lines(&old_lines, &new_lines);

    let mut hunks: Vec<String> = Vec::new();
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    let mut hunk_lines: Vec<String> = Vec::new();
    let mut hunk_old_start = 0usize;
    let mut hunk_new_start = 0usize;
    let mut hunk_old_count = 0usize;
    let mut hunk_new_count = 0usize;
    let context = 3usize;

    let mut match_idx = 0;
    let total_matches = matches.len();

    loop {
        // Advance to next match or end
        let (next_old, next_new) = if match_idx < total_matches {
            matches[match_idx]
        } else {
            (old_lines.len(), new_lines.len())
        };

        // Emit removed lines (in old but not matched)
        while old_idx < next_old {
            if hunk_lines.is_empty() {
                // Start new hunk with context
                let ctx_start_old = old_idx.saturating_sub(context);
                let ctx_start_new = new_idx.saturating_sub(context);
                hunk_old_start = ctx_start_old;
                hunk_new_start = ctx_start_new;
                // Add context lines before this change
                // (tricky with removals — skip for now, context after matches)
            }
            hunk_lines.push(format!("-{}", old_lines[old_idx]));
            hunk_old_count += 1;
            old_idx += 1;
        }

        // Emit added lines (in new but not matched)
        while new_idx < next_new {
            hunk_lines.push(format!("+{}", new_lines[new_idx]));
            hunk_new_count += 1;
            new_idx += 1;
        }

        if match_idx >= total_matches {
            break;
        }

        // This line is a match — context line
        if !hunk_lines.is_empty() {
            hunk_lines.push(format!(" {}", old_lines[old_idx]));
            hunk_old_count += 1;
            hunk_new_count += 1;
        }

        old_idx += 1;
        new_idx += 1;
        match_idx += 1;

        // If we've accumulated enough trailing context after a change,
        // flush the hunk. Check if the next change is far away.
        let next_change_far = if match_idx < total_matches {
            let (no, nn) = matches[match_idx];
            no == old_idx && nn == new_idx // next match is immediately adjacent
        } else {
            old_idx == old_lines.len() && new_idx == new_lines.len()
        };

        if !hunk_lines.is_empty() && next_change_far {
            // Check how far the next actual diff is
            // For simplicity, flush if we have content
        }
    }

    // Flush remaining hunk
    if !hunk_lines.is_empty() {
        let header = format!(
            "@@ -{},{} +{},{} @@",
            hunk_old_start + 1,
            hunk_old_count,
            hunk_new_start + 1,
            hunk_new_count
        );
        hunks.push(format!("{header}\n{}", hunk_lines.join("\n")));
    }

    if hunks.is_empty() {
        return String::new();
    }

    format!("--- a/{path}\n+++ b/{path}\n{}\n", hunks.join("\n"))
}

/// Longest Common Subsequence of lines — returns pairs of (old_idx, new_idx).
pub(super) fn lcs_lines<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(usize, usize)> {
    let n = old.len();
    let m = new.len();
    // DP table
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i] == new[j] {
                1 + dp[i + 1][j + 1]
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Trace back
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < n && j < m {
        if old[i] == new[j] {
            result.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    result
}

#[cfg(test)]
mod read_file_preflight_tests {
    use super::*;
    use std::io::Write;

    fn tmp_file(name: &str, body: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("metis-fs-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body).unwrap();
        path
    }

    #[test]
    fn looks_binary_detects_nul() {
        assert!(looks_binary(b"hello\0world"));
        assert!(!looks_binary(b"hello world\nno nul here"));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn read_text_file_rejects_binary_input() {
        let path = tmp_file("binary.bin", b"abc\0def\0ghi");
        let res = read_text_file(&path, "binary.bin", None, None);
        assert!(res.is_err(), "binary file should be rejected");
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.to_lowercase().contains("binary"),
            "error should mention binary, got: {msg}"
        );
    }

    #[test]
    fn read_text_file_rejects_huge_file_without_limit() {
        // Build a >4MB text file (no NUL bytes — pure ASCII).
        let body = "x".repeat((READ_FILE_SLURP_LIMIT as usize) + 1024);
        let path = tmp_file("huge.txt", body.as_bytes());
        let res = read_text_file(&path, "huge.txt", None, None);
        assert!(res.is_err(), "huge file w/o limit should be rejected");
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("too large"), "msg: {msg}");
        assert!(msg.contains("limit"), "guidance missing: {msg}");
    }

    #[test]
    fn read_text_file_streams_huge_file_with_limit() {
        // Build many short lines so we can request just 5.
        let mut body = String::new();
        // Need to exceed 4MB — each line ~32 bytes → ~130k lines.
        for i in 0..200_000usize {
            body.push_str(&format!("line {i:08} payload xxxxxxxx\n"));
        }
        assert!(
            body.len() as u64 > READ_FILE_SLURP_LIMIT,
            "test setup: body must exceed slurp limit"
        );
        let path = tmp_file("big_lines.txt", body.as_bytes());
        let out = read_text_file(&path, "big_lines.txt", None, Some(5))
            .expect("limit path should succeed");
        // Should have exactly 5 line-number prefixes.
        let count = out.matches('\t').count();
        assert_eq!(count, 5, "expected 5 lines, got {count}: {out}");
        assert!(out.contains("line 00000000"), "first line missing: {out}");
        assert!(
            !out.contains("line 00000005"),
            "should not include line 5: {out}"
        );
    }

    #[test]
    fn read_text_file_small_file_unchanged_behaviour() {
        let body = "alpha\nbeta\ngamma\n";
        let path = tmp_file("small.txt", body.as_bytes());
        let out = read_text_file(&path, "small.txt", None, None).expect("small file read");
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains("gamma"));
    }
}
