//! `aegis mcp <subcommand>` — read-only inspection of the on-disk
//! MCP catalogue cache that `spawn_mcp_server_with_cache`
//! write-throughs every successful boot.
//!
//! Two subcommands ship in Phase 2:
//!
//! - `aegis mcp tools` — lists every cached server with its tool
//!   names, no live spawn (so it works offline, on a stale env, in
//!   CI, in a doc generator).
//! - `aegis mcp clear`  — drops the cache file. Useful when an MCP
//!   server upgrade landed and the on-disk catalogue has gone stale.
//!
//! Stays out of the agent loop on purpose — this is the consumer for
//! the cache the v0.10 candidate set up; it neither spawns servers
//! nor mutates registry state, which means it can land before the
//! lazy-spawn refactor without any of that risk.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// Entry point invoked from `main::run` when the user types
/// `aegis mcp <sub>`. Recognised subcommands: `tools`, `clear`.
/// Unknown subcommands print usage and exit non-zero so a typo
/// doesn't silently no-op.
pub fn run(sub: &str, workspace: &Path) -> Result<()> {
    match sub {
        "tools" | "list" | "ls" => print_tools(workspace),
        "clear" | "purge" => clear(workspace),
        other => {
            bail!(
                "unknown `aegis mcp` subcommand `{other}` — supported: `tools`, `clear`"
            );
        }
    }
}

fn print_tools(workspace: &Path) -> Result<()> {
    let cache = aegis_core::McpCache::load(workspace);
    if cache.entries.is_empty() {
        println!(
            "no cached MCP catalogue at {} — start a session with at \
             least one --mcp / config-attached server to populate it.",
            aegis_core::McpCache::path(workspace).display()
        );
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!(
        "MCP cache: {} ({} server{})",
        aegis_core::McpCache::path(workspace).display(),
        cache.entries.len(),
        if cache.entries.len() == 1 { "" } else { "s" }
    );
    for (key, entry) in &cache.entries {
        let age = now.saturating_sub(entry.cached_at);
        println!(
            "\n● {key}  ({} tool{}, cached {})",
            entry.tools.len(),
            if entry.tools.len() == 1 { "" } else { "s" },
            humanise_age(age)
        );
        for t in &entry.tools {
            let desc = t
                .description
                .as_deref()
                .unwrap_or("(no description)")
                .lines()
                .next()
                .unwrap_or("(no description)");
            println!("    - {:<30} {}", t.name, desc);
        }
    }
    Ok(())
}

fn clear(workspace: &Path) -> Result<()> {
    let path = aegis_core::McpCache::path(workspace);
    if !path.exists() {
        println!("no cache file to clear: {}", path.display());
        return Ok(());
    }
    std::fs::remove_file(&path)
        .with_context(|| format!("failed to remove {}", path.display()))?;
    println!("removed {}", path.display());
    Ok(())
}

/// Render a relative-time string like `"4m ago"` / `"2h ago"`. Kept
/// stand-alone (rather than reusing main.rs's `format_relative`) so
/// the mcp_cmd module has no cross-cutting dependency on the binary.
fn humanise_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subcommand_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run("frobnicate", tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown"), "got: {msg}");
        assert!(msg.contains("tools"), "should suggest valid subs: {msg}");
    }

    #[test]
    fn humanise_age_formats_each_bucket() {
        assert_eq!(humanise_age(0), "0s ago");
        assert_eq!(humanise_age(45), "45s ago");
        assert_eq!(humanise_age(60), "1m ago");
        assert_eq!(humanise_age(3_599), "59m ago");
        assert_eq!(humanise_age(3_600), "1h ago");
        assert_eq!(humanise_age(86_400), "1d ago");
    }

    #[test]
    fn clear_on_missing_cache_succeeds_silently() {
        // No file present, no panic; matches the docstring promise
        // that callers can `aegis mcp clear` unconditionally.
        let tmp = tempfile::tempdir().unwrap();
        assert!(clear(tmp.path()).is_ok());
    }

    #[test]
    fn clear_removes_existing_cache_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".metis")).unwrap();
        let cache_path = aegis_core::McpCache::path(tmp.path());
        std::fs::write(&cache_path, "{}").unwrap();
        assert!(cache_path.exists());
        clear(tmp.path()).unwrap();
        assert!(!cache_path.exists());
    }
}
