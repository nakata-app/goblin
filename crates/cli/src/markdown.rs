//! Incremental markdown renderer for terminal output.
//!
//! Works with streaming text deltas. Call [`MdRenderer::push`] with each
//! chunk — it buffers until a newline, then emits the styled line once.
//!
//! Earlier versions printed each raw char as it arrived and tried to
//! overwrite the line with styled output via `\r\x1b[2K`. That broke
//! whenever the raw text wrapped across terminal rows: the escape only
//! clears the cursor's current row, so the wrapped-off prefix stayed
//! visible above the reprinted styled line and the user saw the
//! beginning of the sentence twice. Buffering until newline is the
//! simplest fix that stays correct regardless of terminal width or
//! Unicode width edge cases. Streaming feel is preserved because model
//! output arrives in sentence-sized deltas and tool/turn boundaries
//! always flush via [`MdRenderer::finish`].
//!
//! Handles: **bold**, *italic*, `code`, ```fenced blocks```, # headings,
//! - bullet lists.

use std::collections::HashSet;
use std::io::Write;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
/// Warm orange used for the assistant's prose so the user can tell at
/// a glance which lines came from them and which came from Metis.
/// Truecolor (24-bit) with a graceful 256-color fallback isn't worth
/// the complexity here — every modern terminal Metis runs on supports
/// the 38;2 sequence.
const ASSISTANT: &str = "\x1b[38;2;255;176;80m";

/// Lines shorter than this are exempt from dedup — short lines
/// (greetings, single words, "Done.") commonly repeat as legitimate
/// content and would otherwise produce false positives.
const DEDUP_MIN_LEN: usize = 24;

/// Whether repeat-killing filters should be active for a given
/// provider. `false` for `glm` / `zai` / `z.ai` — those reasoning
/// models emit legitimate verbatim repetition (headings, "Step 1…Step
/// 2…" scaffolding) and the filters mis-fire, silently eating real
/// output. `true` everywhere else keeps the loop-killer active.
pub fn provider_wants_repeat_filters(provider: &str) -> bool {
    !matches!(
        provider.to_ascii_lowercase().as_str(),
        "glm" | "zai" | "z.ai"
    )
}

/// Resolve the effective MdRenderer dedup state for `provider`. Reads
/// `METIS_LINE_DEDUP` at call time (opt-in) and forces `false` on z.ai
/// / GLM regardless. Centralized so REPL startup, router swaps, and
/// `/provider` switches all agree.
pub fn resolve_dedup_enabled(provider: &str) -> bool {
    std::env::var_os("METIS_LINE_DEDUP").is_some() && provider_wants_repeat_filters(provider)
}

pub struct MdRenderer {
    /// Characters in the current incomplete line.
    line_buf: String,
    /// Inside a fenced code block?
    in_code_block: bool,
    /// Language from opening fence.
    code_lang: String,
    /// Trimmed contents of every non-trivial line completed in the
    /// current turn. Used to suppress sentence/paragraph repetition
    /// where the model restates an idea verbatim. Reset between turns
    /// via [`reset_turn`].
    seen_lines: HashSet<String>,
    /// Whether the per-turn long-line dedup is active. Some models
    /// (e.g. GLM 5.1) legitimately repeat 24+ char lines as part of
    /// normal output (restated headings, list items, "Adım 1…Adım 2…"
    /// scaffolding). For those, dedup silently eats real content and
    /// the user sees text disappear with no warning. REPL flips this
    /// off when the active provider is z.ai / GLM.
    dedup_enabled: bool,
}

impl MdRenderer {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            seen_lines: HashSet::new(),
            // Dedup default-off: silent-dropping long lines eats
            // legit repeated output (web-fetch summaries, reasoning
            // model scaffolds, list items) with no indicator to the
            // user. The API-layer scan_for_repeat already catches
            // true loops. Opt in with METIS_LINE_DEDUP=1 if you want
            // the extra filter.
            dedup_enabled: std::env::var_os("METIS_LINE_DEDUP").is_some(),
        }
    }

    /// Toggle per-turn long-line dedup at runtime. REPL calls this when
    /// the active provider changes (router auto-route or explicit
    /// `/provider`) so the filter matches the model currently producing
    /// output instead of the one that was active at REPL startup.
    pub fn set_dedup_enabled(&mut self, enabled: bool) {
        self.dedup_enabled = enabled;
    }

    pub fn dedup_enabled(&self) -> bool {
        self.dedup_enabled
    }

    /// Clear per-turn dedup state. Call at the start of each new
    /// agent turn so legitimate cross-turn repetition (e.g. the model
    /// quoting the same identifier twice in two separate replies) is
    /// not silently dropped.
    pub fn reset_turn(&mut self) {
        self.seen_lines.clear();
        // Defensively clear any leftover partial line from a previous
        // turn that was interrupted before its trailing newline.
        self.line_buf.clear();
    }

    /// Push a text delta. Buffers until a newline, then emits the
    /// styled line. Nothing is printed mid-line — see the module
    /// docstring for why raw streaming was removed.
    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.complete_line();
            } else if ch != '\r' {
                self.line_buf.push(ch);
            }
        }
    }

    /// A full line is ready — emit the styled version.
    fn complete_line(&mut self) {
        let line = std::mem::take(&mut self.line_buf);

        // Per-turn line dedup: drop the line entirely (no styled
        // reprint, no newline emitted) if it's a verbatim repeat of an
        // earlier line in this turn. Only applies to substantive lines
        // — code-block bodies, short lines, and structural markup are
        // exempt to keep false positives near zero.
        if self.dedup_enabled && !self.in_code_block {
            let trimmed = line.trim();
            if trimmed.len() >= DEDUP_MIN_LEN && !self.seen_lines.insert(trimmed.to_string()) {
                let _ = std::io::stdout().flush();
                return;
            }
        }

        // Code fence toggle
        if line.starts_with("```") {
            if self.in_code_block {
                self.in_code_block = false;
                self.code_lang.clear();
            } else {
                self.in_code_block = true;
                self.code_lang = line.trim_start_matches('`').trim().to_string();
                if !self.code_lang.is_empty() {
                    print!("{DIM}{GREEN}// {}{RESET}", self.code_lang);
                }
            }
            println!();
            let _ = std::io::stdout().flush();
            return;
        }

        if self.in_code_block {
            println!("{DIM}  {line}{RESET}");
            let _ = std::io::stdout().flush();
            return;
        }

        // Headings
        if let Some(heading) = line.strip_prefix("### ") {
            println!("{ASSISTANT}{BOLD}{heading}{RESET}");
            let _ = std::io::stdout().flush();
            return;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            println!("{ASSISTANT}{BOLD}{heading}{RESET}");
            let _ = std::io::stdout().flush();
            return;
        }
        if let Some(heading) = line.strip_prefix("# ") {
            println!("{ASSISTANT}{BOLD}{UNDERLINE}{heading}{RESET}");
            let _ = std::io::stdout().flush();
            return;
        }

        // Bullets
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            println!("{ASSISTANT}  • {}{RESET}", render_inline(rest));
            let _ = std::io::stdout().flush();
            return;
        }

        // Numbered lists: "1. ", "2. ", etc.
        if let Some(dot_pos) = line.find(". ") {
            let prefix = &line[..dot_pos];
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                let rest = &line[dot_pos + 2..];
                println!("{ASSISTANT}  {prefix}. {}{RESET}", render_inline(rest));
                let _ = std::io::stdout().flush();
                return;
            }
        }

        // Normal line
        println!("{ASSISTANT}{}{RESET}", render_inline(&line));
        let _ = std::io::stdout().flush();
    }

    /// Flush any remaining partial line at end of response.
    pub fn finish(&mut self) {
        if !self.line_buf.is_empty() {
            self.complete_line();
        }
    }
}

/// Render inline markdown: **bold**, *italic*, `code`.
///
/// Bold (`**...**`) is intentionally rendered as **plain text** with the
/// markers stripped — the system prompt forbids bold output, but the
/// model still emits `**` constantly. Stripping at render time gives a
/// clean terminal regardless of whether the model behaves.
pub fn render_inline(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // **bold** → strip markers, keep inner text plain
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&inner);
                i = end + 2;
                continue;
            }
        }
        // *italic*
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            if let Some(end) = find_single(&chars, i + 1, '*') {
                out.push_str(ITALIC);
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 1;
                continue;
            }
        }
        // `code`
        if chars[i] == '`' {
            if let Some(end) = find_single(&chars, i + 1, '`') {
                out.push_str(CYAN);
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_closing(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let mlen = marker.len();
    if chars.len() < from + mlen {
        return None;
    }
    for j in from..=chars.len() - mlen {
        if chars[j..j + mlen] == *marker {
            return Some(j);
        }
    }
    None
}

fn find_single(chars: &[char], from: usize, marker: char) -> Option<usize> {
    chars
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, &c)| c == marker)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_rendering_strips_markers() {
        // Bold is intentionally stripped: markers removed, inner kept
        // as plain text, no ANSI bold escape emitted.
        let out = render_inline("hello **world** end");
        assert_eq!(out, "hello world end");
        assert!(!out.contains(BOLD));
        assert!(!out.contains("**"));
    }

    #[test]
    fn italic_rendering() {
        let out = render_inline("hello *world* end");
        assert!(out.contains(ITALIC));
        assert!(out.contains("world"));
    }

    #[test]
    fn code_rendering() {
        let out = render_inline("use `cargo build` here");
        assert!(out.contains(CYAN));
        assert!(out.contains("cargo build"));
    }

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(render_inline("plain text"), "plain text");
    }

    #[test]
    fn unclosed_bold_passthrough() {
        let out = render_inline("hello **world");
        assert_eq!(out, "hello **world");
    }

    #[test]
    fn mixed_bold_and_code() {
        // Bold stripped, code still styled.
        let out = render_inline("**bold** and `code`");
        assert!(!out.contains(BOLD));
        assert!(out.contains("bold"));
        assert!(out.contains(CYAN));
    }

    #[test]
    fn md_renderer_code_block() {
        let mut r = MdRenderer::new();
        // Capture: code block should set in_code_block
        r.push("```rust\n");
        assert!(r.in_code_block);
        assert_eq!(r.code_lang, "rust");
        r.push("let x = 1;\n");
        r.push("```\n");
        assert!(!r.in_code_block);
    }

    #[test]
    fn md_renderer_heading() {
        let mut r = MdRenderer::new();
        // Just ensure it doesn't panic
        r.push("# Hello World\n");
        r.push("## Sub heading\n");
        r.push("Normal text\n");
    }

    #[test]
    fn md_renderer_bullets() {
        let mut r = MdRenderer::new();
        r.push("- item one\n");
        r.push("* item two\n");
        r.push("- **bold item**\n");
    }

    #[test]
    fn md_renderer_finish_flushes_partial() {
        let mut r = MdRenderer::new();
        r.push("partial line no newline");
        assert!(!r.line_buf.is_empty());
        r.finish();
        assert!(r.line_buf.is_empty());
    }

    #[test]
    fn md_renderer_dedup_records_long_line_when_enabled() {
        let mut r = MdRenderer::new();
        r.set_dedup_enabled(true);
        r.push("This is a substantive sentence that should be remembered.\n");
        assert!(r
            .seen_lines
            .contains("This is a substantive sentence that should be remembered."));
    }

    #[test]
    fn md_renderer_dedup_skips_short_lines_when_enabled() {
        let mut r = MdRenderer::new();
        r.set_dedup_enabled(true);
        r.push("ok\n");
        r.push("Done.\n");
        // Short lines are exempt — never recorded.
        assert!(!r.seen_lines.contains("ok"));
        assert!(!r.seen_lines.contains("Done."));
    }

    #[test]
    fn md_renderer_dedup_resets_per_turn_when_enabled() {
        let mut r = MdRenderer::new();
        r.set_dedup_enabled(true);
        r.push("This is a substantive sentence that should be remembered.\n");
        assert_eq!(r.seen_lines.len(), 1);
        r.reset_turn();
        assert!(r.seen_lines.is_empty());
        assert!(r.line_buf.is_empty());
    }

    #[test]
    fn md_renderer_dedup_does_not_record_code_block_body() {
        let mut r = MdRenderer::new();
        r.set_dedup_enabled(true);
        r.push("```rust\n");
        r.push("let x = 1234567890123456789012345;\n");
        r.push("```\n");
        // The code body line is long enough but must NOT be recorded —
        // code legitimately repeats (e.g. boilerplate, tests).
        assert!(r.seen_lines.is_empty());
    }

    #[test]
    fn md_renderer_dedup_off_by_default_preserves_repeated_lines() {
        // Default constructor leaves dedup off so web-fetch summaries
        // and reasoning-model scaffolds don't lose repeated lines.
        let mut r = MdRenderer::new();
        assert!(!r.dedup_enabled());
        r.push("This is a substantive sentence that should be remembered.\n");
        r.push("This is a substantive sentence that should be remembered.\n");
        // seen_lines stays empty when dedup is off.
        assert!(r.seen_lines.is_empty());
    }
}
