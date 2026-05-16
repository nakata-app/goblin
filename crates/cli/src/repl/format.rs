//! Tool-call display formatters and syntax-highlighting helpers.
//!
//! All public items are `pub(super)` because repl.rs is the only
//! consumer. Exposing them as a submodule keeps repl.rs focused on
//! loop/dispatch concerns; these are pure string transformations with
//! no REPL state.

/// Canonical, lowercase tool name for display. Normalizes aliases so
/// `Bash`, `shell`, `bash` all render as `bash`.
pub(super) fn canonical_tool_name(name: &str) -> &str {
    match name.to_lowercase().as_str() {
        "bash" | "shell" | "run_command" | "execute" => "bash",
        "read" | "readfile" | "read_file" | "view" => "read",
        "write" | "writefile" | "write_file" | "create_file" => "write",
        "edit" | "editfile" | "edit_file" | "str_replace" => "edit",
        "grep" | "search_files" | "ripgrep" => "grep",
        "glob" | "find_files" | "list_files" => "glob",
        "ls" | "listdir" | "list_dir" => "ls",
        _ => name,
    }
}

/// Human-readable one-line summary of a tool call.
///
/// Each known tool gets idiomatic formatting; unknown tools fall back to
/// the JSON-value strip. Examples:
///
/// ```text
/// bash  {"command":"git status"}         →  $ git status
/// read  {"file_path":"src/main.rs"}      →  src/main.rs
/// grep  {"pattern":"foo","path":"src/"}  →  grep "foo" in src/
/// glob  {"pattern":"**/*.rs"}            →  **/*.rs
/// write {"file_path":"out.txt"}          →  out.txt
/// edit  {"file_path":"a.rs","...":...}   →  a.rs
/// ```
pub(super) fn format_tool_call(name: &str, raw_args: &str) -> String {
    const MAX: usize = 90;
    let args: Option<serde_json::Value> = serde_json::from_str(raw_args).ok();
    let get =
        |key: &str| -> Option<String> { args.as_ref()?.get(key)?.as_str().map(|s| s.to_string()) };
    let get_u = |key: &str| -> Option<u64> { args.as_ref()?.get(key)?.as_u64() };

    let pretty: Option<String> = (|| {
        Some(match canonical_tool_name(name) {
            "bash" => format!("$ {}", get("command")?),
            "read" => {
                let path = get("file_path").or_else(|| get("path"))?;
                let limit = get_u("limit")
                    .map(|l| format!(" ({l} lines)"))
                    .unwrap_or_default();
                let offset = get_u("offset")
                    .map(|o| format!(", from line {o}"))
                    .unwrap_or_default();
                format!("{path}{limit}{offset}")
            }
            "write" => get("file_path").or_else(|| get("path"))?,
            "edit" => format!("→ {}", get("file_path").or_else(|| get("path"))?),
            "grep" => {
                let pat = get("pattern")?;
                let path = get("path").unwrap_or_else(|| ".".to_string());
                format!("\"{pat}\" in {path}")
            }
            "glob" => get("pattern")?,
            "ls" => get("path").unwrap_or_else(|| ".".to_string()),
            _ => return None,
        })
    })();
    let pretty = pretty.unwrap_or_else(|| format_tool_arg(raw_args));

    if pretty.chars().count() <= MAX {
        pretty
    } else {
        let head: String = pretty.chars().take(MAX.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Returns `(canonical_name, arg_string)` — the split form used by
/// callers that need to display name and arg separately.
pub(crate) fn format_tool_call_display(name: &str, raw_args: &str) -> (String, String) {
    let canonical = canonical_tool_name(name).to_string();
    let arg = format_tool_call(name, raw_args);
    (canonical, arg)
}

/// Reformat the raw tool-arguments preview for the `●` header.
///
/// `arguments_preview` arrives as flattened JSON like
/// `{"path":"src/main.rs","start":1}`. For the inline display we want
/// just the values, comma-joined, with the JSON noise stripped — and
/// truncated to keep the line under terminal width.
pub(super) fn format_tool_arg(raw: &str) -> String {
    const MAX: usize = 80;
    let trimmed = raw.trim();
    // Try to extract JSON values; on parse failure fall back to raw.
    let pretty = extract_json_values(trimmed).unwrap_or_else(|| trimmed.to_string());
    if pretty.chars().count() <= MAX {
        return pretty;
    }
    let head: String = pretty.chars().take(MAX.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Walk a flat JSON object string and join its scalar values with `, `.
/// Returns `None` if the input doesn't look like a JSON object.
pub(super) fn extract_json_values(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    let mut values: Vec<String> = Vec::new();
    let mut i = 1;
    while i < bytes.len() - 1 {
        // Skip past the key: find ':'
        let colon = s[i..].find(':')? + i;
        let mut j = colon + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        let (val, next) = if bytes[j] == b'"' {
            // String value: read until next unescaped quote
            let mut k = j + 1;
            while k < bytes.len() && bytes[k] != b'"' {
                if bytes[k] == b'\\' && k + 1 < bytes.len() {
                    k += 2;
                    continue;
                }
                k += 1;
            }
            if k >= bytes.len() {
                return None;
            }
            (s[j + 1..k].to_string(), k + 1)
        } else {
            // Scalar (number/bool/null): read until ',' or '}'
            let mut k = j;
            while k < bytes.len() && bytes[k] != b',' && bytes[k] != b'}' {
                k += 1;
            }
            (s[j..k].trim().to_string(), k)
        };
        values.push(val);
        i = next;
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
    }
    Some(values.join(", "))
}

/// Tighten a tool result preview: collapse newlines, trim, and clamp.
///
/// Also sanitizes the LLM-facing `[stashed: ctx://<hash> — N bytes, M
/// lines]` format so the user never sees internal blob references —
/// only a clean size summary like `(output: 12.3 MB, 200 lines)`.
/// Empty `[stdout]`/`[stderr]` markers are dropped entirely.
pub(crate) fn trim_tool_preview(raw: &str) -> String {
    const MAX: usize = 100;
    let sanitized = sanitize_stash_leak(raw);
    let flat: String = sanitized
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed: String = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX {
        return collapsed;
    }
    let head: String = collapsed.chars().take(MAX.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Strip `ctx://<hash>` references and replace stash markers with
/// human-readable size summaries. Pure string transform — no I/O.
pub(super) fn sanitize_stash_leak(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("[stashed:") {
        out.push_str(&rest[..start]);
        // Find the matching closing bracket
        let after = &rest[start..];
        let Some(end) = after.find(']') else {
            // Malformed — emit as-is and bail
            out.push_str(after);
            return out;
        };
        let body = &after[..=end]; // [stashed: ... ]
        let summary = stash_summary(body);
        out.push_str(&summary);
        rest = &after[end + 1..];
        // Skip the "--- preview ---" header that follows so the actual
        // preview content (which the LLM may have included) stays.
        if let Some(stripped) = rest.trim_start().strip_prefix("--- preview ---") {
            // preserve a single newline before the preview
            rest = stripped;
        }
    }
    out.push_str(rest);
    // Drop empty `[stdout]` / `[stderr]` markers.
    out = out
        .replace("[stdout]\n", "")
        .replace("[stderr]\n", "")
        .replace("[stdout]", "")
        .replace("[stderr]", "");
    out
}

/// Convert `[stashed: ctx://abc — 12345 bytes, 200 lines]` into a clean
/// `(output: 12.1 KB, 200 lines)` summary. Keeps the size + line count
/// but drops the hash that the user does not need.
fn stash_summary(body: &str) -> String {
    let inner = body
        .strip_prefix("[stashed:")
        .unwrap_or(body)
        .trim_end_matches(']')
        .trim();
    let inner = inner.strip_prefix("ctx://").unwrap_or(inner);
    // Skip past hash up to the em-dash (or `-`) that separates ref
    // from size info.
    let rest = inner
        .split_once('—')
        .map(|(_, r)| r)
        .or_else(|| inner.split_once('-').map(|(_, r)| r))
        .unwrap_or(inner)
        .trim();
    // rest looks like "12345 bytes, 200 lines"
    let mut bytes: Option<u64> = None;
    let mut lines: Option<u64> = None;
    for part in rest.split(',') {
        let p = part.trim();
        if let Some(n) = p.strip_suffix("bytes").map(str::trim) {
            bytes = n.parse().ok();
        } else if let Some(n) = p.strip_suffix("lines").map(str::trim) {
            lines = n.parse().ok();
        }
    }
    match (bytes, lines) {
        (Some(b), Some(l)) => format!("(output: {}, {} lines)", format_size(b), l),
        (Some(b), None) => format!("(output: {})", format_size(b)),
        (None, Some(l)) => format!("(output: {} lines)", l),
        (None, None) => "(output truncated)".to_string(),
    }
}

/// Format file size in human-readable format (KB, MB, GB, etc.)
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}

/// Format a time as "X days/hours/minutes ago" or "just now"
pub(crate) fn format_time_ago(time: std::time::SystemTime) -> String {
    match time.elapsed() {
        Ok(duration) => {
            let secs = duration.as_secs();
            if secs < 60 {
                "just now".to_string()
            } else if secs < 3600 {
                format!("{} min ago", secs / 60)
            } else if secs < 86400 {
                format!("{} hr ago", secs / 3600)
            } else if secs < 2592000 {
                // ~30 days
                format!("{} days ago", secs / 86400)
            } else if secs < 31536000 {
                // 365 days
                format!("{} months ago", secs / 2592000)
            } else {
                format!("{} years ago", secs / 31536000)
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Basic syntax highlighting for different file types
pub(crate) fn highlight_line(line: &str, ext: &str) -> String {
    if line.trim().is_empty() {
        return "\x1b[2m⟨empty⟩\x1b[0m".to_string();
    }

    match ext {
        // Rust
        "rs" => {
            let mut colored = line.to_string();
            // Keywords
            let keywords = [
                "fn ", "let ", "mut ", "pub ", "struct ", "enum ", "impl ", "trait ", "match ",
                "if ", "else ", "for ", "while ", "loop ", "return ", "use ", "mod ", "crate ",
                "self", "Self", "true", "false", "Some", "None", "Ok", "Err", "Option", "Result",
                "String", "Vec", "Box", "Arc",
            ];
            for &kw in &keywords {
                colored = colored.replace(kw, &format!("\x1b[34m{}\x1b[0m", kw));
                // blue
            }
            // Types
            let types = [
                "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "usize",
                "isize", "f32", "f64", "bool", "char", "str",
            ];
            for &ty in &types {
                colored = colored.replace(ty, &format!("\x1b[33m{}\x1b[0m", ty));
                // yellow
            }
            // Strings
            colored = colored.replace("\"", "\x1b[32m\"");
            if colored.contains("\x1b[32m\"") && colored.matches("\"").count() % 2 == 1 {
                colored += "\x1b[0m";
            }
            // Comments
            if colored.contains("//") {
                let parts: Vec<&str> = colored.splitn(2, "//").collect();
                colored = format!("{}\x1b[90m//{}\x1b[0m", parts[0], parts[1]);
            }
            colored
        }
        // Python
        "py" => {
            let mut colored = line.to_string();
            // Keywords
            let keywords = [
                "def ",
                "class ",
                "import ",
                "from ",
                "as ",
                "if ",
                "elif ",
                "else ",
                "for ",
                "while ",
                "try ",
                "except ",
                "finally ",
                "with ",
                "return ",
                "yield ",
                "async ",
                "await ",
                "lambda ",
                "global ",
                "nonlocal ",
                "True",
                "False",
                "None",
                "self",
                "pass",
                "break",
                "continue",
            ];
            for &kw in &keywords {
                colored = colored.replace(kw, &format!("\x1b[34m{}\x1b[0m", kw));
            }
            // Builtins
            let builtins = [
                "print",
                "len",
                "range",
                "enumerate",
                "zip",
                "map",
                "filter",
                "reduce",
                "str",
                "int",
                "float",
                "bool",
                "list",
                "dict",
                "tuple",
                "set",
            ];
            for &b in &builtins {
                colored = colored.replace(b, &format!("\x1b[33m{}\x1b[0m", b));
            }
            // Strings
            colored = colored.replace("\"", "\x1b[32m\"");
            colored = colored.replace("'", "\x1b[32m'");
            // Comments
            if colored.contains("#") {
                let parts: Vec<&str> = colored.splitn(2, "#").collect();
                colored = format!("{}\x1b[90m#{}\x1b[0m", parts[0], parts[1]);
            }
            colored
        }
        // JavaScript/TypeScript
        "js" | "ts" => {
            let mut colored = line.to_string();
            // Keywords
            let keywords = [
                "function ",
                "const ",
                "let ",
                "var ",
                "if ",
                "else ",
                "for ",
                "while ",
                "return ",
                "class ",
                "extends ",
                "import ",
                "export ",
                "from ",
                "default ",
                "async ",
                "await ",
                "try ",
                "catch ",
                "finally ",
                "throw ",
                "new ",
                "true",
                "false",
                "null",
                "undefined",
                "this",
                "super",
            ];
            for &kw in &keywords {
                colored = colored.replace(kw, &format!("\x1b[34m{}\x1b[0m", kw));
            }
            // Strings
            colored = colored.replace("\"", "\x1b[32m\"");
            colored = colored.replace("'", "\x1b[32m'");
            colored = colored.replace("`", "\x1b[32m`");
            // Comments
            if colored.contains("//") {
                let parts: Vec<&str> = colored.splitn(2, "//").collect();
                colored = format!("{}\x1b[90m//{}\x1b[0m", parts[0], parts[1]);
            }
            colored
        }
        // Markdown
        "md" => {
            let mut colored = line.to_string();
            // Headers
            if line.starts_with('#') {
                colored = format!("\x1b[1;36m{}\x1b[0m", line);
            }
            // Lists
            else if line.trim_start().starts_with('-') || line.trim_start().starts_with('*') {
                colored = format!("\x1b[33m{}\x1b[0m", line);
            }
            // Code blocks
            else if line.trim().starts_with("```") {
                colored = format!("\x1b[35m{}\x1b[0m", line);
            }
            // Links
            else if line.contains("http://") || line.contains("https://") {
                colored = format!("\x1b[4;34m{}\x1b[0m", line);
            }
            colored
        }
        // JSON
        "json" => {
            let mut colored = line.to_string();
            // Keys and strings
            if line.contains("\"") {
                let parts: Vec<&str> = line.split("\"").collect();
                colored = String::new();
                for (i, part) in parts.iter().enumerate() {
                    if i % 2 == 0 {
                        colored.push_str(part);
                    } else {
                        colored.push_str(&format!("\x1b[32m\"{}\"\x1b[0m", part));
                    }
                    if i < parts.len() - 1 {
                        colored.push('"');
                    }
                }
            }
            // Numbers and booleans
            colored = colored.replace("true", "\x1b[33mtrue\x1b[0m");
            colored = colored.replace("false", "\x1b[33mfalse\x1b[0m");
            colored = colored.replace("null", "\x1b[33mnull\x1b[0m");
            colored
        }
        // Default - just return the line
        _ => line.to_string(),
    }
}
