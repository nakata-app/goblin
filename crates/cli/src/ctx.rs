//! `aegis ctx` subcommand — inspect and prune the context blob store.
//!
//! Subcommands:
//! - `aegis ctx search <query> [--tool <name>] [--limit N]` — BM25 over
//!   stored tool outputs, top-K hits.
//! - `aegis ctx show <id-prefix>` — full content (decompressed) of a
//!   stashed blob.
//! - `aegis ctx stats` — per-tool counts + on-disk vs. original size.
//! - `aegis ctx prune [--older-than 7d]` — delete blobs older than the
//!   given duration. Default TTL: 7 days.
//!
//! The store and index live under `<workspace>/.aegis/blobs/` and
//! `<workspace>/.aegis/blobs_index/` respectively.

use std::path::Path;

use anyhow::{bail, Context, Result};
use aegis_core::{BlobIndex, BlobStore};

const TURQUOISE: &str = "\x1b[38;2;0;229;209m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

const DEFAULT_SEARCH_LIMIT: usize = 10;

pub fn run_ctx_command(args: &[&str], workspace: &Path) -> Result<()> {
    match args.first().copied() {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("search") => cmd_search(&args[1..], workspace),
        Some("show") => cmd_show(&args[1..], workspace),
        Some("stats") => cmd_stats(workspace),
        Some("prune") => cmd_prune(&args[1..], workspace),
        Some(other) => {
            bail!("unknown ctx subcommand `{other}` — try: search, show, stats, prune");
        }
    }
}

fn print_help() {
    println!(
        "{TURQUOISE}metis ctx{RESET} — inspect the tool-output blob store

usage:
  metis ctx search <query> [--tool <name>] [--limit <n>]
  metis ctx show   <id-prefix>
  metis ctx stats
  metis ctx prune  [--older-than <duration>]

duration syntax: 7d | 12h | 30m | 60s | 1w (default for prune: 7d)

examples:
  metis ctx search 'cargo test'
  metis ctx search 'rust async' --tool bash --limit 5
  metis ctx show 7e1c8f
  metis ctx prune --older-than 14d"
    );
}

// ---------------------------------------------------------------
// search
// ---------------------------------------------------------------

fn cmd_search(args: &[&str], workspace: &Path) -> Result<()> {
    let mut query_parts: Vec<&str> = Vec::new();
    let mut tool_filter: Option<String> = None;
    let mut limit: usize = DEFAULT_SEARCH_LIMIT;

    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--tool" | "-t" => {
                i += 1;
                let v = args
                    .get(i)
                    .with_context(|| "--tool needs a value (e.g. --tool bash)")?;
                tool_filter = Some((*v).to_string());
            }
            "--limit" | "-n" => {
                i += 1;
                let v = args.get(i).with_context(|| "--limit needs a number")?;
                limit = v.parse().context("--limit must be a positive integer")?;
            }
            other => query_parts.push(other),
        }
        i += 1;
    }

    if query_parts.is_empty() {
        bail!("usage: metis ctx search <query> [--tool <name>] [--limit <n>]");
    }
    let query = query_parts.join(" ");

    let index = BlobIndex::open(workspace).context("could not open blob index")?;
    let hits = index
        .search(&query, limit, tool_filter.as_deref())
        .context("search failed")?;

    if hits.is_empty() {
        eprintln!("{DIM}(no matches for {:?}){RESET}", query);
        return Ok(());
    }

    println!(
        "{GREEN}{n}{RESET} hit(s) for {DIM}{:?}{RESET}",
        query,
        n = hits.len()
    );
    for (i, h) in hits.iter().enumerate() {
        let src = h.source.as_deref().unwrap_or("(no source)");
        println!(
            "  {DIM}{idx:>2}.{RESET} {TURQUOISE}ctx://{short}{RESET}  {GREEN}{tool}{RESET}  {YELLOW}{score:.2}{RESET}  {DIM}{src}{RESET}",
            idx = i + 1,
            short = &h.id.0[..h.id.0.len().min(aegis_core::ID_PREFIX_LEN)],
            tool = h.tool,
            score = h.score,
            src = truncate(src, 80),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------
// show
// ---------------------------------------------------------------

fn cmd_show(args: &[&str], workspace: &Path) -> Result<()> {
    let prefix = args
        .first()
        .copied()
        .with_context(|| "usage: metis ctx show <id-prefix>")?;
    let prefix = prefix.trim_start_matches("ctx://");

    let store = BlobStore::open(workspace).context("could not open blob store")?;
    let id = store
        .resolve_prefix(prefix)
        .with_context(|| format!("could not resolve `{prefix}`"))?;
    let (content, meta) = store.read(&id)?;
    let body = String::from_utf8_lossy(&content);

    eprintln!(
        "{DIM}# {tool} — {bytes} bytes ({stored} on disk{compressed}) — created at {created}{RESET}",
        tool = meta.tool,
        bytes = meta.original_size,
        stored = meta.stored_size,
        compressed = if meta.compressed { ", zstd" } else { "" },
        created = meta.created_at,
    );
    if let Some(s) = &meta.source {
        eprintln!("{DIM}# source: {s}{RESET}");
    }
    println!("{}", body);
    Ok(())
}

// ---------------------------------------------------------------
// stats
// ---------------------------------------------------------------

fn cmd_stats(workspace: &Path) -> Result<()> {
    let store = BlobStore::open(workspace).context("could not open blob store")?;
    let stats = store.stats()?;

    let saved = stats
        .total_original_bytes
        .saturating_sub(stats.total_stored_bytes);
    let ratio = if stats.total_original_bytes > 0 {
        100.0 * stats.total_stored_bytes as f64 / stats.total_original_bytes as f64
    } else {
        0.0
    };

    println!("{TURQUOISE}context blob store{RESET}");
    println!("  blobs:         {}", stats.blob_count);
    println!(
        "  original size: {}",
        human_bytes(stats.total_original_bytes)
    );
    println!(
        "  on-disk size:  {} ({:.1}% of original)",
        human_bytes(stats.total_stored_bytes),
        ratio
    );
    println!("  saved:         {GREEN}{}{RESET}", human_bytes(saved));

    if !stats.by_tool.is_empty() {
        println!();
        println!("{TURQUOISE}per tool{RESET}");
        let max = stats
            .by_tool
            .iter()
            .map(|(t, _)| t.len())
            .max()
            .unwrap_or(0);
        for (tool, count) in &stats.by_tool {
            println!("  {:<width$}  {}", tool, count, width = max);
        }
    }

    // Index doc count is informational — usually equals blob_count
    // unless indexing failed somewhere.
    if let Ok(idx) = BlobIndex::open(workspace) {
        if let Ok(n) = idx.doc_count() {
            println!();
            println!("{DIM}index: {n} doc(s){RESET}");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------
// prune
// ---------------------------------------------------------------

fn cmd_prune(args: &[&str], workspace: &Path) -> Result<()> {
    let mut older_than = std::time::Duration::from_secs(7 * 24 * 3600);
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--older-than" => {
                i += 1;
                let v = args
                    .get(i)
                    .with_context(|| "--older-than needs a value (e.g. 7d, 12h, 30m)")?;
                older_than = parse_duration(v)?;
            }
            other => bail!("unknown flag `{other}` — try --older-than <duration>"),
        }
        i += 1;
    }

    let store = BlobStore::open(workspace).context("could not open blob store")?;
    let removed = store.prune_older_than(older_than)?;

    // Best-effort index cleanup: re-index sweep is overkill; leave stale
    // doc entries alone — they'll surface as broken `show` references the
    // user can ignore. A future hygiene pass can rebuild the index.
    eprintln!(
        "{TURQUOISE}pruned {removed} blob(s){RESET} {DIM}(older than {}){RESET}",
        format_duration(older_than)
    );
    Ok(())
}

// ---------------------------------------------------------------
// helpers
// ---------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.saturating_sub(1);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut n = n as f64;
    let mut u = 0;
    while n >= 1024.0 && u < UNITS.len() - 1 {
        n /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n as u64, UNITS[u])
    } else {
        format!("{:.1} {}", n, UNITS[u])
    }
}

fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let (num_part, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num_part
        .parse()
        .with_context(|| format!("could not parse duration `{s}`"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        "w" => n * 7 * 86_400,
        other => bail!("unknown duration unit `{other}` — try s, m, h, d, w"),
    };
    Ok(std::time::Duration::from_secs(secs))
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 7 * 86_400 && secs % (7 * 86_400) == 0 {
        format!("{}w", secs / (7 * 86_400))
    } else if secs >= 86_400 && secs % 86_400 == 0 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("60").unwrap().as_secs(), 60);
        assert_eq!(parse_duration("60s").unwrap().as_secs(), 60);
        assert_eq!(parse_duration("5m").unwrap().as_secs(), 300);
        assert_eq!(parse_duration("2h").unwrap().as_secs(), 7200);
        assert_eq!(parse_duration("3d").unwrap().as_secs(), 259_200);
        assert_eq!(parse_duration("2w").unwrap().as_secs(), 1_209_600);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5y").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn format_duration_round_trips_canonical_units() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(7 * 86_400)),
            "1w"
        );
        assert_eq!(
            format_duration(std::time::Duration::from_secs(3 * 86_400)),
            "3d"
        );
        assert_eq!(format_duration(std::time::Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn truncate_handles_boundary() {
        assert_eq!(truncate("hello", 10), "hello");
        let t = truncate("0123456789abcdef", 6);
        assert!(t.ends_with('…'));
        // '…' is 3 bytes in UTF-8, so the byte budget is `max-1` ASCII +
        // 3 ellipsis bytes. For max=6: 5 + 3 = 8.
        assert!(t.len() <= 8);
        // And we definitely don't return the full input.
        assert!(t.chars().count() < "0123456789abcdef".chars().count());
    }
}
