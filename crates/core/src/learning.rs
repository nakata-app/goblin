//! Cross-session learning — extract patterns from completed sessions
//! and inject them as context hints into future sessions.
//!
//! Insights are stored in `~/.aegis/learned.jsonl` — one JSON line per
//! insight. Each insight has a workspace scope (or global) and a
//! relevance score that combines reinforcement count, time decay, and
//! the success/failure ratio of past outcomes tied to the insight.
//!
//! The system prompt builder calls [`load_relevant_insights`] to inject
//! the most relevant hints for the current workspace.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 30-day time-decay half-life-ish constant (insights lose ~1/e of their
/// weight every 30 days since `last_seen`). Tuneable.
const DECAY_DAYS: f64 = 30.0;

/// Words that, when the user utters them, count as negative feedback on
/// whatever the agent most recently proposed. Mirrors the REPL stop set.
const STOP_WORDS: &[&str] = &["dur", "hayır", "hayir", "stop", "no", "iptal", "cancel"];

/// A single learned insight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insight {
    /// When the insight was first recorded (ISO 8601).
    pub timestamp: String,
    /// Workspace this insight applies to (None = global).
    pub workspace: Option<String>,
    /// Category: "tool_pattern", "error_recovery", "preference", "project_note".
    pub category: String,
    /// The insight text to inject into the system prompt.
    pub text: String,
    /// How many distinct sessions have produced this insight.
    pub reinforcements: u32,
    /// When the insight was most recently seen or reinforced.
    #[serde(default)]
    pub last_seen: Option<String>,
    /// Times an action aligned with this insight succeeded.
    #[serde(default)]
    pub success_count: u32,
    /// Times the user rejected or the action failed in this insight's scope.
    #[serde(default)]
    pub failure_count: u32,
    /// Free-form tags for grouping (e.g. "rust", "tests", "git").
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Insight {
    /// Combined relevance score. Higher = more useful right now.
    ///
    /// `reinforcements * decay(age) * laplace(success_ratio)`
    ///
    /// - Decay: `exp(-age_days / DECAY_DAYS)`
    /// - Success ratio uses Laplace smoothing so an insight with zero
    ///   feedback starts at 0.5, not 1.0 — gives feedback-laden
    ///   insights a measurable edge.
    pub fn score(&self, now_secs: i64) -> f64 {
        let anchor = self.last_seen.as_deref().unwrap_or(self.timestamp.as_str());
        let last = crate::telemetry::iso8601_to_unix(anchor);
        let age_days = ((now_secs - last).max(0)) as f64 / 86400.0;
        let decay = (-age_days / DECAY_DAYS).exp();
        let total = self.success_count + self.failure_count;
        let success_ratio = (self.success_count as f64 + 1.0) / (total as f64 + 2.0); // Laplace
        (self.reinforcements as f64).max(1.0) * decay * success_ratio
    }
}

/// Path to the global learned insights file.
pub fn insights_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".metis").join("learned.jsonl"))
}

/// Path to the global preferences (active rating) log.
pub fn preferences_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".metis").join("preferences.jsonl"))
}

/// Active rating signal recorded via `/rate`. One entry per
/// invocation, appended to `~/.metis/preferences.jsonl`. The
/// aggregator scans these to surface style preferences without ever
/// retraining a model — it is pure context injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preference {
    pub timestamp: String,
    pub workspace: Option<String>,
    pub session_id: Option<String>,
    /// "good" or "bad".
    pub signal: String,
    /// Optional free-form explanation from the user.
    #[serde(default)]
    pub note: Option<String>,
    /// Stable hash of the assistant content the user is rating.
    /// Lets the aggregator deduplicate ratings of the same answer.
    #[serde(default)]
    pub assistant_hash: Option<String>,
    /// Tool names invoked in the turns leading up to this rating.
    /// The aggregator correlates negative ratings with tools to
    /// surface "user dislikes when X gets used here" patterns.
    #[serde(default)]
    pub recent_tools: Vec<String>,
}

/// FNV-1a 64-bit — small, deterministic, no dependency. Stable across
/// runs which is all the aggregator needs.
fn stable_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Append a preference to the given path. Creates parent dirs.
pub fn record_rating_at(path: &Path, pref: &Preference) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let line = serde_json::to_string(pref).map_err(|e| format!("json: {e}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open: {e}"))?;
    use std::io::Write;
    writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Default-path variant. Returns Err if home directory is unknown.
pub fn record_rating(pref: &Preference) -> Result<(), String> {
    let path = preferences_path().ok_or("could not determine home directory")?;
    record_rating_at(&path, pref)
}

/// Build a Preference for the current state. Pulls the last assistant
/// message text + recent tool names from a transcript slice.
pub fn build_rating(
    workspace: &Path,
    session_id: Option<String>,
    signal: &str,
    note: Option<String>,
    messages: &[aegis_api::ChatMessage],
) -> Preference {
    // Hash the last assistant turn so the aggregator can dedup repeated
    // ratings of the same answer. Fallback chain:
    //   1. assistant.content (the common case)
    //   2. assistant.tool_calls (turn was a pure tool call → still distinct)
    //   3. None — only when there's no assistant message at all
    // This keeps tool-call-only turns dedupable instead of all hashing
    // identically to None and inflating bad-tool counts.
    let assistant_hash = messages
        .iter()
        .rev()
        .find(|m| m.role == aegis_api::Role::Assistant)
        .map(|m| {
            if let Some(c) = m.content.as_deref().filter(|s| !s.is_empty()) {
                stable_hash(c)
            } else if !m.tool_calls.is_empty() {
                let joined: String = m
                    .tool_calls
                    .iter()
                    .map(|tc| format!("{}({})", tc.function.name, tc.function.arguments))
                    .collect::<Vec<_>>()
                    .join("|");
                stable_hash(&joined)
            } else {
                stable_hash("")
            }
        });

    // Recent tools: walk backwards, collect distinct tool names from the
    // last few tool messages (cap at 8 to avoid unbounded growth).
    let mut tools: Vec<String> = Vec::new();
    for m in messages.iter().rev() {
        if m.role == aegis_api::Role::Tool {
            if let Some(n) = &m.name {
                if !n.is_empty() && !tools.contains(n) {
                    tools.push(n.clone());
                    if tools.len() >= 8 {
                        break;
                    }
                }
            }
        }
    }

    Preference {
        timestamp: crate::telemetry::now_iso8601(),
        workspace: Some(workspace.display().to_string()),
        session_id,
        signal: signal.to_string(),
        note,
        assistant_hash,
        recent_tools: tools,
    }
}

/// Threshold for the heuristic aggregator: a tool needs at least this
/// many distinct negative ratings (deduped on assistant_hash) before
/// the aggregator emits a style_preference insight about it.
pub const PREFERENCE_THRESHOLD: usize = 3;

/// Heuristic aggregator: scans preferences for the workspace, dedupes
/// ratings by assistant_hash (latest signal wins), counts how many
/// distinct negative ratings each tool name appears in via recent_tools,
/// and upserts a `style_preference` insight for tools at or above
/// `threshold`. Returns the insights emitted (or reinforced).
///
/// Pure heuristic — no LLM call. The intent is to surface tools that
/// correlate with user dissatisfaction so the agent can reach for an
/// alternative on its next attempt.
pub fn aggregate_preferences_at(
    prefs_path: &Path,
    insights_path: &Path,
    workspace: &Path,
    threshold: usize,
) -> Vec<Insight> {
    use std::collections::HashMap;
    let prefs = load_preferences(prefs_path);
    let ws_str = workspace.display().to_string();

    // Dedup on assistant_hash (None hashes treated as distinct).
    let mut by_hash: HashMap<String, Preference> = HashMap::new();
    let mut anonymous: Vec<Preference> = Vec::new();
    for pref in prefs {
        if pref.workspace.as_deref() != Some(&ws_str) {
            continue;
        }
        match &pref.assistant_hash {
            Some(h) => {
                by_hash.insert(h.clone(), pref);
            }
            None => anonymous.push(pref),
        }
    }
    let mut deduped: Vec<Preference> = by_hash.into_values().collect();
    deduped.extend(anonymous);

    // Count tool occurrences across negative ratings only.
    let mut bad_tool_counts: HashMap<String, u32> = HashMap::new();
    for pref in &deduped {
        if pref.signal != "bad" {
            continue;
        }
        for tool in &pref.recent_tools {
            *bad_tool_counts.entry(tool.clone()).or_insert(0) += 1;
        }
    }

    let now = crate::telemetry::now_iso8601();
    let ws_tag = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let mut emitted = Vec::new();
    let mut sorted: Vec<(String, u32)> = bad_tool_counts.into_iter().collect();
    sorted.sort(); // deterministic
    for (tool, count) in sorted {
        if (count as usize) < threshold {
            continue;
        }
        let mut tags = vec!["style_preference".to_string(), tool.clone()];
        if let Some(w) = &ws_tag {
            tags.push(w.clone());
        }
        let insight = Insight {
            timestamp: now.clone(),
            last_seen: Some(now.clone()),
            workspace: Some(ws_str.clone()),
            category: "style_preference".into(),
            text: format!(
                "User has rated {count} reply(ies) `bad` when `{tool}` was in recent context. Prefer an alternative or confirm before using.",
            ),
            reinforcements: 1,
            success_count: 0,
            failure_count: count,
            tags,
        };
        let _ = upsert_insight_at(insights_path, &insight);
        emitted.push(insight);
    }
    emitted
}

/// List all `category == "instruction"` insights scoped to `workspace`
/// (plus unscoped/global ones), sorted by recency — freshest first,
/// ties broken by reinforcement count then text. Reads from `path`.
/// Returns an empty vec if the file doesn't exist.
pub fn list_instructions_at(path: &Path, workspace: &Path) -> Vec<Insight> {
    let ws_str = workspace.display().to_string();
    let mut rules: Vec<Insight> = load_all(path)
        .into_iter()
        .filter(|i| i.category == "instruction")
        .filter(|i| i.workspace.is_none() || i.workspace.as_deref() == Some(&ws_str))
        .collect();
    rules.sort_by(|a, b| {
        let a_anchor = a.last_seen.as_deref().unwrap_or(a.timestamp.as_str());
        let b_anchor = b.last_seen.as_deref().unwrap_or(b.timestamp.as_str());
        b_anchor
            .cmp(a_anchor)
            .then_with(|| b.reinforcements.cmp(&a.reinforcements))
            .then_with(|| a.text.cmp(&b.text))
    });
    rules
}

/// Default-path variant. Uses `~/.metis/learned.jsonl`.
pub fn list_instructions(workspace: &Path) -> Vec<Insight> {
    let path = match insights_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    list_instructions_at(&path, workspace)
}

/// Default-path variant. Uses ~/.metis/preferences.jsonl and learned.jsonl.
pub fn aggregate_preferences(workspace: &Path) -> Vec<Insight> {
    let prefs = match preferences_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let insights = match insights_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    aggregate_preferences_at(&prefs, &insights, workspace, PREFERENCE_THRESHOLD)
}

/// Per-workspace rating roll-up for the `/ratings` stats view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatingSummary {
    pub good: usize,
    pub bad: usize,
    /// `(tool_name, bad_count)` sorted descending by count, ties by name.
    pub bad_tools: Vec<(String, u32)>,
    /// Threshold the aggregator uses to surface a `style_preference` insight.
    pub threshold: usize,
}

/// Summarize ratings for a workspace from the given prefs file.
pub fn summarize_ratings_at(
    prefs_path: &Path,
    workspace: &Path,
    threshold: usize,
) -> RatingSummary {
    use std::collections::HashMap;
    let prefs = load_preferences(prefs_path);
    let ws_str = workspace.display().to_string();

    let mut good = 0usize;
    let mut bad = 0usize;
    let mut bad_tool_counts: HashMap<String, u32> = HashMap::new();
    for pref in prefs {
        if pref.workspace.as_deref() != Some(&ws_str) {
            continue;
        }
        match pref.signal.as_str() {
            "good" => good += 1,
            "bad" => {
                bad += 1;
                for tool in &pref.recent_tools {
                    *bad_tool_counts.entry(tool.clone()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }

    let mut bad_tools: Vec<(String, u32)> = bad_tool_counts.into_iter().collect();
    bad_tools.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    RatingSummary {
        good,
        bad,
        bad_tools,
        threshold,
    }
}

/// Default-path variant. Uses `~/.metis/preferences.jsonl`.
pub fn summarize_ratings(workspace: &Path) -> RatingSummary {
    let prefs = match preferences_path() {
        Some(p) => p,
        None => {
            return RatingSummary {
                threshold: PREFERENCE_THRESHOLD,
                ..Default::default()
            }
        }
    };
    summarize_ratings_at(&prefs, workspace, PREFERENCE_THRESHOLD)
}

/// Remove the most-recent preference for `workspace` from `prefs_path`.
/// Returns the removed entry, or None if the workspace has no ratings.
/// Other workspaces are untouched.
pub fn undo_last_rating_at(
    prefs_path: &Path,
    workspace: &Path,
) -> Result<Option<Preference>, String> {
    let prefs = load_preferences(prefs_path);
    if prefs.is_empty() {
        return Ok(None);
    }
    let ws_str = workspace.display().to_string();

    // Walk from the end; the first match is the most recent.
    let mut idx_to_remove: Option<usize> = None;
    for (i, p) in prefs.iter().enumerate().rev() {
        if p.workspace.as_deref() == Some(&ws_str) {
            idx_to_remove = Some(i);
            break;
        }
    }
    let Some(idx) = idx_to_remove else {
        return Ok(None);
    };

    let mut kept = prefs;
    let removed = kept.remove(idx);

    // Rewrite atomically by truncating + re-appending.
    let lines: Vec<String> = kept
        .iter()
        .filter_map(|p| serde_json::to_string(p).ok())
        .collect();
    let body = if lines.is_empty() {
        String::new()
    } else {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };
    if let Some(parent) = prefs_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(prefs_path, body).map_err(|e| format!("write: {e}"))?;
    Ok(Some(removed))
}

/// Default-path variant. Uses `~/.metis/preferences.jsonl`.
pub fn undo_last_rating(workspace: &Path) -> Result<Option<Preference>, String> {
    let path = preferences_path().ok_or("could not determine home directory")?;
    undo_last_rating_at(&path, workspace)
}

/// Remove insights for `workspace` whose text or tags contain `needle`
/// (case-insensitive substring). Returns the removed insights.
/// Other workspaces are untouched. The file is rewritten in place.
pub fn forget_insights_at(
    insights_path: &Path,
    workspace: &Path,
    needle: &str,
) -> Result<Vec<Insight>, String> {
    let needle_lc = needle.to_lowercase();
    if needle_lc.is_empty() {
        return Ok(Vec::new());
    }
    let all = load_all(insights_path);
    let ws_str = workspace.display().to_string();

    let mut kept = Vec::with_capacity(all.len());
    let mut removed = Vec::new();
    for ins in all {
        let same_ws = ins.workspace.as_deref() == Some(&ws_str);
        let text_hit = ins.text.to_lowercase().contains(&needle_lc);
        let tag_hit = ins
            .tags
            .iter()
            .any(|t| t.to_lowercase().contains(&needle_lc));
        if same_ws && (text_hit || tag_hit) {
            removed.push(ins);
        } else {
            kept.push(ins);
        }
    }

    if removed.is_empty() {
        return Ok(removed);
    }

    let lines: Vec<String> = kept
        .iter()
        .filter_map(|i| serde_json::to_string(i).ok())
        .collect();
    let body = if lines.is_empty() {
        String::new()
    } else {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };
    if let Some(parent) = insights_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(insights_path, body).map_err(|e| format!("write: {e}"))?;
    Ok(removed)
}

/// Default-path variant. Uses `~/.metis/learned.jsonl`.
pub fn forget_insights(workspace: &Path, needle: &str) -> Result<Vec<Insight>, String> {
    let path = insights_path().ok_or("could not determine home directory")?;
    forget_insights_at(&path, workspace, needle)
}

/// Load all preferences from the given path. Skips malformed lines.
pub fn load_preferences(path: &Path) -> Vec<Preference> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Append an insight to disk (no dedup — see [`upsert_insight`] for the
/// dedup-aware version).
pub fn save_insight(insight: &Insight) -> Result<(), String> {
    let path = insights_path().ok_or("could not determine home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let line = serde_json::to_string(insight).map_err(|e| format!("json: {e}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open: {e}"))?;
    use std::io::Write;
    writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Merge an insight into storage at the given path. If an existing
/// insight matches on (workspace, category, text) it is reinforced in
/// place (bump `reinforcements`, refresh `last_seen`, inherit any new
/// tags); otherwise the insight is appended as new. The whole file is
/// rewritten on merge, which is fine for the expected scale (insights
/// number in the hundreds, not millions).
pub fn upsert_insight_at(path: &Path, insight: &Insight) -> Result<(), String> {
    let mut all = load_all(path);

    if let Some(existing) = all.iter_mut().find(|i| {
        i.workspace == insight.workspace && i.category == insight.category && i.text == insight.text
    }) {
        existing.reinforcements = existing.reinforcements.saturating_add(1);
        existing.last_seen = Some(
            insight
                .last_seen
                .clone()
                .unwrap_or_else(crate::telemetry::now_iso8601),
        );
        for tag in &insight.tags {
            if !existing.tags.contains(tag) {
                existing.tags.push(tag.clone());
            }
        }
        existing.success_count = existing.success_count.saturating_add(insight.success_count);
        existing.failure_count = existing.failure_count.saturating_add(insight.failure_count);
        rewrite_all(path, &all)
    } else {
        append_one(path, insight)
    }
}

/// Default-path variant of [`upsert_insight_at`].
pub fn upsert_insight(insight: &Insight) -> Result<(), String> {
    let path = insights_path().ok_or("could not determine home directory")?;
    upsert_insight_at(&path, insight)
}

/// Apply feedback to all insights whose `tags` contain the given tag
/// and whose workspace matches (or is global). Returns number of
/// insights touched. Use with tool names or other coarse markers to
/// reinforce or penalise existing insights without knowing exact text.
pub fn record_feedback_by_tag_at(
    path: &Path,
    workspace: &Path,
    tag: &str,
    positive: bool,
) -> usize {
    let mut all = load_all(path);
    let ws_str = workspace.display().to_string();
    let mut touched = 0usize;
    let now = crate::telemetry::now_iso8601();
    for insight in all.iter_mut() {
        let matches_ws =
            insight.workspace.is_none() || insight.workspace.as_deref() == Some(&ws_str);
        if matches_ws && insight.tags.iter().any(|t| t == tag) {
            if positive {
                insight.success_count = insight.success_count.saturating_add(1);
            } else {
                insight.failure_count = insight.failure_count.saturating_add(1);
            }
            insight.last_seen = Some(now.clone());
            touched += 1;
        }
    }
    if touched > 0 {
        let _ = rewrite_all(path, &all);
    }
    touched
}

/// Default-path variant of [`record_feedback_by_tag_at`].
pub fn record_feedback_by_tag(workspace: &Path, tag: &str, positive: bool) -> usize {
    match insights_path() {
        Some(p) => record_feedback_by_tag_at(&p, workspace, tag, positive),
        None => 0,
    }
}

/// Apply feedback to the insight(s) matching a given text and workspace
/// at the provided path. Returns the number of insights updated.
pub fn record_feedback_at(path: &Path, workspace: &Path, text: &str, positive: bool) -> usize {
    let mut all = load_all(path);
    let ws_str = workspace.display().to_string();
    let mut touched = 0usize;
    let now = crate::telemetry::now_iso8601();
    for insight in all.iter_mut() {
        let matches_ws =
            insight.workspace.is_none() || insight.workspace.as_deref() == Some(&ws_str);
        if matches_ws && insight.text == text {
            if positive {
                insight.success_count = insight.success_count.saturating_add(1);
            } else {
                insight.failure_count = insight.failure_count.saturating_add(1);
            }
            insight.last_seen = Some(now.clone());
            touched += 1;
        }
    }
    if touched > 0 {
        let _ = rewrite_all(path, &all);
    }
    touched
}

/// Default-path variant of [`record_feedback_at`].
pub fn record_feedback(workspace: &Path, text: &str, positive: bool) -> usize {
    match insights_path() {
        Some(p) => record_feedback_at(&p, workspace, text, positive),
        None => 0,
    }
}

fn append_one(path: &Path, insight: &Insight) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let line = serde_json::to_string(insight).map_err(|e| format!("json: {e}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open: {e}"))?;
    use std::io::Write;
    writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn rewrite_all(path: &Path, all: &[Insight]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut body = String::new();
    for insight in all {
        let line = serde_json::to_string(insight).map_err(|e| format!("json: {e}"))?;
        body.push_str(&line);
        body.push('\n');
    }
    std::fs::write(path, body).map_err(|e| format!("write: {e}"))
}

/// Load all insights from disk.
pub fn load_all(path: &Path) -> Vec<Insight> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Load insights relevant to a specific workspace.
/// Returns global insights + workspace-specific ones, ranked by
/// [`Insight::score`] (highest first), capped at `max`.
///
/// Internally calls [`load_relevant_insights_balanced`] with a per-category
/// cap of `max / 3` (min 2), preventing a hot category from crowding
/// out the others in the system prompt.
pub fn load_relevant_insights(workspace: &Path, max: usize) -> Vec<Insight> {
    let per_cat = (max / 3).max(2);
    load_relevant_insights_balanced(workspace, per_cat, max)
}

/// Category-balanced variant: take up to `per_category` insights per
/// category (preference, error_recovery, tool_pattern, project_note,
/// style_preference, …), then merge, re-sort by score, and truncate to
/// `max`. Categories present in the data are each guaranteed up to
/// `per_category` slots so that one category cannot crowd out others.
pub fn load_relevant_insights_balanced(
    workspace: &Path,
    per_category: usize,
    max: usize,
) -> Vec<Insight> {
    let path = match insights_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    load_relevant_insights_balanced_at(&path, workspace, per_category, max)
}

/// Path-injectable form. Used by the round-trip integration test;
/// production code goes through [`load_relevant_insights_balanced`].
pub fn load_relevant_insights_balanced_at(
    insights_path: &Path,
    workspace: &Path,
    per_category: usize,
    max: usize,
) -> Vec<Insight> {
    let all = load_all(insights_path);
    let ws_str = workspace.display().to_string();
    let now = crate::telemetry::now_unix_secs();

    let relevant: Vec<Insight> = all
        .into_iter()
        .filter(|i| i.workspace.is_none() || i.workspace.as_deref() == Some(&ws_str))
        .collect();

    // Group by category
    let mut by_cat: std::collections::BTreeMap<String, Vec<Insight>> =
        std::collections::BTreeMap::new();
    for insight in relevant {
        by_cat
            .entry(insight.category.clone())
            .or_default()
            .push(insight);
    }

    // Take top `per_category` from each
    let mut picked: Vec<Insight> = Vec::new();
    for (_cat, mut group) in by_cat {
        group.sort_by(|a, b| {
            b.score(now)
                .partial_cmp(&a.score(now))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        group.truncate(per_category);
        picked.extend(group);
    }

    // Final global sort + truncate
    picked.sort_by(|a, b| {
        b.score(now)
            .partial_cmp(&a.score(now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    picked.truncate(max);
    picked
}

/// Format insights as a system prompt section.
pub fn format_insights_section(insights: &[Insight]) -> String {
    if insights.is_empty() {
        return String::new();
    }

    // Three-way split. Order of sections (rules → dislikes → patterns)
    // reflects decreasing authority: explicit user instructions bind
    // tightest, explicit negative ratings next, pattern-mined bullets
    // last. The model sees the strongest signal first.
    let mut rules: Vec<&Insight> = Vec::new();
    let mut prefs: Vec<&Insight> = Vec::new();
    let mut rest: Vec<&Insight> = Vec::new();
    for i in insights {
        match i.category.as_str() {
            "instruction" => rules.push(i),
            "style_preference" => prefs.push(i),
            _ => rest.push(i),
        }
    }

    let mut out = String::new();

    if !rules.is_empty() {
        out.push_str("\n# User-stated rules for this workspace\n");
        out.push_str(
            "These are explicit instructions the user gave in past sessions \
             (\"always do X\", \"never do Y\", \"from now on Z\", \
             \"bundan sonra …\", \"hiç … yapma\"). Treat them as hard rules \
             unless the user overrides them this session.\n",
        );
        for insight in &rules {
            let scope = insight
                .workspace
                .as_deref()
                .map(|w| format!(" [{}]", w.rsplit('/').next().unwrap_or(w)))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}{}] {}\n",
                insight.category, scope, insight.text
            ));
        }
    }

    if !prefs.is_empty() {
        out.push_str("\n# User-rated dislikes (avoid unless explicitly asked)\n");
        out.push_str(
            "These come from the user's explicit `/rate bad` feedback. \
             Treat them as soft constraints — prefer alternatives, or \
             confirm before doing the disliked action.\n",
        );
        for insight in &prefs {
            let scope = insight
                .workspace
                .as_deref()
                .map(|w| format!(" [{}]", w.rsplit('/').next().unwrap_or(w)))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}{}] {}\n",
                insight.category, scope, insight.text
            ));
        }
    }

    if !rest.is_empty() {
        out.push_str("\n# Learned patterns from past sessions\n");
        for insight in &rest {
            let scope = insight
                .workspace
                .as_deref()
                .map(|w| format!(" [{}]", w.rsplit('/').next().unwrap_or(w)))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}{}] {}\n",
                insight.category, scope, insight.text
            ));
        }
    }

    out
}

/// Extract insights from a completed session transcript.
///
/// Patterns detected:
/// - **Recovery after retries** — tool errors N times then succeeds → insight + positive feedback
/// - **Permission denial** — tool was rejected → insight tagged `preference`
/// - **User stop words** — the user aborts after the agent proposed something → negative feedback
///
/// The function is pure; it returns insights and leaves storage to the
/// caller. Direct feedback on existing insights is handled via
/// [`record_feedback`] using the `(text, positive)` pairs embedded in the
/// returned [`Insight::success_count`] / [`failure_count`] fields.
pub fn extract_insights(messages: &[aegis_api::ChatMessage], workspace: &Path) -> Vec<Insight> {
    let mut insights = Vec::new();
    let ws = workspace.display().to_string();
    let now = crate::telemetry::now_iso8601();
    let ws_tag = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    // Pattern 1: Count tool errors and find recovery patterns
    let mut consecutive_errors = 0u32;
    let mut last_error_tool = String::new();
    for msg in messages {
        if msg.role == aegis_api::Role::Tool {
            if let Some(content) = &msg.content {
                if content.starts_with("error:") {
                    consecutive_errors += 1;
                    if let Some(name) = &msg.name {
                        last_error_tool = name.clone();
                    }
                } else if consecutive_errors >= 2 {
                    let mut tags = vec!["error_recovery".to_string()];
                    if !last_error_tool.is_empty() {
                        tags.push(last_error_tool.clone());
                    }
                    if let Some(w) = &ws_tag {
                        tags.push(w.clone());
                    }
                    insights.push(Insight {
                        timestamp: now.clone(),
                        last_seen: Some(now.clone()),
                        workspace: Some(ws.clone()),
                        category: "error_recovery".into(),
                        text: format!(
                            "Tool `{last_error_tool}` needed {consecutive_errors} retries before succeeding. Consider reading the file first."
                        ),
                        reinforcements: 1,
                        success_count: 1, // recovery itself is a positive signal
                        failure_count: 0,
                        tags,
                    });
                    consecutive_errors = 0;
                } else {
                    consecutive_errors = 0;
                }
            }
        }
    }

    // Pattern 2: Permission denials → note which tools get denied.
    for msg in messages {
        if msg.role == aegis_api::Role::Tool {
            if let Some(content) = &msg.content {
                if content.contains("permission denied") {
                    if let Some(name) = &msg.name {
                        let mut tags = vec!["preference".to_string(), name.clone()];
                        if let Some(w) = &ws_tag {
                            tags.push(w.clone());
                        }
                        insights.push(Insight {
                            timestamp: now.clone(),
                            last_seen: Some(now.clone()),
                            workspace: Some(ws.clone()),
                            category: "preference".into(),
                            text: format!(
                                "User denied `{name}` — ask before using this tool in this workspace."
                            ),
                            reinforcements: 1,
                            success_count: 0,
                            failure_count: 1, // denial = negative outcome for whatever we proposed
                            tags,
                        });
                    }
                }
            }
        }
    }

    // Pattern 3: User interruption via stop word shortly after an
    // assistant turn. If the preceding assistant message had tool_calls,
    // emit one targeted insight per tool so upsert can accumulate
    // failure_count against the specific tool that got rejected.
    // Otherwise fall back to the generic "we were off-track" hint.
    let mut stopped_tools: Vec<String> = Vec::new();
    let mut saw_generic_stop = false;
    for window in messages.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        if prev.role == aegis_api::Role::Assistant && curr.role == aegis_api::Role::User {
            if let Some(content) = &curr.content {
                let lc = content.trim().to_lowercase();
                let is_stop = STOP_WORDS
                    .iter()
                    .any(|w| lc == *w || lc.starts_with(&format!("{w} ")));
                if is_stop {
                    if !prev.tool_calls.is_empty() {
                        for tc in &prev.tool_calls {
                            let name = tc.function.name.clone();
                            if !stopped_tools.contains(&name) {
                                stopped_tools.push(name);
                            }
                        }
                    } else {
                        saw_generic_stop = true;
                    }
                }
            }
        }
    }
    for tool in &stopped_tools {
        let mut tags = vec!["stop_signal".to_string(), tool.clone()];
        if let Some(w) = &ws_tag {
            tags.push(w.clone());
        }
        insights.push(Insight {
            timestamp: now.clone(),
            last_seen: Some(now.clone()),
            workspace: Some(ws.clone()),
            category: "preference".into(),
            text: format!(
                "User stopped agent while calling `{tool}` — confirm before using this tool again."
            ),
            reinforcements: 1,
            success_count: 0,
            failure_count: 1,
            tags,
        });
    }
    if saw_generic_stop && stopped_tools.is_empty() {
        let mut tags = vec!["stop_signal".to_string()];
        if let Some(w) = &ws_tag {
            tags.push(w.clone());
        }
        insights.push(Insight {
            timestamp: now.clone(),
            last_seen: Some(now.clone()),
            workspace: Some(ws.clone()),
            category: "preference".into(),
            text: "User interrupted the agent mid-turn in this workspace — double-check ambition before long tool chains.".into(),
            reinforcements: 1,
            success_count: 0,
            failure_count: 1,
            tags,
        });
    }

    insights
}

/// Scan a session transcript and emit one net signal per tool:
/// - `(tool, true)` if the tool ran at least once without any error in
///   this session,
/// - `(tool, false)` if the tool errored at any point in this session.
///
/// Signals are deduped per session so running a tool 10 times does not
/// produce 10 signals. The caller applies these via
/// [`record_feedback_by_tag`] to update existing insights.
pub fn extract_tool_feedback(messages: &[aegis_api::ChatMessage]) -> Vec<(String, bool)> {
    use std::collections::HashMap;
    // Per-tool tracking: (any_ok, any_err)
    let mut state: HashMap<String, (bool, bool)> = HashMap::new();
    for msg in messages {
        if msg.role != aegis_api::Role::Tool {
            continue;
        }
        let name = match &msg.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => continue,
        };
        let is_err = msg
            .content
            .as_deref()
            .map(|c| c.starts_with("error:"))
            .unwrap_or(false);
        let entry = state.entry(name).or_insert((false, false));
        if is_err {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }
    let mut out = Vec::new();
    for (name, (ok, err)) in state {
        // Recovery sessions (errored then succeeded) are already handled
        // by pattern 1 in extract_insights — emit no signal here so we
        // don't double-count. Only emit for clearly-clean or
        // stuck-in-error patterns.
        match (ok, err) {
            (true, false) => out.push((name, true)),
            (false, true) => out.push((name, false)),
            _ => {}
        }
    }
    out.sort(); // deterministic ordering for tests
    out
}

/// Reject captures that are common idioms rather than rules. The check
/// runs against the lowercased, whitespace-collapsed body that follows
/// the trigger word. Prefix match is enough — blacklist entries are
/// chosen to cover "I don't know what the user meant…" style hedges.
fn is_instruction_blacklisted(lc_body: &str) -> bool {
    const SKIP_PREFIXES: &[&str] = &[
        // English idioms where the trigger carries no rule.
        "worry",
        "know",
        "think",
        "mind",
        "get me wrong",
        "believe",
        "say",
        "gonna",
        "thought",
        "been",
        "mean",
        "really",
        // Turkish/English conversational filler after "hiç/asla".
        "bir şey",
        "bir sey",
        "fikrim yok",
        "sorun değil",
        "sorun degil",
        "problem yok",
    ];
    SKIP_PREFIXES.iter().any(|p| lc_body.starts_with(p))
}

/// Pattern 4: Mine explicit user rules from transcript user messages.
/// Triggers that are stated as imperatives — "from now on X", "always
/// Y", "never Z", "don't W", "bundan sonra X", "her zaman Y",
/// "hiç/asla Z" — get captured as whole-clause instructions and
/// upserted under `category = "instruction"`. These carry more weight
/// than the pattern-mined signals because the user stated them on
/// purpose. The scoring + injection path treats them like any other
/// insight, but [`format_insights_section`] renders them in a
/// dedicated "hard rules" header so the model doesn't bury them.
///
/// Dedup is per-session (case-insensitive, whitespace-collapsed).
/// Same-rule reinforcement across sessions happens via [`upsert_insight`]
/// at the call site — identical text bumps `reinforcements`.
pub fn extract_instructions(messages: &[aegis_api::ChatMessage], workspace: &Path) -> Vec<Insight> {
    // Match a trigger at start-of-string or after a sentence terminator,
    // then capture the trigger + the clause body up to the next
    // terminator or newline. The trigger stays in the captured text so
    // the stored instruction reads naturally ("from now on always use
    // edit instead of bash"). Length floor on the body is enforced
    // after the capture to keep the regex simple and predictable.
    let re = regex::Regex::new(
        r"(?im)(?:\A|[.!?;\n]\s*)(?P<full>(?:bundan\s+sonra|her\s+zaman|her\s+seferinde|from\s+now\s+on|always|never|don'?t|do\s+not|asla|hi[çc])\b[^.!?\n]{1,240})",
    )
    .expect("instruction regex compiles");

    let ws = workspace.display().to_string();
    let now = crate::telemetry::now_iso8601();
    let ws_tag = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();

    for msg in messages {
        if msg.role != aegis_api::Role::User {
            continue;
        }
        let content = match &msg.content {
            Some(c) => c.as_str(),
            None => continue,
        };
        for cap in re.captures_iter(content) {
            let full = match cap.name("full") {
                Some(m) => m.as_str().trim(),
                None => continue,
            };
            // Require at least 10 characters total so short fragments
            // like "don't!" or "always." don't emit noise insights.
            if full.chars().count() < 10 {
                continue;
            }
            // Split once past the trigger word for the blacklist check.
            let body_lc = {
                let lc = full.to_lowercase();
                // Find the first whitespace after the trigger and take
                // what follows. The trigger itself is one of the alt
                // branches in the regex, so its length varies — easier
                // to find the first space+non-space than to know the
                // trigger boundary exactly.
                match lc.find(char::is_whitespace) {
                    Some(i) => lc[i..].trim_start().to_string(),
                    None => lc,
                }
            };
            if is_instruction_blacklisted(&body_lc) {
                continue;
            }

            // Normalize for dedup: lowercase + collapse whitespace.
            let norm: String = full
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            if !seen.insert(norm) {
                continue;
            }

            let mut tags = vec!["instruction".to_string()];
            if let Some(w) = &ws_tag {
                tags.push(w.clone());
            }
            out.push(Insight {
                timestamp: now.clone(),
                last_seen: Some(now.clone()),
                workspace: Some(ws.clone()),
                category: "instruction".into(),
                text: full.to_string(),
                reinforcements: 1,
                success_count: 0,
                failure_count: 0,
                tags,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_api::ChatMessage;

    fn fixture_insight(text: &str, reinforcements: u32) -> Insight {
        Insight {
            timestamp: "2026-04-10T12:00:00Z".into(),
            last_seen: Some("2026-04-10T12:00:00Z".into()),
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: text.into(),
            reinforcements,
            success_count: 0,
            failure_count: 0,
            tags: vec![],
        }
    }

    #[test]
    fn insight_round_trip() {
        let insight = fixture_insight("Always read before edit", 3);
        let json = serde_json::to_string(&insight).unwrap();
        let back: Insight = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text, "Always read before edit");
        assert_eq!(back.reinforcements, 3);
    }

    #[test]
    fn legacy_jsonl_still_parses() {
        // Pre-v2 insights lack last_seen/success_count/failure_count/tags.
        let legacy = r#"{"timestamp":"2026-04-10T12:00:00Z","workspace":"/tmp/x","category":"note","text":"old","reinforcements":2}"#;
        let parsed: Insight = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.reinforcements, 2);
        assert_eq!(parsed.success_count, 0);
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn score_rewards_reinforcement_and_recency() {
        let now = crate::telemetry::now_unix_secs();
        let fresh = Insight {
            timestamp: crate::telemetry::now_iso8601(),
            last_seen: Some(crate::telemetry::now_iso8601()),
            reinforcements: 5,
            ..fixture_insight("fresh", 5)
        };
        let stale = Insight {
            timestamp: "2020-01-01T00:00:00Z".into(),
            last_seen: Some("2020-01-01T00:00:00Z".into()),
            reinforcements: 5,
            ..fixture_insight("stale", 5)
        };
        assert!(
            fresh.score(now) > stale.score(now) * 10.0,
            "fresh {} should dominate stale {}",
            fresh.score(now),
            stale.score(now)
        );
    }

    #[test]
    fn score_penalises_failures() {
        let now = crate::telemetry::now_unix_secs();
        let good = Insight {
            timestamp: crate::telemetry::now_iso8601(),
            last_seen: Some(crate::telemetry::now_iso8601()),
            success_count: 10,
            failure_count: 0,
            ..fixture_insight("good", 3)
        };
        let bad = Insight {
            timestamp: crate::telemetry::now_iso8601(),
            last_seen: Some(crate::telemetry::now_iso8601()),
            success_count: 0,
            failure_count: 10,
            ..fixture_insight("bad", 3)
        };
        assert!(good.score(now) > bad.score(now) * 3.0);
    }

    #[test]
    fn format_insights_section_renders() {
        let insights = vec![
            Insight {
                workspace: None,
                text: "User prefers concise output".into(),
                reinforcements: 5,
                ..fixture_insight("User prefers concise output", 5)
            },
            Insight {
                workspace: Some("/tmp/myproject".into()),
                category: "error_recovery".into(),
                text: "edit_file often fails without read first".into(),
                reinforcements: 2,
                ..fixture_insight("edit_file often fails without read first", 2)
            },
        ];
        let section = format_insights_section(&insights);
        assert!(section.contains("Learned patterns"));
        assert!(section.contains("concise output"));
        assert!(section.contains("[myproject]"));
    }

    #[test]
    fn format_insights_empty_returns_empty() {
        assert!(format_insights_section(&[]).is_empty());
    }

    #[test]
    fn format_insights_separates_style_preferences() {
        let insights = vec![
            Insight {
                workspace: Some("/tmp/proj".into()),
                category: "style_preference".into(),
                text: "User dislikes bash being used for repo navigation".into(),
                ..fixture_insight("dislike-bash", 1)
            },
            Insight {
                workspace: Some("/tmp/proj".into()),
                category: "tool_pattern".into(),
                text: "edit_file usually needs read first".into(),
                ..fixture_insight("edit-pattern", 2)
            },
        ];
        let section = format_insights_section(&insights);
        assert!(
            section.contains("User-rated dislikes"),
            "missing prefs header: {section}"
        );
        assert!(section.contains("dislikes bash"));
        assert!(section.contains("Learned patterns from past sessions"));
        assert!(section.contains("edit_file usually"));
        // Order: prefs header must come before the general header.
        let prefs_pos = section.find("User-rated dislikes").unwrap();
        let learn_pos = section.find("Learned patterns").unwrap();
        assert!(prefs_pos < learn_pos, "prefs must come before patterns");
    }

    #[test]
    fn format_insights_only_prefs_no_general_header() {
        let insights = vec![Insight {
            category: "style_preference".into(),
            text: "x".into(),
            ..fixture_insight("x", 1)
        }];
        let section = format_insights_section(&insights);
        assert!(section.contains("User-rated dislikes"));
        assert!(!section.contains("Learned patterns from past sessions"));
    }

    #[test]
    fn format_insights_only_general_no_prefs_header() {
        let insights = vec![Insight {
            category: "tool_pattern".into(),
            text: "y".into(),
            ..fixture_insight("y", 1)
        }];
        let section = format_insights_section(&insights);
        assert!(!section.contains("User-rated dislikes"));
        assert!(section.contains("Learned patterns from past sessions"));
    }

    #[test]
    fn load_relevant_filters_by_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "metis-learn-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.jsonl");

        let insights = vec![
            Insight {
                workspace: Some("/tmp/proj-a".into()),
                text: "proj-a specific".into(),
                ..fixture_insight("proj-a specific", 1)
            },
            Insight {
                workspace: None,
                text: "global insight".into(),
                reinforcements: 2,
                ..fixture_insight("global insight", 2)
            },
            Insight {
                workspace: Some("/tmp/proj-b".into()),
                text: "proj-b specific".into(),
                ..fixture_insight("proj-b specific", 1)
            },
        ];

        let mut file = std::fs::File::create(&path).unwrap();
        for i in &insights {
            use std::io::Write;
            writeln!(file, "{}", serde_json::to_string(i).unwrap()).unwrap();
        }

        let all = load_all(&path);
        let ws_str = "/tmp/proj-a";
        let mut relevant: Vec<_> = all
            .into_iter()
            .filter(|i| i.workspace.is_none() || i.workspace.as_deref() == Some(ws_str))
            .collect();
        let now = crate::telemetry::now_unix_secs();
        relevant.sort_by(|a, b| b.score(now).partial_cmp(&a.score(now)).unwrap());

        assert_eq!(relevant.len(), 2); // global + proj-a, not proj-b
        assert_eq!(relevant[0].text, "global insight"); // higher reinforcement
        assert_eq!(relevant[1].text, "proj-a specific");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_insights_from_permission_denied() {
        let messages = vec![
            ChatMessage::user("do something"),
            ChatMessage::tool_result("id1", "bash", "error: permission denied — user said no"),
        ];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        assert!(!insights.is_empty());
        let denial = insights.iter().find(|i| i.text.contains("bash")).unwrap();
        assert_eq!(denial.category, "preference");
        assert_eq!(denial.failure_count, 1, "denial should count as failure");
        assert!(denial.tags.contains(&"bash".to_string()));
    }

    #[test]
    fn extract_insights_from_stop_word() {
        let messages = vec![
            ChatMessage::user("build the feature"),
            ChatMessage::assistant_text("I'll rewrite half the module..."),
            ChatMessage::user("dur"),
        ];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        let stop = insights
            .iter()
            .find(|i| i.text.contains("interrupted"))
            .expect("stop-word insight missing");
        assert_eq!(stop.failure_count, 1);
        assert!(stop.tags.contains(&"stop_signal".to_string()));
    }

    #[test]
    fn extract_insights_targets_stopped_tool() {
        use aegis_api::{ToolCall, ToolCallFunction};
        let mut asst = ChatMessage::assistant_text("running bash…");
        asst.tool_calls = vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{\"cmd\":\"rm -rf /\"}".into(),
            },
        }];
        let messages = vec![
            ChatMessage::user("clean up please"),
            asst,
            ChatMessage::user("dur"),
        ];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));

        // Should get tool-specific insight, NOT the generic fallback.
        let tool_specific = insights
            .iter()
            .find(|i| i.text.contains("`bash`") && i.text.contains("stopped"))
            .expect("tool-specific stop insight missing");
        assert_eq!(tool_specific.failure_count, 1);
        assert!(tool_specific.tags.contains(&"bash".to_string()));
        assert!(tool_specific.tags.contains(&"stop_signal".to_string()));

        // Generic fallback should NOT fire when we have a specific tool.
        let generic = insights.iter().find(|i| i.text.contains("interrupted"));
        assert!(
            generic.is_none(),
            "generic fallback fired even though tool was identified"
        );
    }

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "metis-learn-{}-{}-{}",
            tag,
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("learned.jsonl")
    }

    #[test]
    fn upsert_insight_dedupes_and_reinforces() {
        let path = tmp_path("upsert");
        let base = Insight {
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: "read before edit".into(),
            ..fixture_insight("read before edit", 1)
        };

        upsert_insight_at(&path, &base).unwrap();
        upsert_insight_at(&path, &base).unwrap();
        upsert_insight_at(&path, &base).unwrap();

        let all = load_all(&path);
        assert_eq!(all.len(), 1, "upsert should dedupe, got {all:?}");
        assert_eq!(all[0].reinforcements, 3);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn upsert_insight_merges_tags_and_counts() {
        let path = tmp_path("upsert-merge");
        let first = Insight {
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: "read before edit".into(),
            tags: vec!["rust".into()],
            success_count: 2,
            ..fixture_insight("read before edit", 1)
        };
        let second = Insight {
            tags: vec!["edit".into(), "rust".into()], // "rust" dupe, "edit" new
            failure_count: 1,
            ..first.clone()
        };

        upsert_insight_at(&path, &first).unwrap();
        upsert_insight_at(&path, &second).unwrap();

        let all = load_all(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].success_count, 4);
        assert_eq!(all[0].failure_count, 1);
        assert!(all[0].tags.contains(&"rust".to_string()));
        assert!(all[0].tags.contains(&"edit".to_string()));
        assert_eq!(all[0].tags.iter().filter(|t| *t == "rust").count(), 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn balanced_selection_caps_per_category() {
        let dir = std::env::temp_dir().join(format!(
            "metis-learn-balanced-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.jsonl");

        // 5 hot tool_pattern insights + 1 preference + 1 error_recovery.
        // Without balancing, the 5 hot ones would crowd out the others.
        let mut items = Vec::new();
        for n in 0..5 {
            items.push(Insight {
                workspace: None,
                category: "tool_pattern".into(),
                text: format!("tool pattern {n}"),
                reinforcements: 10,
                ..fixture_insight(&format!("tool pattern {n}"), 10)
            });
        }
        items.push(Insight {
            workspace: None,
            category: "preference".into(),
            text: "user prefers X".into(),
            reinforcements: 1,
            ..fixture_insight("user prefers X", 1)
        });
        items.push(Insight {
            workspace: None,
            category: "error_recovery".into(),
            text: "retry pattern Y".into(),
            reinforcements: 1,
            ..fixture_insight("retry pattern Y", 1)
        });

        let mut file = std::fs::File::create(&path).unwrap();
        for i in &items {
            use std::io::Write;
            writeln!(file, "{}", serde_json::to_string(i).unwrap()).unwrap();
        }

        // Simulate load_relevant_insights_balanced by using the same
        // internals (we can't override insights_path() from a test).
        let all = load_all(&path);
        let now = crate::telemetry::now_unix_secs();
        let mut by_cat: std::collections::BTreeMap<String, Vec<Insight>> =
            std::collections::BTreeMap::new();
        for insight in all {
            by_cat
                .entry(insight.category.clone())
                .or_default()
                .push(insight);
        }
        let per_category = 2;
        let mut picked: Vec<Insight> = Vec::new();
        for (_cat, mut group) in by_cat {
            group.sort_by(|a, b| b.score(now).partial_cmp(&a.score(now)).unwrap());
            group.truncate(per_category);
            picked.extend(group);
        }

        // With per_category=2: 2 tool_pattern + 1 preference + 1 error_recovery = 4 total
        assert_eq!(picked.len(), 4, "balanced pick should cap each category");
        let cats: Vec<&str> = picked.iter().map(|i| i.category.as_str()).collect();
        assert!(cats.contains(&"preference"));
        assert!(cats.contains(&"error_recovery"));
        assert_eq!(
            cats.iter().filter(|c| **c == "tool_pattern").count(),
            2,
            "tool_pattern should be capped at 2, not 5"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_rating_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "metis-rate-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preferences.jsonl");

        let pref = Preference {
            timestamp: "2026-04-20T12:00:00Z".into(),
            workspace: Some("/tmp/proj".into()),
            session_id: Some("sess-1".into()),
            signal: "bad".into(),
            note: Some("too verbose".into()),
            assistant_hash: Some("abc123".into()),
            recent_tools: vec!["bash".into(), "edit_file".into()],
        };
        record_rating_at(&path, &pref).unwrap();
        record_rating_at(
            &path,
            &Preference {
                signal: "good".into(),
                note: None,
                ..pref.clone()
            },
        )
        .unwrap();

        let loaded = load_preferences(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].signal, "bad");
        assert_eq!(loaded[0].note.as_deref(), Some("too verbose"));
        assert_eq!(loaded[1].signal, "good");
        assert!(loaded[0].assistant_hash.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_rating_captures_last_assistant_and_recent_tools() {
        let messages = vec![
            ChatMessage::user("first"),
            ChatMessage::assistant_text("first reply"),
            ChatMessage::tool_result("c1", "read_file", "ok"),
            ChatMessage::tool_result("c2", "bash", "ok"),
            ChatMessage::user("again"),
            ChatMessage::assistant_text("second reply"),
        ];
        let pref = build_rating(
            Path::new("/tmp/proj"),
            Some("sess-x".into()),
            "bad",
            Some("noisy".into()),
            &messages,
        );
        assert_eq!(pref.signal, "bad");
        assert_eq!(pref.note.as_deref(), Some("noisy"));
        assert_eq!(pref.session_id.as_deref(), Some("sess-x"));
        // Hash should target the LAST assistant message ("second reply"),
        // not the first.
        let expected = stable_hash("second reply");
        assert_eq!(pref.assistant_hash.as_deref(), Some(expected.as_str()));
        // Recent tools deduped, in reverse-discovery order (bash seen
        // before read_file when walking from the end).
        assert!(pref.recent_tools.contains(&"bash".to_string()));
        assert!(pref.recent_tools.contains(&"read_file".to_string()));
    }

    #[test]
    fn aggregate_emits_insight_above_threshold() {
        let dir = std::env::temp_dir().join(format!(
            "metis-agg-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let insights_path = dir.join("learned.jsonl");

        // 3 distinct bad ratings all mention "bash" in recent_tools.
        for n in 0..3 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: format!("2026-04-20T12:0{n}:00Z"),
                    workspace: Some("/tmp/proj".into()),
                    session_id: Some(format!("sess-{n}")),
                    signal: "bad".into(),
                    note: None,
                    assistant_hash: Some(format!("h{n}")),
                    recent_tools: vec!["bash".into(), "read_file".into()],
                },
            )
            .unwrap();
        }

        let emitted =
            aggregate_preferences_at(&prefs_path, &insights_path, Path::new("/tmp/proj"), 3);
        assert_eq!(emitted.len(), 2, "both bash and read_file hit threshold");
        let bash = emitted
            .iter()
            .find(|i| i.tags.contains(&"bash".into()))
            .unwrap();
        assert_eq!(bash.category, "style_preference");
        assert_eq!(bash.failure_count, 3);

        let on_disk = load_all(&insights_path);
        assert_eq!(on_disk.len(), 2, "insights persisted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_below_threshold_emits_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "metis-agg-low-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let insights_path = dir.join("learned.jsonl");

        // Only 2 bad ratings — below threshold of 3.
        for n in 0..2 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: format!("2026-04-20T12:0{n}:00Z"),
                    workspace: Some("/tmp/proj".into()),
                    session_id: None,
                    signal: "bad".into(),
                    note: None,
                    assistant_hash: Some(format!("h{n}")),
                    recent_tools: vec!["bash".into()],
                },
            )
            .unwrap();
        }
        let emitted =
            aggregate_preferences_at(&prefs_path, &insights_path, Path::new("/tmp/proj"), 3);
        assert!(emitted.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_dedups_on_assistant_hash() {
        let dir = std::env::temp_dir().join(format!(
            "metis-agg-dedup-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let insights_path = dir.join("learned.jsonl");

        // Same assistant_hash rated bad 5 times — should count as ONE.
        for _ in 0..5 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: "2026-04-20T12:00:00Z".into(),
                    workspace: Some("/tmp/proj".into()),
                    session_id: None,
                    signal: "bad".into(),
                    note: None,
                    assistant_hash: Some("same-hash".into()),
                    recent_tools: vec!["bash".into()],
                },
            )
            .unwrap();
        }
        let emitted =
            aggregate_preferences_at(&prefs_path, &insights_path, Path::new("/tmp/proj"), 3);
        assert!(
            emitted.is_empty(),
            "5 ratings of same response = 1 distinct, below threshold"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_filters_by_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "metis-agg-ws-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let insights_path = dir.join("learned.jsonl");

        // 3 bad ratings under a DIFFERENT workspace — must not affect us.
        for n in 0..3 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: format!("2026-04-20T12:0{n}:00Z"),
                    workspace: Some("/tmp/other-proj".into()),
                    session_id: None,
                    signal: "bad".into(),
                    note: None,
                    assistant_hash: Some(format!("h{n}")),
                    recent_tools: vec!["bash".into()],
                },
            )
            .unwrap();
        }
        let emitted =
            aggregate_preferences_at(&prefs_path, &insights_path, Path::new("/tmp/proj"), 3);
        assert!(
            emitted.is_empty(),
            "ratings from other workspaces should not leak into this one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_good_ratings_dont_count_negative() {
        let dir = std::env::temp_dir().join(format!(
            "metis-agg-good-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let insights_path = dir.join("learned.jsonl");

        for n in 0..5 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: format!("2026-04-20T12:0{n}:00Z"),
                    workspace: Some("/tmp/proj".into()),
                    session_id: None,
                    signal: "good".into(), // all positive
                    note: None,
                    assistant_hash: Some(format!("h{n}")),
                    recent_tools: vec!["bash".into()],
                },
            )
            .unwrap();
        }
        let emitted =
            aggregate_preferences_at(&prefs_path, &insights_path, Path::new("/tmp/proj"), 3);
        assert!(
            emitted.is_empty(),
            "good ratings must not produce negative insights"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_rating_handles_empty_transcript() {
        let pref = build_rating(Path::new("/tmp/proj"), None, "good", None, &[]);
        assert_eq!(pref.signal, "good");
        assert!(pref.assistant_hash.is_none());
        assert!(pref.recent_tools.is_empty());
    }

    #[test]
    fn build_rating_falls_back_to_tool_calls_when_content_empty() {
        use aegis_api::{ToolCall, ToolCallFunction};
        let mut asst = ChatMessage::assistant_text("");
        asst.tool_calls = vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            },
        }];
        let messages = vec![ChatMessage::user("go"), asst];
        let pref = build_rating(Path::new("/tmp/proj"), None, "bad", None, &messages);
        // No content, but tool_calls present — must still hash to Some.
        assert!(
            pref.assistant_hash.is_some(),
            "tool-call-only assistant should still produce a hash"
        );
        // Different args ⇒ different hash.
        let mut asst2 = ChatMessage::assistant_text("");
        asst2.tool_calls = vec![ToolCall {
            id: "c2".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{\"cmd\":\"pwd\"}".into(),
            },
        }];
        let pref2 = build_rating(
            Path::new("/tmp/proj"),
            None,
            "bad",
            None,
            &[ChatMessage::user("go"), asst2],
        );
        assert_ne!(
            pref.assistant_hash, pref2.assistant_hash,
            "distinct tool args must hash distinctly"
        );
    }

    #[test]
    fn extract_insights_multiple_stopped_tools() {
        use aegis_api::{ToolCall, ToolCallFunction};
        let mk = |name: &str, id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: "{}".into(),
            },
        };
        let mut asst = ChatMessage::assistant_text("doing many things");
        asst.tool_calls = vec![mk("bash", "c1"), mk("edit_file", "c2"), mk("bash", "c3")];
        let messages = vec![ChatMessage::user("go"), asst, ChatMessage::user("hayır")];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        // Expect one insight per *distinct* tool — dupes collapsed.
        let bash = insights
            .iter()
            .filter(|i| i.text.contains("`bash`") && i.text.contains("stopped"))
            .count();
        let edit = insights
            .iter()
            .filter(|i| i.text.contains("`edit_file`") && i.text.contains("stopped"))
            .count();
        assert_eq!(bash, 1, "duplicate bash calls should dedupe, got {bash}");
        assert_eq!(edit, 1);
    }

    #[test]
    fn extract_insights_stop_word_case_insensitive_and_padded() {
        // Uppercase + surrounding whitespace still counts as stop.
        let mut asst = ChatMessage::assistant_text("running");
        asst.tool_calls = vec![aegis_api::ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: aegis_api::ToolCallFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        let messages = vec![ChatMessage::user("go"), asst, ChatMessage::user("  DUR  ")];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        assert!(
            insights.iter().any(|i| i.text.contains("`bash`")),
            "trimmed uppercase stop should still trigger, got {insights:?}"
        );
    }

    #[test]
    fn extract_insights_ignores_user_after_user() {
        // A stop word NOT directly after assistant does not count.
        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::user("dur"), // no assistant in between
        ];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        assert!(
            insights.is_empty(),
            "stop-word without preceding assistant should produce no insight"
        );
    }

    #[test]
    fn extract_insights_non_stop_user_after_tools_no_signal() {
        use aegis_api::{ToolCall, ToolCallFunction};
        let mut asst = ChatMessage::assistant_text("running");
        asst.tool_calls = vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        let messages = vec![
            ChatMessage::user("go"),
            asst,
            ChatMessage::user("tamam devam et"),
        ];
        let insights = extract_insights(&messages, Path::new("/tmp/proj"));
        assert!(
            insights.iter().all(|i| !i.text.contains("stopped")),
            "non-stop user message must not produce stop insight, got {insights:?}"
        );
    }

    // ---------------------------------------------------------------
    // extract_instructions — Seviye 3 explicit rule mining
    // ---------------------------------------------------------------

    fn rule_texts(insights: &[Insight]) -> Vec<String> {
        insights.iter().map(|i| i.text.clone()).collect()
    }

    #[test]
    fn extract_instructions_captures_english_rule_forms() {
        let messages = vec![
            ChatMessage::user("from now on always use edit instead of bash"),
            ChatMessage::user("don't write comments unless I ask"),
            ChatMessage::user("never force push to main"),
            ChatMessage::user("always run cargo fmt before committing"),
        ];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        let texts = rule_texts(&insights);
        assert_eq!(insights.len(), 4, "got {texts:?}");
        assert!(
            texts
                .iter()
                .all(|t| t.to_lowercase().starts_with("from now on")
                    || t.to_lowercase().starts_with("don't")
                    || t.to_lowercase().starts_with("never")
                    || t.to_lowercase().starts_with("always")),
            "every captured rule must start with a trigger: {texts:?}"
        );
        assert!(insights.iter().all(|i| i.category == "instruction"));
        assert!(insights
            .iter()
            .all(|i| i.tags.iter().any(|t| t == "instruction")));
    }

    #[test]
    fn extract_instructions_captures_turkish_rule_forms() {
        let messages = vec![
            ChatMessage::user("bundan sonra her değişiklikten önce yedek al"),
            ChatMessage::user("her zaman cargo test çalıştır"),
            ChatMessage::user("hiç yarım iş bırakma"),
            ChatMessage::user("asla console.log bırakma"),
        ];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        let texts = rule_texts(&insights);
        assert_eq!(insights.len(), 4, "got {texts:?}");
    }

    #[test]
    fn extract_instructions_ignores_idioms() {
        let messages = vec![
            ChatMessage::user("don't worry about it, just pick one"),
            ChatMessage::user("I don't know what's happening"),
            ChatMessage::user("never mind, I figured it out"),
            ChatMessage::user("always thought that was a bug"),
            ChatMessage::user("hiç bir şey olmadı"),
        ];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        assert!(
            insights.is_empty(),
            "idioms must be filtered: {:?}",
            rule_texts(&insights)
        );
    }

    #[test]
    fn extract_instructions_dedupes_within_session() {
        let messages = vec![
            ChatMessage::user("always run cargo fmt before committing"),
            ChatMessage::user("reminder: Always run cargo fmt before committing."),
            ChatMessage::user("tamam"),
        ];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        assert_eq!(
            insights.len(),
            1,
            "case/whitespace-equivalent repeats must collapse: {:?}",
            rule_texts(&insights)
        );
    }

    #[test]
    fn extract_instructions_ignores_non_user_roles() {
        let messages = vec![
            ChatMessage::assistant_text("always validate input before using it"),
            ChatMessage::system("never trust user input"),
            ChatMessage::user("from now on treat user input as safe here"),
        ];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        // Only the user-authored clause survives.
        assert_eq!(insights.len(), 1);
        assert!(
            insights[0].text.to_lowercase().starts_with("from now on"),
            "got {}",
            insights[0].text
        );
    }

    #[test]
    fn extract_instructions_mid_sentence_trigger_after_period() {
        let messages = vec![ChatMessage::user(
            "This is fine. Always run cargo check before pushing. Also clean up.",
        )];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        assert_eq!(insights.len(), 1, "got {:?}", rule_texts(&insights));
        let text = insights[0].text.to_lowercase();
        assert!(
            text.starts_with("always run cargo check before pushing"),
            "clause must start at the trigger, stop at '.', got `{}`",
            insights[0].text
        );
    }

    #[test]
    fn extract_instructions_short_fragment_rejected() {
        // "don't!" is a trigger + exclamation — too short to be a rule.
        let messages = vec![ChatMessage::user("don't!"), ChatMessage::user("always.")];
        let insights = extract_instructions(&messages, Path::new("/tmp/proj"));
        assert!(
            insights.is_empty(),
            "short fragments must be rejected: {:?}",
            rule_texts(&insights)
        );
    }

    #[test]
    fn format_insights_section_renders_instruction_header_above_others() {
        let now = crate::telemetry::now_iso8601();
        let rule = Insight {
            timestamp: now.clone(),
            last_seen: Some(now.clone()),
            workspace: Some("/tmp/proj".into()),
            category: "instruction".into(),
            text: "from now on always run cargo fmt".into(),
            reinforcements: 2,
            success_count: 0,
            failure_count: 0,
            tags: vec!["instruction".into()],
        };
        let pref = Insight {
            category: "style_preference".into(),
            text: "User has rated 3 reply(ies) `bad` when `bash` was in recent context.".into(),
            ..rule.clone()
        };
        let pattern = Insight {
            category: "error_recovery".into(),
            text: "Tool `read_file` needed 2 retries.".into(),
            ..rule.clone()
        };
        let out = format_insights_section(&[pattern.clone(), pref.clone(), rule.clone()]);

        let rules_idx = out
            .find("# User-stated rules")
            .expect("rules header present");
        let prefs_idx = out
            .find("# User-rated dislikes")
            .expect("dislikes header present");
        let patterns_idx = out
            .find("# Learned patterns")
            .expect("patterns header present");
        assert!(
            rules_idx < prefs_idx && prefs_idx < patterns_idx,
            "header order must be rules → dislikes → patterns, got {out}"
        );
    }

    #[test]
    fn balanced_selection_empty_file() {
        let dir = std::env::temp_dir().join(format!(
            "metis-learn-empty-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.jsonl");
        std::fs::write(&path, "").unwrap();
        let all = load_all(&path);
        assert!(all.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn balanced_selection_single_category_respects_max() {
        // Only one category present — final truncate at `max` should
        // still bite even if per_category would have allowed more.
        use std::collections::BTreeMap;
        let items: Vec<Insight> = (0..10)
            .map(|n| Insight {
                workspace: None,
                category: "tool_pattern".into(),
                text: format!("t{n}"),
                reinforcements: 10,
                ..fixture_insight(&format!("t{n}"), 10)
            })
            .collect();
        let now = crate::telemetry::now_unix_secs();
        let mut by_cat: BTreeMap<String, Vec<Insight>> = BTreeMap::new();
        for i in items {
            by_cat.entry(i.category.clone()).or_default().push(i);
        }
        let per_category = 8;
        let mut picked: Vec<Insight> = Vec::new();
        for (_cat, mut group) in by_cat {
            group.sort_by(|a, b| b.score(now).partial_cmp(&a.score(now)).unwrap());
            group.truncate(per_category);
            picked.extend(group);
        }
        let max = 5;
        picked.truncate(max);
        assert_eq!(picked.len(), 5, "final max must cap even single-cat input");
    }

    #[test]
    fn tool_feedback_mixed_session() {
        let messages = vec![
            ChatMessage::user("do stuff"),
            ChatMessage::tool_result("c1", "read_file", "ok contents"),
            ChatMessage::tool_result("c2", "bash", "error: permission denied"),
            ChatMessage::tool_result("c3", "bash", "error: still"),
            ChatMessage::tool_result("c4", "edit_file", "error: missing"),
            ChatMessage::tool_result("c5", "edit_file", "ok applied"),
        ];
        let sigs = extract_tool_feedback(&messages);
        // read_file: clean → (read_file, true)
        // bash: stuck in error → (bash, false)
        // edit_file: recovery (err then ok) → no signal
        assert_eq!(
            sigs.len(),
            2,
            "recovery tool should be skipped, got {sigs:?}"
        );
        assert!(sigs.contains(&("read_file".to_string(), true)));
        assert!(sigs.contains(&("bash".to_string(), false)));
        assert!(
            !sigs.iter().any(|(n, _)| n == "edit_file"),
            "edit_file recovery must not emit a direct signal"
        );
    }

    #[test]
    fn tool_feedback_ignores_tool_without_name() {
        let mut headless = ChatMessage::tool_result("c1", "", "ok");
        headless.name = None;
        let messages = vec![ChatMessage::user("go"), headless];
        let sigs = extract_tool_feedback(&messages);
        assert!(sigs.is_empty(), "nameless tool results must be skipped");
    }

    #[test]
    fn record_feedback_by_tag_respects_workspace_scope() {
        let path = tmp_path("tag-ws-scope");
        let global = Insight {
            workspace: None,
            category: "tool_pattern".into(),
            text: "global bash note".into(),
            tags: vec!["bash".into()],
            ..fixture_insight("global bash note", 1)
        };
        let proj_a = Insight {
            workspace: Some("/tmp/proj-a".into()),
            category: "tool_pattern".into(),
            text: "proj-a bash".into(),
            tags: vec!["bash".into()],
            ..fixture_insight("proj-a bash", 1)
        };
        let proj_b = Insight {
            workspace: Some("/tmp/proj-b".into()),
            category: "tool_pattern".into(),
            text: "proj-b bash".into(),
            tags: vec!["bash".into()],
            ..fixture_insight("proj-b bash", 1)
        };
        append_one(&path, &global).unwrap();
        append_one(&path, &proj_a).unwrap();
        append_one(&path, &proj_b).unwrap();

        // Calling from proj-a should touch global + proj-a only.
        let touched = record_feedback_by_tag_at(&path, Path::new("/tmp/proj-a"), "bash", true);
        assert_eq!(touched, 2, "should touch global + proj-a, not proj-b");

        let all = load_all(&path);
        let by_text = |t: &str| all.iter().find(|i| i.text == t).unwrap();
        assert_eq!(by_text("global bash note").success_count, 1);
        assert_eq!(by_text("proj-a bash").success_count, 1);
        assert_eq!(
            by_text("proj-b bash").success_count,
            0,
            "other workspace must remain untouched"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn record_feedback_by_tag_no_match_returns_zero() {
        let path = tmp_path("tag-no-match");
        append_one(
            &path,
            &Insight {
                workspace: Some("/tmp/proj".into()),
                tags: vec!["bash".into()],
                ..fixture_insight("x", 1)
            },
        )
        .unwrap();
        let touched = record_feedback_by_tag_at(&path, Path::new("/tmp/proj"), "nonexistent", true);
        assert_eq!(touched, 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn tool_feedback_clean_run_positive() {
        let messages = vec![
            ChatMessage::user("read it"),
            ChatMessage::tool_result("c1", "read_file", "contents of file"),
        ];
        let sigs = extract_tool_feedback(&messages);
        assert_eq!(sigs, vec![("read_file".to_string(), true)]);
    }

    #[test]
    fn tool_feedback_stuck_error_negative() {
        let messages = vec![
            ChatMessage::user("run it"),
            ChatMessage::tool_result("c1", "bash", "error: command not found"),
            ChatMessage::tool_result("c2", "bash", "error: still broken"),
        ];
        let sigs = extract_tool_feedback(&messages);
        assert_eq!(sigs, vec![("bash".to_string(), false)]);
    }

    #[test]
    fn tool_feedback_recovery_emits_nothing() {
        // Recovery is handled by pattern 1 in extract_insights, so
        // extract_tool_feedback should stay silent to avoid double-count.
        let messages = vec![
            ChatMessage::user("edit it"),
            ChatMessage::tool_result("c1", "edit_file", "error: file not found"),
            ChatMessage::tool_result("c2", "edit_file", "ok"),
        ];
        let sigs = extract_tool_feedback(&messages);
        assert!(
            sigs.is_empty(),
            "recovery should emit no direct signal, got {sigs:?}"
        );
    }

    #[test]
    fn record_feedback_by_tag_bumps_matching_insights() {
        let path = tmp_path("tag-feedback");
        let a = Insight {
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: "bash is flaky".into(),
            tags: vec!["bash".into()],
            ..fixture_insight("bash is flaky", 1)
        };
        let b = Insight {
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: "read works".into(),
            tags: vec!["read_file".into()],
            ..fixture_insight("read works", 1)
        };
        append_one(&path, &a).unwrap();
        append_one(&path, &b).unwrap();

        let touched = record_feedback_by_tag_at(&path, Path::new("/tmp/proj"), "bash", false);
        assert_eq!(touched, 1);

        let all = load_all(&path);
        let bash_insight = all
            .iter()
            .find(|i| i.tags.contains(&"bash".into()))
            .unwrap();
        assert_eq!(bash_insight.failure_count, 1);
        let read_insight = all
            .iter()
            .find(|i| i.tags.contains(&"read_file".into()))
            .unwrap();
        assert_eq!(
            read_insight.failure_count, 0,
            "non-matching insight must stay untouched"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn record_feedback_bumps_counts() {
        let path = tmp_path("feedback");
        let base = Insight {
            workspace: Some("/tmp/proj".into()),
            category: "tool_pattern".into(),
            text: "use grep not find".into(),
            ..fixture_insight("use grep not find", 1)
        };
        append_one(&path, &base).unwrap();

        let touched = record_feedback_at(&path, Path::new("/tmp/proj"), "use grep not find", true);
        assert_eq!(touched, 1);
        let touched = record_feedback_at(&path, Path::new("/tmp/proj"), "use grep not find", false);
        assert_eq!(touched, 1);

        let all = load_all(&path);
        assert_eq!(all[0].success_count, 1);
        assert_eq!(all[0].failure_count, 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn summarize_ratings_counts_and_sorts_bad_tools() {
        let dir = std::env::temp_dir().join(format!(
            "metis-summary-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let ws = Path::new("/tmp/proj");

        // Mix: 2 good, 3 bad with bash, 1 bad with edit_file, 1 from another ws.
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "t".into(),
                workspace: Some("/tmp/proj".into()),
                session_id: None,
                signal: "good".into(),
                note: None,
                assistant_hash: Some("g1".into()),
                recent_tools: vec!["bash".into()],
            },
        )
        .unwrap();
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "t".into(),
                workspace: Some("/tmp/proj".into()),
                session_id: None,
                signal: "good".into(),
                note: None,
                assistant_hash: Some("g2".into()),
                recent_tools: vec![],
            },
        )
        .unwrap();
        for n in 0..3 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: "t".into(),
                    workspace: Some("/tmp/proj".into()),
                    session_id: None,
                    signal: "bad".into(),
                    note: None,
                    assistant_hash: Some(format!("b{n}")),
                    recent_tools: vec!["bash".into()],
                },
            )
            .unwrap();
        }
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "t".into(),
                workspace: Some("/tmp/proj".into()),
                session_id: None,
                signal: "bad".into(),
                note: None,
                assistant_hash: Some("be".into()),
                recent_tools: vec!["edit_file".into()],
            },
        )
        .unwrap();
        // Another workspace — must be ignored.
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "t".into(),
                workspace: Some("/tmp/other".into()),
                session_id: None,
                signal: "bad".into(),
                note: None,
                assistant_hash: Some("o1".into()),
                recent_tools: vec!["bash".into()],
            },
        )
        .unwrap();

        let summary = summarize_ratings_at(&prefs_path, ws, 3);
        assert_eq!(summary.good, 2);
        assert_eq!(summary.bad, 4);
        assert_eq!(summary.threshold, 3);
        assert_eq!(
            summary.bad_tools,
            vec![("bash".to_string(), 3), ("edit_file".to_string(), 1),]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_last_rating_pops_most_recent_for_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "metis-undo-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let ws = Path::new("/tmp/proj");

        for n in 0..3 {
            record_rating_at(
                &prefs_path,
                &Preference {
                    timestamp: format!("2026-04-20T12:0{n}:00Z"),
                    workspace: Some("/tmp/proj".into()),
                    session_id: None,
                    signal: "good".into(),
                    note: Some(format!("n{n}")),
                    assistant_hash: Some(format!("h{n}")),
                    recent_tools: vec![],
                },
            )
            .unwrap();
        }
        // Other workspace — must be preserved.
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "2026-04-20T12:99:00Z".into(),
                workspace: Some("/tmp/other".into()),
                session_id: None,
                signal: "bad".into(),
                note: None,
                assistant_hash: Some("o".into()),
                recent_tools: vec![],
            },
        )
        .unwrap();

        let removed = undo_last_rating_at(&prefs_path, ws).unwrap().unwrap();
        assert_eq!(removed.note.as_deref(), Some("n2"));

        let after = load_preferences(&prefs_path);
        assert_eq!(after.len(), 3);
        assert!(
            after
                .iter()
                .any(|p| p.workspace.as_deref() == Some("/tmp/other")),
            "other workspace must be untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_last_rating_returns_none_when_workspace_empty() {
        let dir = std::env::temp_dir().join(format!(
            "metis-undo-empty-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        record_rating_at(
            &prefs_path,
            &Preference {
                timestamp: "t".into(),
                workspace: Some("/tmp/other".into()),
                session_id: None,
                signal: "bad".into(),
                note: None,
                assistant_hash: None,
                recent_tools: vec![],
            },
        )
        .unwrap();

        let removed = undo_last_rating_at(&prefs_path, Path::new("/tmp/proj")).unwrap();
        assert!(removed.is_none());
        // Other workspace's entry must still be there.
        assert_eq!(load_preferences(&prefs_path).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_insights_removes_matching_workspace_entries() {
        let dir = std::env::temp_dir().join(format!(
            "metis-forget-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.jsonl");
        let ws = Path::new("/tmp/proj");

        upsert_insight_at(
            &path,
            &Insight {
                workspace: Some("/tmp/proj".into()),
                category: "tool_pattern".into(),
                text: "User dislikes bash navigation".into(),
                tags: vec!["bash".into()],
                ..fixture_insight("dislike-bash", 1)
            },
        )
        .unwrap();
        upsert_insight_at(
            &path,
            &Insight {
                workspace: Some("/tmp/proj".into()),
                category: "tool_pattern".into(),
                text: "edit_file pattern".into(),
                tags: vec!["edit_file".into()],
                ..fixture_insight("edit-pattern", 1)
            },
        )
        .unwrap();
        upsert_insight_at(
            &path,
            &Insight {
                workspace: Some("/tmp/other".into()),
                category: "tool_pattern".into(),
                text: "other workspace bash thing".into(),
                tags: vec!["bash".into()],
                ..fixture_insight("other-bash", 1)
            },
        )
        .unwrap();

        let removed = forget_insights_at(&path, ws, "bash").unwrap();
        assert_eq!(removed.len(), 1, "only proj-scoped bash insight removed");
        assert!(removed[0].text.contains("dislikes bash"));

        let kept = load_all(&path);
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter().any(|i| i.text.contains("other workspace")),
            "other workspace must be preserved"
        );
        assert!(
            kept.iter().any(|i| i.text.contains("edit_file")),
            "non-matching proj insight must be preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_transcript_to_system_prompt_section() {
        // End-to-end: simulate a transcript, run the full pipeline that
        // production calls (extract → upsert → load_balanced → format),
        // and assert the user-visible section reflects what happened.
        use aegis_api::{ToolCall, ToolCallFunction};

        let dir = std::env::temp_dir().join(format!(
            "metis-roundtrip-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let insights_path = dir.join("learned.jsonl");
        let prefs_path = dir.join("preferences.jsonl");
        let ws = dir.clone();

        // Pattern A: user stops bash after assistant proposes it.
        let mut asst = ChatMessage::assistant_text("running bash");
        asst.tool_calls = vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        let transcript = vec![
            ChatMessage::user("look at logs"),
            asst,
            ChatMessage::user("dur"),
        ];

        // Pipeline step 1: extract → upsert (mirrors what TUI/REPL run on session end)
        let extracted = extract_insights(&transcript, &ws);
        assert!(
            extracted
                .iter()
                .any(|i| i.text.contains("`bash`") && i.text.contains("stopped")),
            "extract should produce bash-stopped insight, got {extracted:?}"
        );
        for ins in &extracted {
            upsert_insight_at(&insights_path, ins).unwrap();
        }

        // Pattern B: user rates `bad` × 3 → aggregator promotes to style_preference
        for n in 0..3 {
            let mut a2 = ChatMessage::assistant_text("");
            a2.tool_calls = vec![ToolCall {
                id: format!("d{n}"),
                kind: "function".into(),
                function: ToolCallFunction {
                    name: "bash".into(),
                    arguments: format!("{{\"n\":{n}}}"),
                },
            }];
            // Tool reply — build_rating walks Role::Tool to populate
            // recent_tools, which is what the aggregator counts.
            let mut tool_reply = ChatMessage::user("ignored");
            tool_reply.role = aegis_api::Role::Tool;
            tool_reply.name = Some("bash".into());
            tool_reply.content = Some("ok".into());
            let pref = build_rating(
                &ws,
                Some(format!("s{n}")),
                "bad",
                Some(format!("noisy {n}")),
                &[ChatMessage::user("go"), a2, tool_reply],
            );
            record_rating_at(&prefs_path, &pref).unwrap();
        }
        let promoted = aggregate_preferences_at(&prefs_path, &insights_path, &ws, 3);
        assert_eq!(promoted.len(), 1, "exactly one style_preference for `bash`");
        assert_eq!(promoted[0].category, "style_preference");

        // Pipeline step 2: balanced load + format (mirrors main.rs:736)
        let loaded = load_relevant_insights_balanced_at(&insights_path, &ws, 4, 10);
        let section = format_insights_section(&loaded);

        // The dedicated dislikes header must appear and come BEFORE the
        // generic patterns header — that's the contract the system
        // prompt relies on so the model treats prefs as soft constraints.
        let prefs_pos = section
            .find("User-rated dislikes")
            .expect("section must include dislikes header");
        let learn_pos = section
            .find("Learned patterns from past sessions")
            .expect("section must include patterns header");
        assert!(prefs_pos < learn_pos);
        assert!(
            section.contains("`bash`"),
            "bash referenced somewhere in section"
        );

        // /forget removes both — the user can correct a wrong learning
        // pass via a single command. Verify symmetry of the loop.
        let removed = forget_insights_at(&insights_path, &ws, "bash").unwrap();
        assert!(
            removed.len() >= 2,
            "/forget bash should remove both stopped + style_preference, got {}",
            removed.len()
        );
        let after = load_all(&insights_path);
        assert!(
            !after.iter().any(|i| i.text.to_lowercase().contains("bash")),
            "no bash insight should survive /forget"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_insights_empty_needle_is_noop() {
        let dir = std::env::temp_dir().join(format!(
            "metis-forget-empty-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.jsonl");
        upsert_insight_at(
            &path,
            &Insight {
                workspace: Some("/tmp/proj".into()),
                text: "x".into(),
                ..fixture_insight("x", 1)
            },
        )
        .unwrap();

        let removed = forget_insights_at(&path, Path::new("/tmp/proj"), "").unwrap();
        assert!(removed.is_empty());
        assert_eq!(load_all(&path).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_ratings_empty_file_returns_zero() {
        let dir = std::env::temp_dir().join(format!(
            "metis-summary-empty-{}-{}",
            std::process::id(),
            crate::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("preferences.jsonl");
        let summary = summarize_ratings_at(&prefs_path, Path::new("/tmp/proj"), 3);
        assert_eq!(summary.good, 0);
        assert_eq!(summary.bad, 0);
        assert!(summary.bad_tools.is_empty());
        assert_eq!(summary.threshold, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
