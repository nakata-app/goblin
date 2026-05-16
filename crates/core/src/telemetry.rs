//! Usage telemetry — persists per-session statistics to disk so users
//! can track cost, token usage, and tool activity over time.
//!
//! Data is stored in `~/.aegis/telemetry.jsonl` — one JSON line per
//! completed session. The file is append-only and never rewritten.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A single telemetry record written at the end of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryRecord {
    /// ISO 8601 timestamp when the session ended.
    pub timestamp: String,
    /// Session id (if any).
    pub session_id: Option<String>,
    /// Model used.
    pub model: String,
    /// Provider name.
    pub provider: String,
    /// Total input tokens.
    pub input_tokens: u32,
    /// Total output tokens.
    pub output_tokens: u32,
    /// Cache read tokens.
    pub cache_read_tokens: u32,
    /// Cache write tokens.
    pub cache_write_tokens: u32,
    /// Number of agent turns.
    pub turns: usize,
    /// Estimated cost in USD.
    pub cost_usd: f64,
    /// Tool call counts.
    pub tool_calls: HashMap<String, u32>,
}

/// Path to the global telemetry file.
pub fn telemetry_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".metis").join("telemetry.jsonl"))
}

/// Append a telemetry record to disk.
pub fn append_record(record: &TelemetryRecord) -> Result<(), String> {
    let path = telemetry_path().ok_or("could not determine home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let line = serde_json::to_string(record).map_err(|e| format!("json: {e}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Load all telemetry records from disk.
pub fn load_records(path: &Path) -> Vec<TelemetryRecord> {
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

/// Aggregated statistics from telemetry records.
#[derive(Debug, Default)]
pub struct UsageStats {
    pub total_sessions: usize,
    pub total_turns: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub tool_counts: HashMap<String, u64>,
    pub model_counts: HashMap<String, usize>,
    /// Cost per day (YYYY-MM-DD → cost).
    pub daily_cost: HashMap<String, f64>,
}

impl UsageStats {
    /// Compute aggregate stats from a list of records.
    pub fn from_records(records: &[TelemetryRecord]) -> Self {
        let mut stats = Self {
            total_sessions: records.len(),
            ..Default::default()
        };
        for r in records {
            stats.total_turns += r.turns;
            stats.total_input_tokens += r.input_tokens as u64;
            stats.total_output_tokens += r.output_tokens as u64;
            stats.total_cost_usd += r.cost_usd;
            for (tool, count) in &r.tool_calls {
                *stats.tool_counts.entry(tool.clone()).or_default() += *count as u64;
            }
            *stats.model_counts.entry(r.model.clone()).or_default() += 1;
            // Extract date from timestamp (first 10 chars of ISO 8601)
            let day = if r.timestamp.len() >= 10 {
                r.timestamp[..10].to_string()
            } else {
                r.timestamp.clone()
            };
            *stats.daily_cost.entry(day).or_default() += r.cost_usd;
        }
        stats
    }

    /// Format as a human-readable dashboard string.
    pub fn format_dashboard(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Sessions: {}  |  Turns: {}  |  Cost: ${:.4}\n",
            self.total_sessions, self.total_turns, self.total_cost_usd
        ));
        out.push_str(&format!(
            "Tokens: {} in / {} out\n",
            self.total_input_tokens, self.total_output_tokens
        ));

        // Top models
        if !self.model_counts.is_empty() {
            let mut models: Vec<_> = self.model_counts.iter().collect();
            models.sort_by(|a, b| b.1.cmp(a.1));
            out.push_str("Models: ");
            for (i, (model, count)) in models.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{model} ({count})"));
            }
            out.push('\n');
        }

        // Top 5 tools
        if !self.tool_counts.is_empty() {
            let mut tools: Vec<_> = self.tool_counts.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1));
            out.push_str("Top tools: ");
            for (i, (tool, count)) in tools.iter().take(5).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{tool} ({count})"));
            }
            out.push('\n');
        }

        // Last 7 days cost
        if !self.daily_cost.is_empty() {
            let mut days: Vec<_> = self.daily_cost.iter().collect();
            days.sort_by(|a, b| b.0.cmp(a.0));
            out.push_str("Recent cost:\n");
            for (day, cost) in days.iter().take(7) {
                out.push_str(&format!("  {day}: ${cost:.4}\n"));
            }
        }

        out
    }
}

/// UTC date for today in `YYYY-MM-DD` form. Matches the prefix of the
/// ISO 8601 timestamp stamped into [`TelemetryRecord::timestamp`], so
/// callers can compare with a plain string `starts_with`.
pub fn today_date() -> String {
    now_iso8601().chars().take(10).collect()
}

/// Cumulative spend in USD across every [`TelemetryRecord`] whose
/// `timestamp` falls on the given UTC date. Missing or empty telemetry
/// files yield `0.0` — the `metis --budget` path wants a number, not a
/// `Result`, so IO failures are treated as "no data yet" and handled
/// transparently.
///
/// Used by the `/budget` slash command and the REPL's startup banner
/// to answer "how much have I spent today?" without replaying the full
/// telemetry dashboard.
pub fn spent_on(date: &str) -> f64 {
    let path = match telemetry_path() {
        Some(p) => p,
        None => return 0.0,
    };
    let records = load_records(&path);
    records
        .iter()
        .filter(|r| r.timestamp.starts_with(date))
        .map(|r| r.cost_usd)
        .sum()
}

/// Shorthand for [`spent_on`]`(&`[`today_date`]`())`.
pub fn spent_today() -> f64 {
    spent_on(&today_date())
}

/// Snapshot of the daily budget state, ready to be formatted for a
/// banner or a `/budget` readout. `prior_usd` covers every session that
/// completed earlier today; `session_usd` is what the current,
/// still-running REPL has spent since it started. `budget_usd` is the
/// user's configured ceiling (`daily_budget_usd` in `config.toml`).
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetStatus {
    pub prior_usd: f64,
    pub session_usd: f64,
    pub budget_usd: Option<f64>,
}

impl BudgetStatus {
    pub fn total_usd(&self) -> f64 {
        self.prior_usd + self.session_usd
    }

    /// Fraction of the daily budget consumed, clamped to `[0, 1]`.
    /// Returns `None` when no budget is configured.
    pub fn fraction(&self) -> Option<f64> {
        self.budget_usd.map(|b| {
            if b <= 0.0 {
                0.0
            } else {
                (self.total_usd() / b).clamp(0.0, 1.0)
            }
        })
    }

    /// `true` when the running total has met or passed the budget.
    /// Always `false` when no budget is set.
    pub fn over_budget(&self) -> bool {
        match self.budget_usd {
            Some(b) => self.total_usd() >= b && b > 0.0,
            None => false,
        }
    }

    /// One-line human summary, e.g. `"today: $1.23 (prior $0.80 + session $0.43) / $5.00 (24%)"`.
    /// Omits the budget suffix when no budget is configured.
    pub fn summary(&self) -> String {
        let base = format!(
            "today: ${:.4} (prior ${:.4} + session ${:.4})",
            self.total_usd(),
            self.prior_usd,
            self.session_usd
        );
        match self.budget_usd {
            Some(b) => {
                let pct = self.fraction().unwrap_or(0.0) * 100.0;
                format!("{base} / ${b:.2} ({pct:.0}%)")
            }
            None => base,
        }
    }
}

/// Unix seconds since epoch for now.
pub fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an ISO 8601 timestamp produced by [`now_iso8601`] back to unix
/// seconds. Returns 0 on parse failure (caller treats as "very old").
pub fn iso8601_to_unix(s: &str) -> i64 {
    // Expected shape: YYYY-MM-DDTHH:MM:SSZ
    let s = s.trim_end_matches('Z');
    let (date, time) = match s.split_once('T') {
        Some(v) => v,
        None => return 0,
    };
    let date_parts: Vec<&str> = date.split('-').collect();
    let time_parts: Vec<&str> = time.split(':').collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return 0;
    }
    let year: u32 = match date_parts[0].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let month: u32 = match date_parts[1].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let day: u32 = match date_parts[2].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let hour: u32 = match time_parts[0].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let minute: u32 = match time_parts[1].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    // Seconds may be fractional; drop the fraction.
    let second: u32 = match time_parts[2].split('.').next().unwrap_or("0").parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if !(1..=12).contains(&month) || day < 1 {
        return 0;
    }

    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days: [u32; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for md in month_days.iter().take(month as usize - 1) {
        days += *md as u64;
    }
    days += (day - 1) as u64;

    (days * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64) as i64
}

/// ISO 8601 timestamp for now.
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple formatting without chrono dependency
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since epoch → date (simplified, no leap second handling)
    let mut year = 1970u32;
    let mut remaining_days = days as u32;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        year += 1;
    }
    let month_days: [u32; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for md in month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }
    let day = remaining_days + 1;

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn is_leap(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip() {
        let record = TelemetryRecord {
            timestamp: "2026-04-10T12:00:00Z".into(),
            session_id: Some("abc".into()),
            model: "deepseek-chat".into(),
            provider: "deepseek".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            turns: 3,
            cost_usd: 0.0015,
            tool_calls: [("read_file".to_string(), 5), ("bash".to_string(), 2)]
                .into_iter()
                .collect(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TelemetryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, "deepseek-chat");
        assert_eq!(back.turns, 3);
        assert_eq!(back.tool_calls.get("read_file"), Some(&5));
    }

    #[test]
    fn stats_aggregation() {
        let records = vec![
            TelemetryRecord {
                timestamp: "2026-04-10T12:00:00Z".into(),
                session_id: None,
                model: "gpt-4o".into(),
                provider: "openai".into(),
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                turns: 2,
                cost_usd: 0.001,
                tool_calls: [("bash".to_string(), 3)].into_iter().collect(),
            },
            TelemetryRecord {
                timestamp: "2026-04-10T14:00:00Z".into(),
                session_id: None,
                model: "gpt-4o".into(),
                provider: "openai".into(),
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                turns: 5,
                cost_usd: 0.002,
                tool_calls: [("bash".to_string(), 1), ("read_file".to_string(), 4)]
                    .into_iter()
                    .collect(),
            },
        ];
        let stats = UsageStats::from_records(&records);
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.total_turns, 7);
        assert_eq!(stats.total_input_tokens, 300);
        assert_eq!(stats.total_output_tokens, 150);
        assert!((stats.total_cost_usd - 0.003).abs() < 1e-9);
        assert_eq!(stats.tool_counts.get("bash"), Some(&4));
        assert_eq!(stats.tool_counts.get("read_file"), Some(&4));
        assert_eq!(stats.model_counts.get("gpt-4o"), Some(&2));
    }

    #[test]
    fn dashboard_format_contains_key_info() {
        let stats = UsageStats {
            total_sessions: 5,
            total_turns: 20,
            total_cost_usd: 0.05,
            total_input_tokens: 5000,
            total_output_tokens: 2000,
            ..Default::default()
        };
        let dash = stats.format_dashboard();
        assert!(dash.contains("Sessions: 5"));
        assert!(dash.contains("Turns: 20"));
        assert!(dash.contains("5000 in"));
    }

    #[test]
    fn iso8601_round_trip_matches_unix() {
        let secs = now_unix_secs();
        let ts = now_iso8601();
        let parsed = iso8601_to_unix(&ts);
        // now_iso8601 formats with 1-second resolution, so allow a 2s slack.
        assert!((parsed - secs).abs() <= 2, "parsed={parsed} secs={secs}");
    }

    #[test]
    fn iso8601_to_unix_handles_invalid() {
        assert_eq!(iso8601_to_unix(""), 0);
        assert_eq!(iso8601_to_unix("not a date"), 0);
        assert_eq!(iso8601_to_unix("2026-13-01T00:00:00Z"), 0);
    }

    #[test]
    fn iso8601_to_unix_known_value() {
        // 2026-01-01T00:00:00Z = 56 years after epoch
        // Leap years 1972..=2024: 1972,76,80,84,88,92,96,2000,04,08,12,16,20,24 = 14 leaps
        // 56*365 + 14 = 20454 days
        let expected = 20454i64 * 86400;
        assert_eq!(iso8601_to_unix("2026-01-01T00:00:00Z"), expected);
    }

    #[test]
    fn now_iso8601_is_valid() {
        let ts = now_iso8601();
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
        assert!(ts.contains('T'));
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn append_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("metis-telem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("telemetry.jsonl");

        let record = TelemetryRecord {
            timestamp: now_iso8601(),
            session_id: Some("test-session".into()),
            model: "test-model".into(),
            provider: "test".into(),
            input_tokens: 42,
            output_tokens: 7,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            turns: 1,
            cost_usd: 0.0001,
            tool_calls: HashMap::new(),
        };

        // Write manually to the test path
        let line = serde_json::to_string(&record).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let loaded = load_records(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session_id.as_deref(), Some("test-session"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- BudgetStatus -----------------------------------------------------

    #[test]
    fn budget_status_no_budget_fraction_is_none() {
        let b = BudgetStatus {
            prior_usd: 0.10,
            session_usd: 0.05,
            budget_usd: None,
        };
        assert!((b.total_usd() - 0.15).abs() < 1e-9);
        assert!(b.fraction().is_none());
        assert!(!b.over_budget());
        assert!(b.summary().contains("today: $0.1500"));
        assert!(!b.summary().contains('/'));
    }

    #[test]
    fn budget_status_fraction_and_over_budget() {
        let b = BudgetStatus {
            prior_usd: 3.0,
            session_usd: 1.0,
            budget_usd: Some(5.0),
        };
        assert!((b.fraction().unwrap() - 0.8).abs() < 1e-9);
        assert!(!b.over_budget());
        let summary = b.summary();
        assert!(summary.contains("$5.00"));
        assert!(summary.contains("80%"));

        let over = BudgetStatus {
            prior_usd: 4.0,
            session_usd: 2.0,
            budget_usd: Some(5.0),
        };
        assert!(over.over_budget());
        // Fraction clamps at 1.0 so we never print e.g. "120%"
        assert!((over.fraction().unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn budget_status_zero_budget_is_not_over() {
        // Defensive: a configured budget of 0 should behave like "no
        // budget" rather than always tripping over_budget.
        let b = BudgetStatus {
            prior_usd: 1.0,
            session_usd: 0.0,
            budget_usd: Some(0.0),
        };
        assert!(!b.over_budget());
        assert_eq!(b.fraction(), Some(0.0));
    }

    #[test]
    fn today_date_matches_timestamp_prefix() {
        let ts = now_iso8601();
        let d = today_date();
        assert!(ts.len() >= 10);
        assert_eq!(d.len(), 10);
        assert_eq!(&ts[..10], d.as_str());
    }
}
