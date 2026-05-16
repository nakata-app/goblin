//! `aegis dashboard` / `aegis stats` — terminal-based usage dashboard.
//!
//! Reads telemetry records from `~/.aegis/telemetry.jsonl` and renders
//! a compact, colourful overview of sessions, costs, token usage, and
//! tool activity. No external dependencies — just ANSI escape codes and
//! Unicode box-drawing characters.

use std::collections::HashMap;

use aegis_core::telemetry::{self, TelemetryRecord, UsageStats};

// ── Colours ──────────────────────────────────────────────────────────

/// Metis turquoise for section headers.
const TURQUOISE: &str = "\x1b[38;2;0;229;209m";
/// Dim text for secondary information.
const DIM: &str = "\x1b[2m";
/// Reset all attributes.
const RESET: &str = "\x1b[0m";

// ── Public entry point ───────────────────────────────────────────────

/// Run the dashboard command. Loads telemetry from disk and prints the
/// full dashboard to stderr. Returns `Ok(())` on success; never fails
/// hard — missing data is handled gracefully.
pub fn run() -> anyhow::Result<()> {
    let path = telemetry::telemetry_path();
    let records = match path {
        Some(ref p) if p.exists() => telemetry::load_records(p),
        _ => Vec::new(),
    };

    if records.is_empty() {
        eprintln!("{TURQUOISE}metis dashboard{RESET}");
        eprintln!();
        eprintln!("  No telemetry data yet. Run some prompts first!");
        eprintln!("  {DIM}Data is recorded in ~/.metis/telemetry.jsonl{RESET}");
        return Ok(());
    }

    let stats = UsageStats::from_records(&records);

    let mut out = String::new();
    render_header(&mut out);
    render_session_summary(&mut out, &stats, &records);
    render_cost_breakdown(&mut out, &stats, &records);
    render_daily_trend(&mut out, &stats);
    render_tool_usage(&mut out, &stats);
    render_recent_sessions(&mut out, &records);

    eprint!("{out}");
    Ok(())
}

// ── Section renderers ────────────────────────────────────────────────

fn render_header(out: &mut String) {
    out.push_str(&format!(
        "\n{TURQUOISE}╭──────────────────────────────────────────────────╮{RESET}\n"
    ));
    out.push_str(&format!(
        "{TURQUOISE}│            m e t i s   d a s h b o a r d         │{RESET}\n"
    ));
    out.push_str(&format!(
        "{TURQUOISE}╰──────────────────────────────────────────────────╯{RESET}\n"
    ));
}

fn render_session_summary(out: &mut String, stats: &UsageStats, records: &[TelemetryRecord]) {
    section_title(out, "Session Summary");

    // Date range
    let (first, last) = date_range(records);

    out.push_str(&format!(
        "  Total sessions: {bold}{}{RESET}",
        stats.total_sessions,
        bold = "\x1b[1m",
    ));
    out.push_str(&format!(
        "    Total turns: {bold}{}{RESET}\n",
        stats.total_turns,
        bold = "\x1b[1m",
    ));
    out.push_str(&format!(
        "  Tokens: {bold}{}{RESET} in / {bold}{}{RESET} out",
        format_number(stats.total_input_tokens),
        format_number(stats.total_output_tokens),
        bold = "\x1b[1m",
    ));
    out.push_str(&format!(
        "    {DIM}({} total){RESET}\n",
        format_number(stats.total_input_tokens + stats.total_output_tokens),
    ));
    out.push_str(&format!("  Date range: {DIM}{first} .. {last}{RESET}\n"));
}

fn render_cost_breakdown(out: &mut String, stats: &UsageStats, records: &[TelemetryRecord]) {
    section_title(out, "Cost Breakdown");

    out.push_str(&format!(
        "  Total cost: {bold}${:.4}{RESET}\n\n",
        stats.total_cost_usd,
        bold = "\x1b[1m",
    ));

    // Per-model table
    let model_data = aggregate_per_model(records);
    if !model_data.is_empty() {
        // Find column widths
        let header = ("Model", "Sessions", "Tokens", "Cost");
        let rows: Vec<(String, String, String, String)> = model_data
            .iter()
            .map(|(model, sessions, tokens, cost)| {
                (
                    model.clone(),
                    sessions.to_string(),
                    format_number(*tokens),
                    format!("${:.4}", cost),
                )
            })
            .collect();

        let w0 = rows
            .iter()
            .map(|r| r.0.len())
            .max()
            .unwrap_or(5)
            .max(header.0.len());
        let w1 = rows
            .iter()
            .map(|r| r.1.len())
            .max()
            .unwrap_or(8)
            .max(header.1.len());
        let w2 = rows
            .iter()
            .map(|r| r.2.len())
            .max()
            .unwrap_or(6)
            .max(header.2.len());
        let w3 = rows
            .iter()
            .map(|r| r.3.len())
            .max()
            .unwrap_or(4)
            .max(header.3.len());

        // Header
        out.push_str(&format!(
            "  {TURQUOISE}{:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}{RESET}\n",
            header.0, header.1, header.2, header.3,
        ));
        out.push_str(&format!(
            "  {DIM}{}{RESET}\n",
            "─".repeat(w0 + w1 + w2 + w3 + 6)
        ));

        for (m, s, t, c) in &rows {
            out.push_str(&format!(
                "  {:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}\n",
                m, s, t, c,
            ));
        }
    }
}

fn render_daily_trend(out: &mut String, stats: &UsageStats) {
    if stats.daily_cost.is_empty() {
        return;
    }

    section_title(out, "Daily Cost Trend");

    let mut days: Vec<(&String, &f64)> = stats.daily_cost.iter().collect();
    days.sort_by(|a, b| a.0.cmp(b.0));

    // Show last 14 days at most
    let days: Vec<_> = if days.len() > 14 {
        days[days.len() - 14..].to_vec()
    } else {
        days
    };

    let max_cost = days.iter().map(|(_, c)| **c).fold(0.0_f64, f64::max);

    if max_cost == 0.0 {
        return;
    }

    // Bar chart: max bar width = 30 chars
    let bar_width = 30;
    for (day, cost) in &days {
        let ratio = **cost / max_cost;
        let filled = (ratio * bar_width as f64).round() as usize;
        let bar = render_bar(filled, bar_width);
        // Shorten day: show MM-DD only
        let short_day = if day.len() >= 10 { &day[5..10] } else { day };
        out.push_str(&format!("  {DIM}{short_day}{RESET} {bar} ${:.4}\n", cost));
    }
}

fn render_tool_usage(out: &mut String, stats: &UsageStats) {
    if stats.tool_counts.is_empty() {
        return;
    }

    section_title(out, "Tool Usage");

    let mut tools: Vec<(&String, &u64)> = stats.tool_counts.iter().collect();
    tools.sort_by(|a, b| b.1.cmp(a.1));

    let max_count = tools.first().map(|(_, c)| **c).unwrap_or(1);
    let bar_width = 20;

    // Name column width: max of tool names, capped at 20
    let name_w = tools
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(4)
        .min(20);

    // Header
    out.push_str(&format!(
        "  {TURQUOISE}{:<name_w$}  {:>6}  {}{RESET}\n",
        "Tool", "Calls", "",
    ));
    out.push_str(&format!(
        "  {DIM}{}{RESET}\n",
        "─".repeat(name_w + bar_width + 10)
    ));

    // Show top 15 tools
    for (name, count) in tools.iter().take(15) {
        let ratio = **count as f64 / max_count as f64;
        let filled = (ratio * bar_width as f64).round() as usize;
        let bar = render_bar(filled, bar_width);
        let display_name = if name.len() > name_w {
            &name[..name_w]
        } else {
            name
        };
        out.push_str(&format!(
            "  {:<name_w$}  {:>6}  {bar}\n",
            display_name, count,
        ));
    }

    if tools.len() > 15 {
        out.push_str(&format!(
            "  {DIM}...and {} more tools{RESET}\n",
            tools.len() - 15
        ));
    }
}

fn render_recent_sessions(out: &mut String, records: &[TelemetryRecord]) {
    section_title(out, "Recent Sessions");

    // Last 10 records (they're in chronological order from the file)
    let recent: Vec<_> = if records.len() > 10 {
        records[records.len() - 10..].iter().rev().collect()
    } else {
        records.iter().rev().collect()
    };

    // Column widths
    let id_w = 12;
    let date_w = 16;

    out.push_str(&format!(
        "  {TURQUOISE}{:<id_w$}  {:<date_w$}  {:>5}  {:>10}  {:>8}{RESET}\n",
        "Session", "Date", "Turns", "Model", "Cost",
    ));
    out.push_str(&format!(
        "  {DIM}{}{RESET}\n",
        "─".repeat(id_w + date_w + 5 + 10 + 8 + 8)
    ));

    for r in recent {
        let sid = r.session_id.as_deref().unwrap_or("-");
        let display_id = if sid.len() > id_w { &sid[..id_w] } else { sid };
        // Timestamp: show date + time (first 16 chars)
        let display_date = if r.timestamp.len() >= 16 {
            &r.timestamp[..16]
        } else {
            &r.timestamp
        };
        // Short model name
        let short_model = shorten_model(&r.model);
        let display_model = if short_model.len() > 10 {
            &short_model[..10]
        } else {
            &short_model
        };

        out.push_str(&format!(
            "  {:<id_w$}  {:<date_w$}  {:>5}  {:>10}  ${:>7.4}\n",
            display_id, display_date, r.turns, display_model, r.cost_usd,
        ));
    }

    out.push('\n');
}

// ── Helpers ──────────────────────────────────────────────────────────

fn section_title(out: &mut String, title: &str) {
    out.push_str(&format!("\n{TURQUOISE}  ── {title} ──{RESET}\n\n"));
}

/// Aggregate per-model: (model, sessions, total_tokens, cost)
fn aggregate_per_model(records: &[TelemetryRecord]) -> Vec<(String, usize, u64, f64)> {
    let mut map: HashMap<String, (usize, u64, f64)> = HashMap::new();
    for r in records {
        let entry = map.entry(r.model.clone()).or_default();
        entry.0 += 1;
        entry.1 += (r.input_tokens + r.output_tokens) as u64;
        entry.2 += r.cost_usd;
    }
    let mut rows: Vec<_> = map
        .into_iter()
        .map(|(model, (sessions, tokens, cost))| (model, sessions, tokens, cost))
        .collect();
    rows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

/// Extract date range (first timestamp, last timestamp) from records.
fn date_range(records: &[TelemetryRecord]) -> (String, String) {
    if records.is_empty() {
        return ("n/a".into(), "n/a".into());
    }
    let first = records
        .iter()
        .map(|r| &r.timestamp)
        .min()
        .cloned()
        .unwrap_or_default();
    let last = records
        .iter()
        .map(|r| &r.timestamp)
        .max()
        .cloned()
        .unwrap_or_default();
    // Trim to date only (first 10 chars)
    let first = if first.len() >= 10 {
        first[..10].to_string()
    } else {
        first
    };
    let last = if last.len() >= 10 {
        last[..10].to_string()
    } else {
        last
    };
    (first, last)
}

/// Format a number with thousands separators: 1234567 → "1,234,567".
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

/// Render a horizontal bar using Unicode block characters.
/// `filled` is the number of full positions (0..=width).
pub fn render_bar(filled: usize, width: usize) -> String {
    let filled = filled.min(width);
    let empty = width - filled;
    let mut bar = String::with_capacity(width + 10);
    bar.push_str(TURQUOISE);
    for _ in 0..filled {
        bar.push('█');
    }
    bar.push_str(DIM);
    for _ in 0..empty {
        bar.push('░');
    }
    bar.push_str(RESET);
    bar
}

/// Shorten a model name for display: strip common prefixes.
fn shorten_model(model: &str) -> String {
    model
        .strip_prefix("accounts/fireworks/models/")
        .or_else(|| model.strip_prefix("deepseek/"))
        .or_else(|| model.strip_prefix("openai/"))
        .unwrap_or(model)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_records() -> Vec<TelemetryRecord> {
        vec![
            TelemetryRecord {
                timestamp: "2026-04-08T10:00:00Z".into(),
                session_id: Some("sess-001".into()),
                model: "deepseek-chat".into(),
                provider: "deepseek".into(),
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                turns: 3,
                cost_usd: 0.0008,
                tool_calls: [("bash".to_string(), 5), ("read_file".to_string(), 3)]
                    .into_iter()
                    .collect(),
            },
            TelemetryRecord {
                timestamp: "2026-04-09T14:30:00Z".into(),
                session_id: Some("sess-002".into()),
                model: "deepseek-chat".into(),
                provider: "deepseek".into(),
                input_tokens: 2000,
                output_tokens: 800,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                turns: 5,
                cost_usd: 0.0015,
                tool_calls: [
                    ("bash".to_string(), 2),
                    ("edit_file".to_string(), 4),
                    ("read_file".to_string(), 7),
                ]
                .into_iter()
                .collect(),
            },
            TelemetryRecord {
                timestamp: "2026-04-10T09:00:00Z".into(),
                session_id: Some("sess-003".into()),
                model: "claude-sonnet-4-5".into(),
                provider: "anthropic".into(),
                input_tokens: 500,
                output_tokens: 200,
                cache_read_tokens: 3000,
                cache_write_tokens: 0,
                turns: 2,
                cost_usd: 0.0045,
                tool_calls: [("bash".to_string(), 1)].into_iter().collect(),
            },
        ]
    }

    #[test]
    fn format_number_with_separators() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(100), "100");
    }

    #[test]
    fn render_bar_full_and_empty() {
        let bar = render_bar(10, 10);
        assert!(bar.contains('█'));
        assert!(!bar.contains('░'));

        let bar = render_bar(0, 10);
        assert!(!bar.contains('█'));
        assert!(bar.contains('░'));
    }

    #[test]
    fn render_bar_partial() {
        let bar = render_bar(5, 10);
        // Count block chars (ignoring ANSI escapes)
        let stripped = strip_ansi(&bar);
        let filled: usize = stripped.chars().filter(|c| *c == '█').count();
        let empty: usize = stripped.chars().filter(|c| *c == '░').count();
        assert_eq!(filled, 5);
        assert_eq!(empty, 5);
    }

    #[test]
    fn render_bar_clamped_to_width() {
        let bar = render_bar(100, 10);
        let stripped = strip_ansi(&bar);
        let filled: usize = stripped.chars().filter(|c| *c == '█').count();
        assert_eq!(filled, 10);
    }

    #[test]
    fn date_range_extracts_first_and_last() {
        let records = sample_records();
        let (first, last) = date_range(&records);
        assert_eq!(first, "2026-04-08");
        assert_eq!(last, "2026-04-10");
    }

    #[test]
    fn date_range_empty_records() {
        let (first, last) = date_range(&[]);
        assert_eq!(first, "n/a");
        assert_eq!(last, "n/a");
    }

    #[test]
    fn aggregate_per_model_groups_correctly() {
        let records = sample_records();
        let agg = aggregate_per_model(&records);
        // Should have 2 models: deepseek-chat (2 sessions) and sonnet (1)
        assert_eq!(agg.len(), 2);

        // Sorted by cost descending — sonnet ($0.0045) before deepseek ($0.0023)
        assert!(agg[0].0.contains("sonnet") || agg[0].3 >= agg[1].3);

        // Find deepseek entry
        let ds = agg.iter().find(|r| r.0 == "deepseek-chat").unwrap();
        assert_eq!(ds.1, 2); // 2 sessions
        assert_eq!(ds.2, 4300); // 3000+1300 tokens (input+output)
    }

    #[test]
    fn shorten_model_strips_prefixes() {
        assert_eq!(
            shorten_model("accounts/fireworks/models/deepseek-chat"),
            "deepseek-chat"
        );
        assert_eq!(shorten_model("deepseek-chat"), "deepseek-chat");
        assert_eq!(shorten_model("claude-sonnet-4-5"), "claude-sonnet-4-5");
    }

    #[test]
    fn full_dashboard_renders_without_panic() {
        let records = sample_records();
        let stats = UsageStats::from_records(&records);

        let mut out = String::new();
        render_header(&mut out);
        render_session_summary(&mut out, &stats, &records);
        render_cost_breakdown(&mut out, &stats, &records);
        render_daily_trend(&mut out, &stats);
        render_tool_usage(&mut out, &stats);
        render_recent_sessions(&mut out, &records);

        // Basic smoke checks on the output
        assert!(out.contains("Session Summary"));
        assert!(out.contains("Cost Breakdown"));
        assert!(out.contains("Tool Usage"));
        assert!(out.contains("Recent Sessions"));
        assert!(out.contains("deepseek-chat"));
        assert!(out.contains("bash"));
    }

    #[test]
    fn empty_data_renders_gracefully() {
        let stats = UsageStats::default();
        let records: Vec<TelemetryRecord> = Vec::new();

        let mut out = String::new();
        render_header(&mut out);
        render_session_summary(&mut out, &stats, &records);
        render_cost_breakdown(&mut out, &stats, &records);
        render_daily_trend(&mut out, &stats);
        render_tool_usage(&mut out, &stats);
        render_recent_sessions(&mut out, &records);

        // Should not panic and should contain headers
        assert!(out.contains("Session Summary"));
    }

    #[test]
    fn daily_trend_shows_bars() {
        let records = sample_records();
        let stats = UsageStats::from_records(&records);

        let mut out = String::new();
        render_daily_trend(&mut out, &stats);

        // Should contain at least one bar character
        let stripped = strip_ansi(&out);
        assert!(
            stripped.contains('█') || stripped.contains('░'),
            "expected bar chars in daily trend"
        );
        // Should contain day labels
        assert!(out.contains("04-08") || out.contains("04-09") || out.contains("04-10"));
    }

    /// Strip ANSI escape sequences for testing display content.
    fn strip_ansi(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // Skip until 'm'
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                result.push(ch);
            }
        }
        result
    }
}
