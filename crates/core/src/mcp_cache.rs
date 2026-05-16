//! On-disk cache of `tools/list` responses for spawned MCP servers.
//!
//! `spawn_mcp_server` issues a JSON-RPC `tools/list` call after every
//! handshake. For chatty servers (Obsidian and friends) that adds
//! 200-500ms per session start on top of the process spawn itself.
//! This cache records the previous response keyed by the canonical
//! command line so callers can skip the round-trip when a recent entry
//! still applies. Live `tools/call` requests still hit the spawned
//! process — only the catalogue lookup is cached.
//!
//! Storage shape (`<workspace>/.metis/mcp-cache.json`):
//!
//! ```json
//! {
//!   "<command> <arg1> <arg2>": {
//!     "tools": [{"name": "...", "description": "...", "inputSchema": {...}}, ...],
//!     "cached_at": 1715000000
//!   },
//!   ...
//! }
//! ```
//!
//! Freshness is decided by the caller — `get` accepts a TTL and returns
//! `None` for expired entries instead of silently honouring stale
//! catalogues. Default TTL (`DEFAULT_TTL_SECS`) is one hour: long
//! enough that back-to-back boots benefit, short enough that an MCP
//! server upgrade lands within a single session.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aegis_mcp::McpToolInfo;

/// Default cache freshness window. One hour balances "back-to-back
/// boots reuse the catalogue" against "an MCP server upgrade is
/// noticed within the same workday." Callers can override per `get`.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Single cached `tools/list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCacheEntry {
    pub tools: Vec<McpToolInfo>,
    /// Unix epoch seconds when the entry was written. Used by `get` to
    /// reject stale data.
    pub cached_at: u64,
}

/// In-memory view of the on-disk cache. Loaded from the JSON file on
/// `load`, mutated via `put`, persisted on `save`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpCache {
    /// `BTreeMap` keeps the on-disk JSON ordering deterministic, which
    /// makes the file diff-friendly when checked into a workspace.
    #[serde(default)]
    pub entries: BTreeMap<String, McpCacheEntry>,
}

impl McpCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonical key for a `(command, args)` pair. Joining with a
    /// single space matches what the MCP server label uses, so future
    /// tooling can map a label back to its cache entry trivially.
    pub fn key_for(command: &str, args: &[String]) -> String {
        if args.is_empty() {
            command.to_string()
        } else {
            format!("{} {}", command, args.join(" "))
        }
    }

    /// Look up a cached entry. Returns `None` when the key is missing
    /// OR when the entry is older than `ttl_secs`. The TTL check uses
    /// the system clock; if the clock is unreadable or has rewound
    /// before `cached_at`, the entry is treated as expired so callers
    /// don't keep a stale catalogue indefinitely.
    pub fn get(&self, key: &str, ttl_secs: u64) -> Option<&Vec<McpToolInfo>> {
        let entry = self.entries.get(key)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if now < entry.cached_at {
            return None;
        }
        if now - entry.cached_at > ttl_secs {
            return None;
        }
        Some(&entry.tools)
    }

    /// Insert / replace an entry, stamping `cached_at` with the current
    /// system time. Writes are in-memory only; call `save` to persist.
    pub fn put(&mut self, key: String, tools: Vec<McpToolInfo>) {
        let cached_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.entries
            .insert(key, McpCacheEntry { tools, cached_at });
    }

    /// Drop a single entry — used when a server's `tools/list` differs
    /// from the cached version, or to manually invalidate via tooling.
    pub fn forget(&mut self, key: &str) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Resolve `<workspace>/.metis/mcp-cache.json`. The function does
    /// not create the directory — callers that want write access should
    /// `fs::create_dir_all` first (the agent already does this for
    /// `.metis/` at startup).
    pub fn path(workspace: &Path) -> PathBuf {
        workspace.join(".metis").join("mcp-cache.json")
    }

    /// Read the cache from disk. Missing file or unparseable contents
    /// produce an empty cache rather than an error — a corrupted cache
    /// is recoverable by the next successful spawn writing fresh data,
    /// and boot should never fail because the cache file went sideways.
    pub fn load(workspace: &Path) -> Self {
        let path = Self::path(workspace);
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the cache to disk. Returns the IO error if the parent
    /// directory does not exist or the file cannot be written; callers
    /// should treat a save failure as advisory (the in-memory cache
    /// remains consistent for the rest of the session) rather than
    /// fatal.
    pub fn save(&self, workspace: &Path) -> std::io::Result<()> {
        let path = Self::path(workspace);
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tool(name: &str) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            description: Some(format!("desc for {name}")),
            input_schema: Some(json!({"type": "object"})),
        }
    }

    #[test]
    fn key_for_handles_no_args() {
        assert_eq!(McpCache::key_for("playwright", &[]), "playwright");
    }

    #[test]
    fn key_for_joins_args_with_space() {
        let args = vec!["mcp".to_string(), "--verbose".to_string()];
        assert_eq!(
            McpCache::key_for("aegis-mcp-obsidian", &args),
            "aegis-mcp-obsidian mcp --verbose"
        );
    }

    #[test]
    fn put_then_get_returns_tools_within_ttl() {
        let mut cache = McpCache::new();
        cache.put("playwright".into(), vec![sample_tool("click")]);
        let got = cache.get("playwright", DEFAULT_TTL_SECS);
        assert!(got.is_some());
        assert_eq!(got.unwrap()[0].name, "click");
    }

    #[test]
    fn get_returns_none_for_unknown_key() {
        let cache = McpCache::new();
        assert!(cache.get("missing", DEFAULT_TTL_SECS).is_none());
    }

    #[test]
    fn get_returns_none_when_entry_older_than_ttl() {
        // Hand-build an entry with a cached_at deep in the past so the
        // TTL check rejects it. Avoids racing the system clock that
        // `put` reads, and pins the boundary behaviour explicitly.
        let mut cache = McpCache::new();
        cache.entries.insert(
            "stale".into(),
            McpCacheEntry {
                tools: vec![sample_tool("x")],
                cached_at: 0,
            },
        );
        // ttl=1 against an epoch-0 entry: `now - 0 > 1` is true on any
        // machine whose clock is past 1970, so the entry must expire.
        assert!(cache.get("stale", 1).is_none());
    }

    #[test]
    fn get_returns_none_when_clock_appears_to_have_rewound() {
        // Future-stamped entries (clock skew, manual edits) must not
        // be honoured — otherwise a corrupted file pins a stale
        // catalogue indefinitely.
        let mut cache = McpCache::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        cache.entries.insert(
            "futuristic".into(),
            McpCacheEntry {
                tools: vec![sample_tool("x")],
                cached_at: now + 3600,
            },
        );
        assert!(cache.get("futuristic", DEFAULT_TTL_SECS).is_none());
    }

    #[test]
    fn forget_removes_entry_and_reports_hit() {
        let mut cache = McpCache::new();
        cache.put("a".into(), vec![sample_tool("t")]);
        assert!(cache.forget("a"));
        assert!(!cache.forget("a"));
        assert!(cache.get("a", DEFAULT_TTL_SECS).is_none());
    }

    #[test]
    fn save_load_roundtrip_preserves_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".metis")).unwrap();
        let mut original = McpCache::new();
        original.put("alpha".into(), vec![sample_tool("a"), sample_tool("b")]);
        original.put("beta".into(), vec![sample_tool("c")]);
        original.save(tmp.path()).unwrap();
        let loaded = McpCache::load(tmp.path());
        assert_eq!(loaded.entries.len(), 2);
        let alpha = loaded.entries.get("alpha").unwrap();
        assert_eq!(alpha.tools.len(), 2);
        assert_eq!(alpha.tools[0].name, "a");
        let beta = loaded.entries.get("beta").unwrap();
        assert_eq!(beta.tools.len(), 1);
        assert_eq!(beta.tools[0].name, "c");
    }

    #[test]
    fn load_missing_file_returns_empty_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = McpCache::load(tmp.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_empty_cache() {
        // A corrupted cache must not break boot — the next successful
        // spawn will write fresh data over the top.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".metis")).unwrap();
        std::fs::write(McpCache::path(tmp.path()), "{not json").unwrap();
        let cache = McpCache::load(tmp.path());
        assert!(cache.entries.is_empty());
    }
}
