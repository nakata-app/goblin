//! Boxed display helpers — box-drawing characters + ANSI colour for
//! tables, diagrams, and structured terminal output.  Inspired by
//! Claude Code's `display_boxed_text`.
//!
//! # Example
//!
//! ```ignore
//! use aegis_core::display;
//!
//! let table = display::table(
//!     Some("Files"),
//!     &["Path", "Size"],
//!     &[
//!         vec!["src/main.rs", "12 KiB"],
//!         vec!["README.md", "1 KiB"],
//!     ],
//! );
//! println!("{table}");
//! ```

/// Light-gray border so it's visible without screaming.
const BORDER: &str = "\x1b[37m";
const RESET: &str = "\x1b[0m";

// ---- Box primitives ----

fn hr_top(cols: &[usize]) -> String {
    let mut s = String::from(BORDER);
    s.push('╔');
    for (i, &w) in cols.iter().enumerate() {
        if i > 0 {
            s.push('╦');
        }
        for _ in 0..w + 2 {
            s.push('═');
        }
    }
    s.push('╗');
    s.push_str(RESET);
    s
}

fn hr_mid(cols: &[usize]) -> String {
    let mut s = String::from(BORDER);
    s.push('╠');
    for (i, &w) in cols.iter().enumerate() {
        if i > 0 {
            s.push('╬');
        }
        for _ in 0..w + 2 {
            s.push('═');
        }
    }
    s.push('╣');
    s.push_str(RESET);
    s
}

fn hr_bot(cols: &[usize]) -> String {
    let mut s = String::from(BORDER);
    s.push('╚');
    for (i, &w) in cols.iter().enumerate() {
        if i > 0 {
            s.push('╩');
        }
        for _ in 0..w + 2 {
            s.push('═');
        }
    }
    s.push('╝');
    s.push_str(RESET);
    s
}

fn row(cols: &[usize], cells: &[String], cell_style: &str) -> String {
    let mut s = String::from(BORDER);
    s.push('║');
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            s.push_str(BORDER);
            s.push('║');
        }
        let w = cols.get(i).copied().unwrap_or(10);
        s.push_str(cell_style);
        s.push(' ');
        s.push_str(cell);
        // pad with spaces to fill column
        let visible_len = strip_ansi(cell).len();
        for _ in 0..w.saturating_sub(visible_len) {
            s.push(' ');
        }
        s.push(' ');
        s.push_str(RESET);
    }
    s.push_str(BORDER);
    s.push('║');
    s.push_str(RESET);
    s
}

fn title_bar(cols: &[usize], title: &str, title_style: &str) -> String {
    let total_width: usize = cols.iter().map(|w| w + 3).sum::<usize>() + 1;
    let visible_title = strip_ansi(title);
    let pad_left = total_width.saturating_sub(visible_title.len()) / 2;
    let mut s = String::from(BORDER);
    s.push('╔');
    for _ in 0..pad_left.saturating_sub(1) {
        s.push('═');
    }
    s.push(' ');
    s.push_str(title_style);
    s.push_str(title);
    s.push_str(RESET);
    s.push_str(BORDER);
    s.push(' ');
    let remaining = total_width - pad_left - 1 - visible_title.len() - 1;
    for _ in 0..remaining {
        s.push('═');
    }
    if !s.ends_with('╗') {
        s.push('╗');
    }
    s.push_str(RESET);
    s
}

// ---- Public API ----

/// Render a table with an optional title, a header row, and data rows.
/// Column widths are computed automatically from header + data.
pub fn table(title: Option<&str>, header: &[&str], data: &[Vec<&str>]) -> String {
    let ncols = header.len().max(1);
    let mut cols: Vec<usize> = (0..ncols).map(|i| strip_ansi(header.get(i).copied().unwrap_or("")).len()).collect();

    for row in data {
        for (i, cell) in row.iter().enumerate() {
            let w = strip_ansi(cell).len();
            if i < cols.len() && w > cols[i] {
                cols[i] = w;
            }
        }
    }

    // minimum 8 chars per column
    for w in &mut cols {
        *w = (*w).max(8);
    }

    let header_style = "\x1b[1m\x1b[36m"; // bold cyan
    let cell_style = "\x1b[37m"; // white

    let mut lines: Vec<String> = Vec::new();

    if let Some(t) = title {
        lines.push(title_bar(&cols, t, "\x1b[1m\x1b[33m")); // bold yellow
    } else {
        lines.push(hr_top(&cols));
    }

    lines.push(row(&cols, &header.iter().map(|h| h.to_string()).collect::<Vec<_>>(), header_style));
    lines.push(hr_mid(&cols));

    for data_row in data {
        let cells: Vec<String> = data_row.iter().map(|s| s.to_string()).collect();
        lines.push(row(&cols, &cells, cell_style));
    }

    lines.push(hr_bot(&cols));
    lines.join("\n")
}

/// Render a simple box around a block of text.  Useful for diagrams,
/// ascii-art schematics, or highlighted code blocks.
pub fn boxed(lines: &[&str], title: Option<&str>, border_color: Option<&str>) -> String {
    let border = border_color.unwrap_or("\x1b[37m");
    let width = lines.iter().map(|l| strip_ansi(l).len()).max().unwrap_or(10) + 4;

    let mut out = vec![format!("{border}╔{}╗\x1b[0m", "═".repeat(width - 2))];

    if let Some(t) = title {
        let t_visible = strip_ansi(t);
        let pad = (width - 2).saturating_sub(t_visible.len()) / 2;
        out.push(format!(
            "{border}║\x1b[1m\x1b[33m{}{}\x1b[0m{border}║\x1b[0m",
            " ".repeat(pad),
            t
        ));
        out.push(format!("{border}╠{}╣\x1b[0m", "═".repeat(width - 2)));
    }

    for line in lines {
        let visible_len = strip_ansi(line);
        let pad = width.saturating_sub(visible_len.len()) - 4;
        out.push(format!("{border}║ \x1b[0m{}\x1b[0m{} {border}║\x1b[0m", line, " ".repeat(pad)));
    }

    out.push(format!("{border}╚{}╝\x1b[0m", "═".repeat(width - 2)));
    out.join("\n")
}

/// Strip ANSI escape sequences to compute visible string width.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
            // skip this char
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Copy `text` to the system clipboard.
///
/// On macOS: shells out to `pbcopy`, which writes directly to the system
/// clipboard via AppKit/NSPasteboard. Works in any terminal (Terminal.app,
/// iTerm2, ghostty, kitty, …) because pbcopy bypasses the terminal layer.
///
/// On every other OS: emits an OSC 52 escape sequence on stdout. Works in
/// terminals that honor OSC 52 (kitty, WezTerm, foot, tmux with
/// `set-clipboard on`); silently ignored elsewhere.
///
/// Returns `true` if the call was issued. On macOS this means pbcopy
/// spawned and we waited on it; on other OSes it means the sequence was
/// flushed to stdout. Neither path can guarantee the bytes reached the
/// user's clipboard — it's a best-effort probe.
pub fn copy_to_clipboard_osc52(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return true;
        }
        return false;
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::io::{self, Write};
        let encoded = base64_encode(text);
        let osc = format!("\x1b]52;c;{}\x1b\\", encoded);
        if io::stdout().write_all(osc.as_bytes()).is_ok() {
            let _ = io::stdout().flush();
            return true;
        }
        false
    }
}

/// Minimal base64 encoder for the OSC 52 clipboard fallback below.
/// macOS uses `pbcopy` and never reaches the OSC 52 path, so this is
/// gated to non-macOS targets — without the gate the symbol is dead
/// code on macOS builds and the workspace lint trips.
#[cfg(not(target_os = "macos"))]
fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_basic() {
        let t = table(
            Some("Files"),
            &["Path", "Size"],
            &[vec!["src/main.rs", "12 KiB"], vec!["README.md", "1 KiB"]],
        );
        assert!(t.contains("Files"));
        assert!(t.contains("src/main.rs"));
        assert!(t.contains("12 KiB"));
    }

    #[test]
    fn boxed_basic() {
        let b = boxed(&["hello", "world"], Some("Title"), None);
        assert!(b.contains("hello"));
        assert!(b.contains("world"));
        assert!(b.contains("Title"));
    }

    #[test]
    fn strip_ansi_no_escape() {
        assert_eq!(strip_ansi("hello"), "hello");
    }

    #[test]
    fn strip_ansi_color() {
        assert_eq!(strip_ansi("\x1b[34mblue\x1b[0m"), "blue");
    }

    #[test]
    fn osc52_encodes_short_string() {
        let text = "hello";
        assert!(super::copy_to_clipboard_osc52(text));
    }

    #[test]
    fn osc52_empty_string() {
        assert!(super::copy_to_clipboard_osc52(""));
    }

    #[test]
    fn osc52_unicode_content() {
        assert!(super::copy_to_clipboard_osc52("merhaba dünya 🌍"));
    }

    #[test]
    fn osc52_long_string_no_panic() {
        let text = "a".repeat(128_000);
        let _ = super::copy_to_clipboard_osc52(&text);
    }
}
