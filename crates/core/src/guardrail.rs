//! Output guardrail — regex banlist applied to model-generated text.
//!
//! Why this exists: reasoning models (Qwen, DeepSeek V4, Kimi) sometimes
//! hallucinate hostile or politically dangerous content unrelated to the
//! prompt. In Turkey specifically, anything that maps onto TCK 299
//! ("insulting the President") is a real legal exposure even though
//! the user didn't author it. The session jsonl on disk is a record
//! that can hurt the user; the live TUI render is what they show
//! colleagues. We need to keep both clean.
//!
//! Mechanism: a small list of regex patterns is compiled at startup;
//! any model-generated text matching one is treated as `Verdict::Block`.
//! The agent loop converts a block into `AgentError::GuardrailBlocked`,
//! and the session store also drops blocked messages on append as a
//! defense-in-depth layer. With no banlist file present, the guardrail
//! is a no-op and Aegis behaves exactly as before.
//!
//! File format (TOML at `~/.aegis/banlist.toml` or
//! `<workspace>/.aegis/banlist.toml`):
//! ```toml
//! patterns = [
//!   "(?i)cumhurbaşkan",
//!   "(?i)\\borospu\\b",
//! ]
//! ```
//!
//! Patterns are case-sensitive Rust regex by default; use the `(?i)`
//! flag inline for case-insensitive matching. Word boundaries (`\\b`)
//! are recommended to avoid false positives on substrings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Text is clean — pass through untouched.
    Allow,
    /// Text matched at least one banned pattern. Caller should drop the
    /// message (don't render, don't persist) and surface the matched
    /// pattern to the user so they know which rule fired.
    Block(String),
}

#[derive(Debug, Error)]
pub enum GuardrailError {
    #[error("banlist file unreadable: {0}")]
    Io(#[from] std::io::Error),
    #[error("banlist toml parse failed: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("banlist pattern `{pattern}` is not a valid regex: {source}")]
    BadPattern {
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

#[derive(Debug, Deserialize, Default)]
struct BanlistFile {
    #[serde(default)]
    patterns: Vec<String>,
}

/// Compiled banlist, ready for `check()`. Cheap to clone (`Arc` over the
/// regex set), so the agent loop can hold one and pass refs into hooks
/// without re-parsing the file on every turn.
#[derive(Debug, Clone, Default)]
pub struct Guardrail {
    patterns: Arc<Vec<(String, Regex)>>,
}

impl Guardrail {
    /// Empty guardrail — `check()` always returns `Allow`. Used when no
    /// banlist file exists, so consumers can wire the call site
    /// unconditionally and pay zero cost in the common case.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load the first existing banlist from `paths`, in order. Missing
    /// files are silently skipped — that is the "no banlist configured"
    /// path. If multiple files exist, only the first is loaded; later
    /// paths are intentionally ignored so a user-global banlist can be
    /// overridden by a project-local one (or vice versa, depending on
    /// caller's order).
    pub fn from_paths<P: AsRef<Path>>(paths: &[P]) -> Result<Self, GuardrailError> {
        for p in paths {
            let p = p.as_ref();
            if !p.is_file() {
                continue;
            }
            let body = std::fs::read_to_string(p)?;
            let parsed: BanlistFile = toml::from_str(&body)?;
            let mut compiled: Vec<(String, Regex)> = Vec::with_capacity(parsed.patterns.len());
            for raw in parsed.patterns {
                let re = Regex::new(&raw).map_err(|source| GuardrailError::BadPattern {
                    pattern: raw.clone(),
                    source,
                })?;
                compiled.push((raw, re));
            }
            return Ok(Self {
                patterns: Arc::new(compiled),
            });
        }
        Ok(Self::empty())
    }

    /// Number of compiled patterns. Lets the caller log "guardrail
    /// loaded with N patterns" at startup or short-circuit when zero.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Check `text` against every pattern. Returns the *first* matching
    /// pattern as the block reason — the caller usually only needs to
    /// know the rule fired, not the full set, and stopping early keeps
    /// big assistant messages cheap.
    pub fn check(&self, text: &str) -> Verdict {
        if self.patterns.is_empty() || text.is_empty() {
            return Verdict::Allow;
        }
        for (raw, re) in self.patterns.iter() {
            if re.is_match(text) {
                return Verdict::Block(raw.clone());
            }
        }
        Verdict::Allow
    }
}

/// Default search order for the banlist file: project-local first, then
/// user-global. Caller picks whether to use this or supply explicit paths.
pub fn default_banlist_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = vec![workspace.join(".metis").join("banlist.toml")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".metis").join("banlist.toml"));
    }
    paths
}

/// One-call wrapper used by every CLI surface (REPL, TUI, one-shot
/// `metis run`): load the default banlist if it exists, log to stderr on
/// any parse error, and fall back to an empty guardrail. Never returns
/// an error — a broken banlist must not prevent Metis from starting.
pub fn load_default(workspace: &Path) -> Guardrail {
    let paths = default_banlist_paths(workspace);
    match Guardrail::from_paths(&paths) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[aegis] banlist load failed, using empty guardrail: {e}");
            Guardrail::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_banlist(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn empty_guardrail_passes_everything_through() {
        let g = Guardrail::empty();
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
        assert_eq!(g.check(""), Verdict::Allow);
        assert_eq!(g.check("anything goes"), Verdict::Allow);
    }

    #[test]
    fn missing_files_yield_empty_guardrail() {
        let dir = TempDir::new().unwrap();
        let g = Guardrail::from_paths(&[dir.path().join("nope.toml")]).unwrap();
        assert!(g.is_empty());
    }

    #[test]
    fn first_existing_path_wins() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        write_banlist(b.path(), "banlist.toml", "patterns = [\"only-in-b\"]");
        let g = Guardrail::from_paths(&[
            a.path().join("banlist.toml"), // doesn't exist
            b.path().join("banlist.toml"),
        ])
        .unwrap();
        assert_eq!(g.len(), 1);
        match g.check("see only-in-b in this text long enough to match") {
            Verdict::Block(p) => assert_eq!(p, "only-in-b"),
            v => panic!("expected block, got {v:?}"),
        }
    }

    #[test]
    fn case_insensitive_pattern_catches_mixed_case() {
        let dir = TempDir::new().unwrap();
        let path = write_banlist(
            dir.path(),
            "banlist.toml",
            "patterns = [\"(?i)cumhurbaşkan\"]",
        );
        let g = Guardrail::from_paths(&[path]).unwrap();
        assert!(matches!(
            g.check("Cumhurbaşkanı orospu çocuğu!"),
            Verdict::Block(_)
        ));
        assert!(matches!(
            g.check("CUMHURBAŞKANLIĞINDAN geçtin aq?"),
            Verdict::Block(_)
        ));
        // Unrelated text passes.
        assert_eq!(
            g.check("hello world this is fine and very normal"),
            Verdict::Allow
        );
    }

    #[test]
    fn invalid_regex_is_reported_with_source_pattern() {
        let dir = TempDir::new().unwrap();
        let path = write_banlist(dir.path(), "banlist.toml", "patterns = [\"[unclosed\"]");
        let err = Guardrail::from_paths(&[path]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[unclosed"), "got: {msg}");
    }

    #[test]
    fn first_matching_pattern_is_returned_as_block_reason() {
        let dir = TempDir::new().unwrap();
        let path = write_banlist(
            dir.path(),
            "banlist.toml",
            "patterns = [\"alpha\", \"beta\"]",
        );
        let g = Guardrail::from_paths(&[path]).unwrap();
        match g.check("text mentioning alpha and beta both") {
            Verdict::Block(r) => assert_eq!(r, "alpha", "first pattern wins"),
            v => panic!("expected block, got {v:?}"),
        }
    }

    #[test]
    fn empty_text_is_always_allowed() {
        let dir = TempDir::new().unwrap();
        let path = write_banlist(dir.path(), "banlist.toml", "patterns = [\".*\"]");
        let g = Guardrail::from_paths(&[path]).unwrap();
        // ".*" would match empty, but we short-circuit before hitting
        // the regex — empty text is never blocked.
        assert_eq!(g.check(""), Verdict::Allow);
    }

    #[test]
    fn default_paths_orders_project_before_user() {
        let dir = TempDir::new().unwrap();
        let paths = default_banlist_paths(dir.path());
        // Always ≥1 entry (project), and project entry comes first.
        assert!(!paths.is_empty());
        assert!(paths[0].starts_with(dir.path()));
        assert!(paths[0].ends_with(".metis/banlist.toml"));
    }
}
