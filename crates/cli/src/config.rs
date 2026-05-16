//! Configuration file support for `goblin`.
//!
//! Two config files are loaded and merged (workspace overrides global):
//!
//! 1. `~/.aegis/config.toml` — global defaults
//! 2. `<workspace>/.aegis/config.toml` — per-project overrides
//!
//! CLI flags always take precedence over both. The intent is that you
//! set your preferred provider/model once and forget about it.
//!
//! Example `config.toml`:
//!
//! ```toml
//! provider = "deepseek"
//! model = "deepseek-chat"
//! temperature = 0.0
//! max_tokens = 8192
//! context_window = 64000
//! keep_tail = 12
//! smart_compaction = true
//! sandbox = "sandbox-exec"
//! yes = false
//! mcp = ["mcp-obsidian ~/Documents/Vault"]
//! ```

use crate::router::RoutingConfig;
use serde::Deserialize;
use std::path::Path;

/// Parsed config file. Every field is optional — missing fields keep
/// the CLI default.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MetisConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Fallback model slugs tried in order when the primary model produces
    /// no patch. Format: `provider:alias` or full model id. CLI
    /// `--fallback-model` flags prepend to this list.
    #[serde(default)]
    pub fallback_models: Vec<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub keep_tail: Option<usize>,
    pub smart_compaction: Option<bool>,
    pub sandbox: Option<String>,
    /// Wall-clock budget for `bash`, `glob`, and similar long-running
    /// tools, in seconds. Default is 120 (matches Claude Code's
    /// 2-minute default). Hard cap 600 (10 min).
    pub tool_timeout_secs: Option<u64>,
    pub yes: Option<bool>,
    /// Show a colored unified diff before running `edit_file`, `multi_edit`,
    /// or `write_file`. Defaults to `true` — set to `false` to restore the
    /// pre-v0.8 "just the args" prompt.
    pub edit_diff_preview: Option<bool>,
    /// Daily budget in USD. When set, metis prints the running total at
    /// startup and via `/budget`, so long autonomous runs can't
    /// surprise-bill you. Purely informational in v0.9.0 — no hard stop.
    pub daily_budget_usd: Option<f64>,
    /// Opt-in hard stop at the daily budget ceiling. When `true` AND
    /// `daily_budget_usd` is set, each REPL turn checks `prior +
    /// session` cost and — if over the ceiling — prompts `[y] continue /
    /// [a] always / [n] stop` before the turn runs. `a` remembers
    /// "continue" for the rest of the session. Non-TTY invocations bail
    /// with an error so scripted runs cannot accidentally burn past the
    /// cap. Default `false` to keep pre-v0.10 behaviour.
    pub budget_hard_stop: Option<bool>,
    /// Auto-model routing configuration.
    pub routing: Option<RoutingConfig>,
    /// Auto-commit edits to git after each file change.
    pub auto_commit: Option<bool>,
    /// Enable autonomous security layer (kill switches, resource limits).
    pub autonomous_security: Option<bool>,
    /// Auto-classify queries and apply optimal temperature/top_p.
    pub autotune: Option<bool>,
    /// Auto-fix: run lint/test after edits and retry on failure.
    pub auto_fix: Option<AutoFixConfig>,
    /// Run LLM-based memory extraction on session exit. Default: true.
    pub auto_memory: Option<bool>,
    /// Minimum user turns before auto-memory fires. Default: 3.
    pub auto_memory_min_turns: Option<usize>,
    /// MCP servers to start automatically. Each entry is a command string
    /// (same format as `--mcp`), e.g. `"mcp-obsidian ~/Documents/Vault"`.
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Capture mouse events into the TUI for in-panel drag/wheel scroll.
    /// `Some(true)` (default when unset) gives aegis the wheel/drag so
    /// help/picker overlays scroll with the mouse. `Some(false)` lets
    /// the host terminal own gestures — needed for Termius and other
    /// mobile terminals where capture conflicts with native tut-kaydır.
    /// Either way, keyboard scroll (↑↓ PgUp/PgDn Home/End) keeps working.
    pub mouse_capture: Option<bool>,
    /// Atakan: session başında uygulanacak permission mod.
    /// Geçerli değerler: "default" | "accept-edits" | "plan" | "bypass".
    /// Yok ise "default" varsayılan. Shift+Tab ile runtime'da
    /// değiştirilebilir; bu ayar sadece ilk başlangıcı etkiler.
    pub default_permission_mode: Option<String>,
    /// API keys for providers. Keys are env var names (e.g. `OPENAI_API_KEY`),
    /// values are the keys. Loaded at startup and set as env vars if not
    /// already set, so they work alongside the built-in provider lookup.
    ///
    /// Example `config.toml`:
    /// ```toml
    /// [api_keys]
    /// OPENAI_API_KEY = "sk-..."
    /// ANTHROPIC_API_KEY = "sk-ant-..."
    /// ```
    #[serde(default)]
    pub api_keys: std::collections::HashMap<String, String>,
    /// Advanced features configuration.
    #[serde(default)]
    pub features: Option<FeaturesConfig>,
    /// Rego policy gate. When `policy_dirs` is non-empty AND the binary
    /// was built with `--features policy`, the agent loop wraps its
    /// permission stack with `RegoPermission`. Each directory is scanned
    /// for `*.rego` files; missing dirs are silently skipped.
    /// Default: empty (no policy active, exact pre-policy behaviour).
    #[serde(default)]
    pub policy_dirs: Vec<String>,
    /// Custom status-line command (CC pattern). When set, the TUI
    /// recap row pipes a small JSON payload to this command on stdin
    /// and uses the first line of its stdout as the footer. Runs on a
    /// throttled worker thread (≤1s wall-clock cap) so slow scripts
    /// can't block the renderer. Unset → default model/cost recap.
    pub status_line: Option<String>,
}

impl MetisConfig {
    /// Merge `other` on top of `self` — non-None fields in `other` win.
    pub fn merge(self, other: MetisConfig) -> MetisConfig {
        MetisConfig {
            provider: other.provider.or(self.provider),
            model: other.model.or(self.model),
            fallback_models: if other.fallback_models.is_empty() {
                self.fallback_models
            } else {
                other.fallback_models
            },
            temperature: other.temperature.or(self.temperature),
            max_tokens: other.max_tokens.or(self.max_tokens),
            context_window: other.context_window.or(self.context_window),
            keep_tail: other.keep_tail.or(self.keep_tail),
            smart_compaction: other.smart_compaction.or(self.smart_compaction),
            sandbox: other.sandbox.or(self.sandbox),
            tool_timeout_secs: other.tool_timeout_secs.or(self.tool_timeout_secs),
            yes: other.yes.or(self.yes),
            edit_diff_preview: other.edit_diff_preview.or(self.edit_diff_preview),
            daily_budget_usd: other.daily_budget_usd.or(self.daily_budget_usd),
            budget_hard_stop: other.budget_hard_stop.or(self.budget_hard_stop),
            routing: match (self.routing, other.routing) {
                (Some(a), Some(b)) => Some(a.merge(b)),
                (a, b) => b.or(a),
            },
            auto_commit: other.auto_commit.or(self.auto_commit),
            autonomous_security: other.autonomous_security.or(self.autonomous_security),
            autotune: other.autotune.or(self.autotune),
            auto_fix: other.auto_fix.or(self.auto_fix),
            auto_memory: other.auto_memory.or(self.auto_memory),
            auto_memory_min_turns: other.auto_memory_min_turns.or(self.auto_memory_min_turns),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
            default_permission_mode: other
                .default_permission_mode
                .or(self.default_permission_mode),
            mcp: if other.mcp.is_empty() {
                self.mcp
            } else {
                // Workspace MCP list replaces global, not appends —
                // keeps behavior predictable.
                other.mcp
            },
            api_keys: {
                let mut merged = self.api_keys;
                merged.extend(other.api_keys);
                merged
            },
            features: other.features.or(self.features),
            policy_dirs: if other.policy_dirs.is_empty() {
                self.policy_dirs
            } else {
                other.policy_dirs
            },
            status_line: other.status_line.or(self.status_line),
        }
    }
}

/// Configuration for automatic lint/test after edits.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AutoFixConfig {
    /// Lint command to run after edits (e.g. "cargo clippy 2>&1").
    pub lint_command: Option<String>,
    /// Test command to run after edits (e.g. "cargo test 2>&1").
    pub test_command: Option<String>,
    /// Maximum auto-fix retries before giving up.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 {
    3
}

/// Advanced features configuration.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FeaturesConfig {
    /// Enable multi-model evaluation (ULTRAPLINIAN-like).
    #[serde(default)]
    pub multi_model_evaluation: bool,

    /// Enable prompt perturbation for testing (Parseltongue-like).
    #[serde(default)]
    pub prompt_perturbation: bool,

    /// Enable parallel model execution (GODMODE CLASSIC-like).
    #[serde(default)]
    pub parallel_models: bool,

    /// Enable output normalization (STM Modules-like).
    #[serde(default)]
    pub output_normalization: bool,

    /// Enable adaptive sampling (AutoTune-like).
    #[serde(default)]
    pub adaptive_sampling: bool,

    /// Enable UI themes.
    #[serde(default)]
    pub themes: bool,

    /// Enable Easter eggs (fun hidden features).
    #[serde(default)]
    pub easter_eggs: bool,

    /// Maximum number of parallel models (if parallel_models enabled).
    #[serde(default = "default_max_parallel_models")]
    pub max_parallel_models: u32,

    /// Require confirmation for red-teaming features.
    #[serde(default = "default_true")]
    pub require_confirmation: bool,

    /// Maximum budget for multi-model calls in USD.
    #[serde(default = "default_multi_model_budget")]
    pub multi_model_budget_usd: f64,
}

fn default_max_parallel_models() -> u32 {
    3
}

fn default_true() -> bool {
    true
}

fn default_multi_model_budget() -> f64 {
    5.0
}

/// Try to parse a config.toml from a directory. Returns default if the
/// file doesn't exist or can't be parsed (non-fatal).
fn load_from_dir(dir: &Path) -> MetisConfig {
    let path = dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|err| {
            eprintln!("[goblin] warning: could not parse {}: {err}", path.display());
            MetisConfig::default()
        }),
        Err(_) => MetisConfig::default(),
    }
}

/// Load merged config: global (~/.metis/) then workspace (.metis/).
/// Returns default if neither file exists.
pub fn load_config(workspace: &Path) -> MetisConfig {
    let global = dirs::home_dir()
        .map(|h| load_from_dir(&h.join(".metis")))
        .unwrap_or_default();
    let local = load_from_dir(&workspace.join(".metis"));
    global.merge(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_prefers_right() {
        let base = MetisConfig {
            provider: Some("deepseek".into()),
            model: Some("deepseek-v4-flash".into()),
            ..Default::default()
        };
        let over = MetisConfig {
            model: Some("gpt-4o".into()),
            temperature: Some(0.5),
            ..Default::default()
        };
        let merged = base.merge(over);
        assert_eq!(merged.provider.as_deref(), Some("deepseek"));
        assert_eq!(merged.model.as_deref(), Some("gpt-4o"));
        assert_eq!(merged.temperature, Some(0.5));
    }

    #[test]
    fn load_missing_dir_returns_default() {
        let cfg = load_from_dir(Path::new("/nonexistent/path"));
        assert!(cfg.provider.is_none());
    }
}
