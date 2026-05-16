//! Rustyline helper: slash-command completion, file-path completion,
//! and a stop-signal detector that the REPL uses before feeding lines
//! into the agent.
//!
//! All items here are `pub(super)` — repl.rs is the only consumer,
//! and the rustyline Helper marker trait has to be implemented on a
//! concrete type rather than a `dyn` box.

use std::path::{Path, PathBuf};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::Helper;

// ---------------------------------------------------------------------------
// Tab completion + input highlighting
// ---------------------------------------------------------------------------

/// Returns true when the user's entire input is an unconditional stop signal.
/// These words often get misinterpreted by models as context — we intercept
/// and rewrite them so the model can't pivot around the user's explicit "no".
pub(super) fn is_stop_signal(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "dur"
            | "hayır"
            | "yok"
            | "stop"
            | "no"
            | "cancel"
            | "iptal"
            | "tamam dur"
            | "dur bakalım"
            | "bırak"
            | "vazgeç"
            | "devam etme"
            | "yapma"
    )
}

/// All slash commands the REPL recognises, for tab completion.
pub(super) const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/cost",
    "/clear",
    "/fork",
    "/session",
    "/tree",
    "/update",
    "/stats",
    "/overthink",
    "/plan",
    "/skills",
    "/compact",
    "/dag",
    "/budget",
    "/map",
    "/insights",
    "/advisor",
    "/skill-install",
    "/skill-uninstall",
    "/skill-search",
    "/provider",
    "/providers",
    "/key",
    "/model",
    "/swarm",
    "/glm",
    "/consult",
    "/image",
    "/images",
    "/files",
    "/view",
    "/search",
    "/sessions",
    "/resume",
    "/tasks",
    "/task",
    "/btw",
    "/multi-model",
    "/perturbation",
    "/parallel",
    "/api-keys",
    "/godmode",
    "/learn",
    "/context",
    "/tokens",
    "/history",
    "/exit",
    "/quit",
];

/// Rustyline helper that provides tab completion for slash commands
/// and file paths.
pub(super) struct MetisHelper {
    /// Workspace root for file path completion.
    workspace: PathBuf,
}

impl MetisHelper {
    pub(super) fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

impl Completer for MetisHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];

        // Slash command completion
        if before.starts_with('/') {
            let matches: Vec<Pair> = SLASH_COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(before))
                .map(|cmd| Pair {
                    display: cmd.to_string(),
                    replacement: cmd.to_string(),
                })
                .collect();
            return Ok((0, matches));
        }

        // File path completion: find the last whitespace-delimited word
        let word_start = before
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &before[word_start..];

        // Only complete if the word looks like a path (contains / or .)
        if !word.contains('/') && !word.contains('.') {
            return Ok((pos, Vec::new()));
        }

        let (dir, prefix) = if let Some(slash_pos) = word.rfind('/') {
            let dir_part = &word[..=slash_pos];
            let file_part = &word[slash_pos + 1..];
            let dir_path = if dir_part.starts_with('/') {
                PathBuf::from(dir_part)
            } else {
                self.workspace.join(dir_part)
            };
            (dir_path, file_part.to_string())
        } else {
            (self.workspace.clone(), word.to_string())
        };

        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) && !name.starts_with('.') {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let replacement = if is_dir {
                        format!("{name}/")
                    } else {
                        name.clone()
                    };
                    matches.push(Pair {
                        display: name,
                        replacement,
                    });
                }
            }
        }
        matches.sort_by(|a, b| a.display.cmp(&b.display));
        Ok((word_start + word.len() - prefix.len(), matches))
    }
}

impl Hinter for MetisHelper {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for MetisHelper {}
impl Validator for MetisHelper {}
impl Helper for MetisHelper {}
