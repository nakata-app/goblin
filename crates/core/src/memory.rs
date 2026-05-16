//! File-based auto-memory system for cross-conversation persistence.
//!
//! Each workspace gets a `.aegis/memory/` directory containing individual
//! memory files with YAML-style frontmatter and an `MEMORY.md` index.
//!
//! Memory types:
//! - **user** — role, preferences, expertise
//! - **feedback** — corrections and confirmed approaches
//! - **project** — ongoing work context, goals, decisions
//! - **reference** — pointers to external systems
//!
//! The store is deliberately simple: plain text files that tools and
//! humans can both read. No database, no binary format.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The four kinds of memory the system tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::Feedback => write!(f, "feedback"),
            Self::Project => write!(f, "project"),
            Self::Reference => write!(f, "reference"),
        }
    }
}

impl MemoryType {
    /// Parse from a string, case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

/// Frontmatter fields stored at the top of each memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMeta {
    pub name: String,
    pub description: String,
    pub memory_type: MemoryType,
}

/// A single memory entry: metadata + body content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub meta: MemoryMeta,
    pub body: String,
    /// Filename (without directory), e.g. `feedback_testing.md`.
    pub filename: String,
}

/// One-line summary for the MEMORY.md index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub title: String,
    pub filename: String,
    pub hook: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid frontmatter in `{file}`: {reason}")]
    BadFrontmatter { file: String, reason: String },
    #[error("memory file already exists: `{0}`")]
    AlreadyExists(String),
    #[error("memory file not found: `{0}`")]
    NotFound(String),
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// Manages the `.metis/memory/` directory within a workspace.
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    /// Open (or create) the memory directory for the given workspace.
    pub fn open(workspace: &Path) -> Result<Self, MemoryError> {
        let dir = workspace.join(".metis").join("memory");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Save a new memory file and update the MEMORY.md index.
    /// Fails with `AlreadyExists` if the filename is taken.
    pub fn save(&self, entry: &MemoryEntry) -> Result<PathBuf, MemoryError> {
        let path = self.dir.join(&entry.filename);
        if path.exists() {
            return Err(MemoryError::AlreadyExists(entry.filename.clone()));
        }
        self.write_file(&path, entry)?;
        self.upsert_index(&entry.filename, &entry.meta.name, &entry.meta.description)?;
        Ok(path)
    }

    /// Overwrite an existing memory file and update the index line.
    pub fn update(&self, entry: &MemoryEntry) -> Result<PathBuf, MemoryError> {
        let path = self.dir.join(&entry.filename);
        if !path.exists() {
            return Err(MemoryError::NotFound(entry.filename.clone()));
        }
        self.write_file(&path, entry)?;
        self.upsert_index(&entry.filename, &entry.meta.name, &entry.meta.description)?;
        Ok(path)
    }

    /// Delete a memory file and remove its index line.
    pub fn delete(&self, filename: &str) -> Result<(), MemoryError> {
        let path = self.dir.join(filename);
        if !path.exists() {
            return Err(MemoryError::NotFound(filename.to_string()));
        }
        fs::remove_file(&path)?;
        self.remove_from_index(filename)?;
        Ok(())
    }

    /// Read and parse a single memory file.
    pub fn read(&self, filename: &str) -> Result<MemoryEntry, MemoryError> {
        let path = self.dir.join(filename);
        if !path.exists() {
            return Err(MemoryError::NotFound(filename.to_string()));
        }
        let content = fs::read_to_string(&path)?;
        parse_memory_file(&content, filename)
    }

    /// List all memory files, parsed. Skips files with bad frontmatter
    /// (logs a warning to stderr but keeps going).
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut entries = Vec::new();
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(e) => return Err(e.into()),
        };
        for item in read_dir {
            let item = item?;
            let name = item.file_name().to_string_lossy().to_string();
            if name == "MEMORY.md" || !name.ends_with(".md") {
                continue;
            }
            match self.read(&name) {
                Ok(entry) => entries.push(entry),
                Err(MemoryError::BadFrontmatter { file, reason }) => {
                    eprintln!("[aegis] skipping memory `{file}`: {reason}");
                }
                Err(e) => return Err(e),
            }
        }
        entries.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(entries)
    }

    /// Read the MEMORY.md index file. Returns empty string if missing.
    pub fn read_index(&self) -> Result<String, MemoryError> {
        let path = self.dir.join("MEMORY.md");
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Return up to `max` most-recent index entries from MEMORY.md as raw
    /// markdown lines (`- [Title](file.md) — hook`). New entries are
    /// appended in `upsert_index`, so the *last* N lines are the most
    /// recently created. Used by the TUI's session-start recap to surface
    /// what the agent remembers from prior sessions, claude-mem style.
    pub fn recap_lines(&self, max: usize) -> Result<Vec<String>, MemoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let raw = self.read_index()?;
        let mut lines: Vec<String> = raw
            .lines()
            .map(|l| l.trim())
            .filter(|l| l.starts_with('-'))
            .map(|s| s.to_string())
            .collect();
        let len = lines.len();
        if len > max {
            lines.drain(0..len - max);
        }
        Ok(lines)
    }

    /// Return up to `max` MEMORY.md index entries that share tokens with
    /// `query`, ordered by overlap score (highest first). Used by the
    /// TUI's per-turn recap so each user prompt surfaces just the
    /// memories that match what the user is currently asking about,
    /// mirroring claude-mem's UserPromptSubmit hook.
    ///
    /// Scoring is intentionally cheap — token overlap with stop-word
    /// filtering, no embeddings or LLM calls. Returns an empty vec when
    /// no entry shares any meaningful token with the query.
    pub fn relevant_recap_lines(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<String>, MemoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let raw = self.read_index()?;
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let q_tokens = tokenize_for_recap(query);
        if q_tokens.is_empty() {
            return Ok(Vec::new());
        }

        let mut scored: Vec<(usize, String)> = raw
            .lines()
            .map(|l| l.trim())
            .filter(|l| l.starts_with('-'))
            .map(|line| {
                let l_tokens = tokenize_for_recap(line);
                let score = l_tokens.iter().filter(|t| q_tokens.contains(*t)).count();
                (score, line.to_string())
            })
            .filter(|(score, _)| *score >= 2)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.truncate(max);
        Ok(scored.into_iter().map(|(_, l)| l).collect())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn write_file(&self, path: &Path, entry: &MemoryEntry) -> Result<(), MemoryError> {
        let content = format_memory_file(&entry.meta, &entry.body);
        let mut f = fs::File::create(path)?;
        f.write_all(content.as_bytes())?;
        Ok(())
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("MEMORY.md")
    }

    /// Add or update a single line in MEMORY.md for the given filename.
    fn upsert_index(&self, filename: &str, title: &str, hook: &str) -> Result<(), MemoryError> {
        let idx_path = self.index_path();
        let existing = match fs::read_to_string(&idx_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };

        let new_line = format!("- [{}]({}) — {}", title, filename, hook);

        // Replace existing line for this filename, or append.
        let mut found = false;
        let mut lines: Vec<String> = existing
            .lines()
            .map(|line| {
                if line.contains(&format!("({})", filename)) {
                    found = true;
                    new_line.clone()
                } else {
                    line.to_string()
                }
            })
            .collect();

        if !found {
            lines.push(new_line);
        }

        let mut f = fs::File::create(&idx_path)?;
        f.write_all(lines.join("\n").as_bytes())?;
        if !lines.is_empty() {
            f.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Remove the index line that references the given filename.
    fn remove_from_index(&self, filename: &str) -> Result<(), MemoryError> {
        let idx_path = self.index_path();
        let existing = match fs::read_to_string(&idx_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let lines: Vec<&str> = existing
            .lines()
            .filter(|line| !line.contains(&format!("({})", filename)))
            .collect();

        let mut f = fs::File::create(&idx_path)?;
        f.write_all(lines.join("\n").as_bytes())?;
        if !lines.is_empty() {
            f.write_all(b"\n")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Recap tokenization
// ---------------------------------------------------------------------------

/// Tokenize text for recap relevance scoring. Lowercase, split on
/// non-alphanumeric, drop tokens shorter than 3 chars (which weeds out
/// most stop words in both Turkish and English without a curated list)
/// and a small set of common bridge words. Returns a deduped set.
fn tokenize_for_recap(text: &str) -> std::collections::HashSet<String> {
    const STOP: &[&str] = &[
        // Turkish
        "ile", "için", "veya", "ama", "ancak", "şey", "ben", "sen", "biz",
        "siz", "onu", "bir", "ona", "bu", "şu", "ne", "var", "yok",
        "olan", "oldu", "yapıldı", "edildi", "eklendi", "silindi",
        // English
        "the", "and", "for", "with", "that", "this", "from", "into", "have",
        "has", "had", "are", "was", "were", "you", "your", "but", "not",
        "all", "any", "can", "out", "use", "via", "per", "done", "fix",
        "fixed", "added", "now", "also", "when", "then", "just",
        // High-frequency noise tokens (appear in almost every memory entry)
        "atakan", "metis", "session", "commit", "2026",
    ];
    let mut set = std::collections::HashSet::new();
    let mut current = String::new();
    let lowered = text.to_lowercase();
    for ch in lowered.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else {
            if current.chars().count() >= 3 && !STOP.contains(&current.as_str()) {
                set.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.chars().count() >= 3 && !STOP.contains(&current.as_str()) {
        set.insert(current);
    }
    set
}

// ---------------------------------------------------------------------------
// Frontmatter parsing / formatting
// ---------------------------------------------------------------------------

/// Render a memory file with YAML-style frontmatter.
pub fn format_memory_file(meta: &MemoryMeta, body: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", meta.name));
    out.push_str(&format!("description: {}\n", meta.description));
    out.push_str(&format!("type: {}\n", meta.memory_type));
    out.push_str("---\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Parse a memory file's frontmatter and body.
pub fn parse_memory_file(content: &str, filename: &str) -> Result<MemoryEntry, MemoryError> {
    let bad = |reason: &str| MemoryError::BadFrontmatter {
        file: filename.to_string(),
        reason: reason.to_string(),
    };

    // Expect `---\n...\n---\n` at the top.
    let content = content.trim_start_matches('\u{feff}'); // strip BOM
    if !content.starts_with("---") {
        return Err(bad("missing opening `---`"));
    }
    let after_open = &content[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or_else(|| bad("missing closing `---`"))?;
    let frontmatter_block = &after_open[..close_pos];
    let body_start = 3 + close_pos + 4; // "---" + "\n---"
    let body = content
        .get(body_start..)
        .unwrap_or("")
        .trim_start_matches('\n')
        .trim_start_matches('\r');

    // Parse key: value lines from frontmatter.
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut memory_type: Option<MemoryType> = None;

    for line in frontmatter_block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "name" => name = Some(val.to_string()),
                "description" => description = Some(val.to_string()),
                "type" => {
                    memory_type = Some(
                        MemoryType::parse(val)
                            .ok_or_else(|| bad(&format!("unknown type `{val}`")))?,
                    );
                }
                _ => {} // ignore unknown keys for forward compat
            }
        }
    }

    let meta = MemoryMeta {
        name: name.ok_or_else(|| bad("missing `name`"))?,
        description: description.ok_or_else(|| bad("missing `description`"))?,
        memory_type: memory_type.ok_or_else(|| bad("missing `type`"))?,
    };

    Ok(MemoryEntry {
        meta,
        body: body.to_string(),
        filename: filename.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_store() -> (TempDir, MemoryStore) {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(tmp.path()).unwrap();
        (tmp, store)
    }

    fn sample_entry(filename: &str, mt: MemoryType) -> MemoryEntry {
        MemoryEntry {
            meta: MemoryMeta {
                name: "Test memory".to_string(),
                description: "A test entry".to_string(),
                memory_type: mt,
            },
            body: "Some body content here.\n".to_string(),
            filename: filename.to_string(),
        }
    }

    // -- Frontmatter round-trip --

    #[test]
    fn format_then_parse_round_trips() {
        let meta = MemoryMeta {
            name: "User role".to_string(),
            description: "Who the user is".to_string(),
            memory_type: MemoryType::User,
        };
        let body = "Senior Rust developer.\n";
        let rendered = format_memory_file(&meta, body);
        let parsed = parse_memory_file(&rendered, "user_role.md").unwrap();
        assert_eq!(parsed.meta, meta);
        assert_eq!(parsed.body.trim(), body.trim());
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let result = parse_memory_file("no frontmatter here", "bad.md");
        assert!(matches!(result, Err(MemoryError::BadFrontmatter { .. })));
    }

    #[test]
    fn parse_rejects_missing_closing_fence() {
        let result = parse_memory_file("---\nname: x\n", "bad.md");
        assert!(matches!(result, Err(MemoryError::BadFrontmatter { .. })));
    }

    #[test]
    fn parse_rejects_unknown_type() {
        let content = "---\nname: x\ndescription: y\ntype: banana\n---\n\nbody\n";
        let result = parse_memory_file(content, "bad.md");
        assert!(matches!(result, Err(MemoryError::BadFrontmatter { .. })));
    }

    #[test]
    fn parse_rejects_missing_name() {
        let content = "---\ndescription: y\ntype: user\n---\n\nbody\n";
        let result = parse_memory_file(content, "bad.md");
        assert!(matches!(result, Err(MemoryError::BadFrontmatter { .. })));
    }

    #[test]
    fn memory_type_display_and_parse() {
        for mt in [
            MemoryType::User,
            MemoryType::Feedback,
            MemoryType::Project,
            MemoryType::Reference,
        ] {
            let s = mt.to_string();
            assert_eq!(MemoryType::parse(&s), Some(mt));
        }
        assert_eq!(MemoryType::parse("FEEDBACK"), Some(MemoryType::Feedback));
        assert_eq!(MemoryType::parse("unknown"), None);
    }

    // -- Store CRUD --

    #[test]
    fn save_creates_file_and_index_entry() {
        let (_tmp, store) = tmp_store();
        let entry = sample_entry("proj_foo.md", MemoryType::Project);
        let path = store.save(&entry).unwrap();
        assert!(path.exists());

        // Index should contain the entry.
        let idx = store.read_index().unwrap();
        assert!(idx.contains("(proj_foo.md)"));
        assert!(idx.contains("Test memory"));
    }

    #[test]
    fn save_rejects_duplicate_filename() {
        let (_tmp, store) = tmp_store();
        let entry = sample_entry("dup.md", MemoryType::User);
        store.save(&entry).unwrap();
        let result = store.save(&entry);
        assert!(matches!(result, Err(MemoryError::AlreadyExists(_))));
    }

    #[test]
    fn read_parses_saved_file() {
        let (_tmp, store) = tmp_store();
        let entry = sample_entry("read_test.md", MemoryType::Feedback);
        store.save(&entry).unwrap();
        let loaded = store.read("read_test.md").unwrap();
        assert_eq!(loaded.meta, entry.meta);
        assert_eq!(loaded.body.trim(), entry.body.trim());
    }

    #[test]
    fn read_not_found() {
        let (_tmp, store) = tmp_store();
        let result = store.read("nope.md");
        assert!(matches!(result, Err(MemoryError::NotFound(_))));
    }

    #[test]
    fn update_overwrites_content_and_index() {
        let (_tmp, store) = tmp_store();
        let mut entry = sample_entry("upd.md", MemoryType::User);
        store.save(&entry).unwrap();

        entry.meta.name = "Updated name".to_string();
        entry.body = "New body.\n".to_string();
        store.update(&entry).unwrap();

        let loaded = store.read("upd.md").unwrap();
        assert_eq!(loaded.meta.name, "Updated name");
        assert_eq!(loaded.body.trim(), "New body.");

        let idx = store.read_index().unwrap();
        assert!(idx.contains("Updated name"));
        // Old name should be gone (replaced, not duplicated).
        assert!(!idx.contains("Test memory"));
    }

    #[test]
    fn update_not_found() {
        let (_tmp, store) = tmp_store();
        let entry = sample_entry("ghost.md", MemoryType::User);
        let result = store.update(&entry);
        assert!(matches!(result, Err(MemoryError::NotFound(_))));
    }

    #[test]
    fn delete_removes_file_and_index_line() {
        let (_tmp, store) = tmp_store();
        let entry = sample_entry("del.md", MemoryType::Reference);
        let path = store.save(&entry).unwrap();
        assert!(path.exists());

        store.delete("del.md").unwrap();
        assert!(!path.exists());

        let idx = store.read_index().unwrap();
        assert!(!idx.contains("del.md"));
    }

    #[test]
    fn delete_not_found() {
        let (_tmp, store) = tmp_store();
        let result = store.delete("nope.md");
        assert!(matches!(result, Err(MemoryError::NotFound(_))));
    }

    #[test]
    fn list_returns_all_entries_sorted() {
        let (_tmp, store) = tmp_store();
        store
            .save(&sample_entry("b_second.md", MemoryType::User))
            .unwrap();
        store
            .save(&sample_entry("a_first.md", MemoryType::Feedback))
            .unwrap();
        store
            .save(&sample_entry("c_third.md", MemoryType::Project))
            .unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].filename, "a_first.md");
        assert_eq!(entries[1].filename, "b_second.md");
        assert_eq!(entries[2].filename, "c_third.md");
    }

    #[test]
    fn list_skips_bad_frontmatter_files() {
        let (_tmp, store) = tmp_store();
        store
            .save(&sample_entry("good.md", MemoryType::User))
            .unwrap();
        // Write a bad file directly.
        fs::write(store.dir().join("bad.md"), "no frontmatter").unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "good.md");
    }

    #[test]
    fn index_upsert_does_not_duplicate() {
        let (_tmp, store) = tmp_store();
        let mut entry = sample_entry("idx_test.md", MemoryType::User);
        store.save(&entry).unwrap();

        // Update twice — index should still have exactly one line for this file.
        entry.meta.name = "V2".to_string();
        store.update(&entry).unwrap();
        entry.meta.name = "V3".to_string();
        store.update(&entry).unwrap();

        let idx = store.read_index().unwrap();
        let count = idx.lines().filter(|l| l.contains("(idx_test.md)")).count();
        assert_eq!(count, 1, "index should have exactly one entry per file");
        assert!(idx.contains("V3"));
    }

    #[test]
    fn empty_store_list_returns_empty() {
        let (_tmp, store) = tmp_store();
        let entries = store.list().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn empty_store_read_index_returns_empty_string() {
        let (_tmp, store) = tmp_store();
        let idx = store.read_index().unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn frontmatter_with_extra_keys_is_tolerated() {
        let content = "---\nname: x\ndescription: y\ntype: user\nextra_key: ignored\n---\n\nbody\n";
        let entry = parse_memory_file(content, "extra.md").unwrap();
        assert_eq!(entry.meta.name, "x");
    }

    #[test]
    fn bom_prefix_is_stripped() {
        let content = "\u{feff}---\nname: x\ndescription: y\ntype: feedback\n---\n\nbody\n";
        let entry = parse_memory_file(content, "bom.md").unwrap();
        assert_eq!(entry.meta.memory_type, MemoryType::Feedback);
    }

    #[test]
    fn recap_lines_empty_when_no_index() {
        let (_tmp, store) = tmp_store();
        assert!(store.recap_lines(5).unwrap().is_empty());
    }

    #[test]
    fn recap_lines_returns_last_n_in_order() {
        let (_tmp, store) = tmp_store();
        let idx = "\
- [Old A](a.md) — first
- [Old B](b.md) — second
- [Mid C](c.md) — third
- [New D](d.md) — fourth
- [New E](e.md) — fifth
";
        fs::write(store.dir().join("MEMORY.md"), idx).unwrap();
        let lines = store.recap_lines(3).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("Mid C"));
        assert!(lines[1].contains("New D"));
        assert!(lines[2].contains("New E"));
    }

    #[test]
    fn recap_lines_skips_non_bullet_lines() {
        let (_tmp, store) = tmp_store();
        let idx = "# header\nsome prose\n- [Real](real.md) — kept\n";
        fs::write(store.dir().join("MEMORY.md"), idx).unwrap();
        let lines = store.recap_lines(5).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Real"));
    }

    #[test]
    fn recap_lines_max_zero_returns_empty() {
        let (_tmp, store) = tmp_store();
        fs::write(store.dir().join("MEMORY.md"), "- [a](a.md) — x\n").unwrap();
        assert!(store.recap_lines(0).unwrap().is_empty());
    }

    #[test]
    fn relevant_recap_returns_overlapping_entries() {
        let (_tmp, store) = tmp_store();
        let idx = "\
- [Pistachio fix](p.md) — Pistachio NanaBanana retry logic
- [Sienna pose](s.md) — Sienna pose library packshot
- [Wink Build](w.md) — Wink iOS submission ASC API
";
        fs::write(store.dir().join("MEMORY.md"), idx).unwrap();
        // Threshold is >=2 unique-token overlap (8987100); single-word query
        // "Pistachio" is not enough — query must share at least two tokens.
        let hits = store
            .relevant_recap_lines("Pistachio retry geçen sefer ne yapmıştık", 3)
            .unwrap();
        assert_eq!(hits.len(), 1, "only Pistachio entry should match");
        assert!(hits[0].contains("Pistachio"));
    }

    #[test]
    fn relevant_recap_orders_by_score() {
        let (_tmp, store) = tmp_store();
        let idx = "\
- [a](a.md) — pose library work
- [b](b.md) — pose pose library library library
- [c](c.md) — unrelated topic
";
        fs::write(store.dir().join("MEMORY.md"), idx).unwrap();
        // With >=2 threshold, both a and b reach score 2 (pose+library, dedup
        // collapses repeats in b). Order then falls back to input order.
        let hits = store.relevant_recap_lines("pose library", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.contains("pose")));
        assert!(hits.iter().any(|h| h.contains("[a]")));
        assert!(hits.iter().any(|h| h.contains("[b]")));
    }

    #[test]
    fn relevant_recap_empty_when_no_overlap() {
        let (_tmp, store) = tmp_store();
        fs::write(
            store.dir().join("MEMORY.md"),
            "- [a](a.md) — pistachio retry\n",
        )
        .unwrap();
        let hits = store.relevant_recap_lines("totally unrelated query", 3).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn relevant_recap_empty_when_query_is_short() {
        let (_tmp, store) = tmp_store();
        fs::write(
            store.dir().join("MEMORY.md"),
            "- [a](a.md) — long entry text here\n",
        )
        .unwrap();
        // Short tokens (<3 chars) and stopwords filtered → query empty.
        let hits = store.relevant_recap_lines("ne ki bu", 3).unwrap();
        assert!(hits.is_empty());
    }
}
