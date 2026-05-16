//! Content-addressed blob store for tool outputs.
//!
//! Large tool outputs (bash, file reads, web fetches) are written here
//! and replaced in the agent context with a short reference like
//! `ctx://abc12345`. Content is BLAKE3-hashed for dedup and integrity.
//!
//! Layout (`<workspace>/.aegis/blobs/`):
//! ```text
//! <aa>/<full-64-hex-hash>.bin       ← raw or zstd-compressed payload
//! <aa>/<full-64-hex-hash>.meta.json ← BlobMeta sidecar
//! ```
//! Files are sharded by the first two hex chars of the hash so a single
//! directory never exceeds a few thousand entries.
//!
//! User-facing IDs use the first 16 hex chars (64-bit space). Resolution
//! by prefix is supported via [`BlobStore::resolve_prefix`].

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Threshold above which payloads are zstd-compressed on disk.
const ZSTD_THRESHOLD: usize = 16 * 1024;
/// zstd level — 3 is the standard fast/good-ratio tradeoff.
const ZSTD_LEVEL: i32 = 3;
/// Length of the user-facing ID prefix (in hex chars).
pub const ID_PREFIX_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("ambiguous prefix: {0} matches {1} blobs")]
    AmbiguousPrefix(String, usize),
    #[error("invalid id: {0}")]
    InvalidId(String),
}

/// Stable, content-addressed identifier (full BLAKE3 hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobId(pub String);

impl BlobId {
    pub fn from_content(content: &[u8]) -> Self {
        let hash = blake3::hash(content);
        BlobId(hash.to_hex().to_string())
    }

    /// User-facing short form: first 16 hex chars.
    pub fn short(&self) -> &str {
        &self.0[..ID_PREFIX_LEN.min(self.0.len())]
    }

    /// Display reference, e.g. `ctx://abc1234567890def`.
    pub fn reference(&self) -> String {
        format!("ctx://{}", self.short())
    }

    fn shard(&self) -> &str {
        &self.0[..2]
    }
}

/// Metadata stored alongside each blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMeta {
    /// Originating tool, e.g. "bash", "read_file", "web_fetch".
    pub tool: String,
    /// Optional source path or URL — helps later searches and pruning.
    pub source: Option<String>,
    /// Original (uncompressed) size in bytes.
    pub original_size: u64,
    /// On-disk size in bytes (== original_size if not compressed).
    pub stored_size: u64,
    /// Whether the on-disk payload is zstd-compressed.
    pub compressed: bool,
    /// Unix epoch seconds when the blob was first written.
    pub created_at: u64,
    /// MIME or content-type hint, optional.
    pub content_type: Option<String>,
}

impl BlobMeta {
    pub fn new(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            source: None,
            original_size: 0,
            stored_size: 0,
            compressed: false,
            created_at: now_secs(),
            content_type: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }
}

/// Aggregate stats for `metis ctx stats`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BlobStats {
    pub blob_count: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    pub by_tool: Vec<(String, u64)>,
}

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (and lazily create) the blob store under
    /// `<workspace>/.metis/blobs/`.
    pub fn open(workspace: &Path) -> Result<Self, BlobError> {
        let root = workspace.join(".metis").join("blobs");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Root directory for tests/inspection.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Hash, write, and return the content-addressed ID. If the blob
    /// already exists, it is not rewritten — content is deduplicated
    /// automatically by the hash.
    pub fn store(&self, content: &[u8], mut meta: BlobMeta) -> Result<BlobId, BlobError> {
        let id = BlobId::from_content(content);
        let (data_path, meta_path) = self.paths(&id);

        if data_path.exists() {
            return Ok(id);
        }

        if let Some(parent) = data_path.parent() {
            fs::create_dir_all(parent)?;
        }

        meta.original_size = content.len() as u64;

        let stored = if content.len() >= ZSTD_THRESHOLD {
            meta.compressed = true;
            zstd::stream::encode_all(content, ZSTD_LEVEL)?
        } else {
            meta.compressed = false;
            content.to_vec()
        };
        meta.stored_size = stored.len() as u64;

        write_atomically(&data_path, &stored)?;
        write_atomically(&meta_path, serde_json::to_string(&meta)?.as_bytes())?;
        Ok(id)
    }

    /// Read content + meta. Decompresses transparently if needed.
    pub fn read(&self, id: &BlobId) -> Result<(Vec<u8>, BlobMeta), BlobError> {
        let (data_path, meta_path) = self.paths(id);
        if !data_path.exists() {
            return Err(BlobError::NotFound(id.0.clone()));
        }
        let raw = fs::read(&data_path)?;
        let meta_bytes = fs::read(&meta_path)?;
        let meta: BlobMeta = serde_json::from_slice(&meta_bytes)?;

        let content = if meta.compressed {
            zstd::stream::decode_all(raw.as_slice())?
        } else {
            raw
        };
        Ok((content, meta))
    }

    /// Read meta only, useful for `stats` and `prune`.
    pub fn read_meta(&self, id: &BlobId) -> Result<BlobMeta, BlobError> {
        let (_, meta_path) = self.paths(id);
        let bytes = fs::read(&meta_path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn exists(&self, id: &BlobId) -> bool {
        self.paths(id).0.exists()
    }

    /// Resolve a short prefix (`abc12345`) to a full `BlobId`. Returns
    /// [`BlobError::NotFound`] for no matches and
    /// [`BlobError::AmbiguousPrefix`] for >1 matches.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<BlobId, BlobError> {
        if prefix.len() < 4 {
            return Err(BlobError::InvalidId(format!(
                "prefix must be ≥4 hex chars, got {}",
                prefix.len()
            )));
        }
        let prefix = prefix.to_lowercase();
        let shard_dir = self.root.join(&prefix[..2]);
        if !shard_dir.exists() {
            return Err(BlobError::NotFound(prefix));
        }

        let mut matches = Vec::new();
        for entry in fs::read_dir(&shard_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".bin") {
                if stem.starts_with(&prefix) {
                    matches.push(stem.to_string());
                }
            }
        }
        match matches.len() {
            0 => Err(BlobError::NotFound(prefix)),
            1 => Ok(BlobId(matches.remove(0))),
            n => Err(BlobError::AmbiguousPrefix(prefix, n)),
        }
    }

    /// Iterate all blob IDs. Used by `stats` and the indexer.
    pub fn iter_ids(&self) -> Result<Vec<BlobId>, BlobError> {
        let mut ids = Vec::new();
        if !self.root.exists() {
            return Ok(ids);
        }
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(stem) = name.strip_suffix(".bin") {
                    ids.push(BlobId(stem.to_string()));
                }
            }
        }
        Ok(ids)
    }

    /// Aggregate per-tool stats across the store.
    pub fn stats(&self) -> Result<BlobStats, BlobError> {
        let mut stats = BlobStats::default();
        let mut by_tool: std::collections::BTreeMap<String, u64> = Default::default();

        for id in self.iter_ids()? {
            let meta = match self.read_meta(&id) {
                Ok(m) => m,
                Err(_) => continue, // tolerate orphaned data
            };
            stats.blob_count += 1;
            stats.total_original_bytes += meta.original_size;
            stats.total_stored_bytes += meta.stored_size;
            *by_tool.entry(meta.tool).or_insert(0) += 1;
        }
        stats.by_tool = by_tool.into_iter().collect();
        Ok(stats)
    }

    /// Delete blobs older than `ttl`. Returns the number deleted.
    pub fn prune_older_than(&self, ttl: Duration) -> Result<usize, BlobError> {
        let cutoff = now_secs().saturating_sub(ttl.as_secs());
        let mut removed = 0;
        for id in self.iter_ids()? {
            let meta = match self.read_meta(&id) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.created_at < cutoff {
                let (data_path, meta_path) = self.paths(&id);
                let _ = fs::remove_file(&data_path);
                let _ = fs::remove_file(&meta_path);
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Delete a specific blob by ID.
    pub fn delete(&self, id: &BlobId) -> Result<(), BlobError> {
        let (data_path, meta_path) = self.paths(id);
        if !data_path.exists() {
            return Err(BlobError::NotFound(id.0.clone()));
        }
        fs::remove_file(&data_path)?;
        let _ = fs::remove_file(&meta_path);
        Ok(())
    }

    fn paths(&self, id: &BlobId) -> (PathBuf, PathBuf) {
        let shard = self.root.join(id.shard());
        let data = shard.join(format!("{}.bin", id.0));
        let meta = shard.join(format!("{}.meta.json", id.0));
        (data, meta)
    }
}

fn write_atomically(target: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = target.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, target)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// keep `Read` import live for future trait-object readers
#[allow(dead_code)]
fn _force_read_use<R: Read>(_r: R) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, BlobStore) {
        let tmp = TempDir::new().unwrap();
        let store = BlobStore::open(tmp.path()).unwrap();
        (tmp, store)
    }

    #[test]
    fn store_and_read_round_trip_small() {
        let (_tmp, store) = store();
        let payload = b"hello world";
        let id = store.store(payload, BlobMeta::new("bash")).unwrap();
        let (content, meta) = store.read(&id).unwrap();
        assert_eq!(content, payload);
        assert_eq!(meta.tool, "bash");
        assert_eq!(meta.original_size, payload.len() as u64);
        assert!(!meta.compressed);
    }

    #[test]
    fn store_and_read_round_trip_large_compressed() {
        let (_tmp, store) = store();
        let payload = vec![b'a'; 64 * 1024];
        let id = store.store(&payload, BlobMeta::new("read_file")).unwrap();
        let (content, meta) = store.read(&id).unwrap();
        assert_eq!(content, payload);
        assert!(meta.compressed);
        assert!(meta.stored_size < meta.original_size);
    }

    #[test]
    fn dedup_same_content() {
        let (_tmp, store) = store();
        let id1 = store.store(b"dup", BlobMeta::new("bash")).unwrap();
        let id2 = store.store(b"dup", BlobMeta::new("bash")).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.iter_ids().unwrap().len(), 1);
    }

    #[test]
    fn id_reference_format() {
        let id = BlobId::from_content(b"abc");
        assert!(id.reference().starts_with("ctx://"));
        assert_eq!(id.reference().len(), 6 + ID_PREFIX_LEN);
    }

    #[test]
    fn resolve_prefix_unique() {
        let (_tmp, store) = store();
        let id = store.store(b"unique", BlobMeta::new("bash")).unwrap();
        let resolved = store.resolve_prefix(id.short()).unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn resolve_prefix_too_short_errors() {
        let (_tmp, store) = store();
        let _ = store.store(b"x", BlobMeta::new("bash")).unwrap();
        let err = store.resolve_prefix("abc").unwrap_err();
        assert!(matches!(err, BlobError::InvalidId(_)));
    }

    #[test]
    fn resolve_prefix_not_found() {
        let (_tmp, store) = store();
        let err = store.resolve_prefix("deadbeef").unwrap_err();
        assert!(matches!(err, BlobError::NotFound(_)));
    }

    #[test]
    fn stats_aggregates_by_tool() {
        let (_tmp, store) = store();
        store.store(b"a", BlobMeta::new("bash")).unwrap();
        store.store(b"b", BlobMeta::new("bash")).unwrap();
        store.store(b"c", BlobMeta::new("read_file")).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.blob_count, 3);
        let bash_count = stats
            .by_tool
            .iter()
            .find(|(t, _)| t == "bash")
            .map(|(_, c)| *c)
            .unwrap();
        assert_eq!(bash_count, 2);
    }

    #[test]
    fn delete_removes_blob() {
        let (_tmp, store) = store();
        let id = store.store(b"xx", BlobMeta::new("bash")).unwrap();
        assert!(store.exists(&id));
        store.delete(&id).unwrap();
        assert!(!store.exists(&id));
    }

    #[test]
    fn prune_older_than_zero_removes_all() {
        let (_tmp, store) = store();
        store.store(b"a", BlobMeta::new("bash")).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let removed = store
            .prune_older_than(std::time::Duration::from_millis(10))
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.iter_ids().unwrap().len(), 0);
    }
}
