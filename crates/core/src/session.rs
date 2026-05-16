//! JSONL session persistence.
//!
//! Every message the agent touches — system, user, assistant, tool —
//! can be appended to a newline-delimited JSON file so the run can be
//! resumed later or inspected after the fact. One line per
//! [`ChatMessage`], in wire-format order; nothing fancy, nothing
//! lossy.
//!
//! Design choices:
//!
//! * **JSONL, not a single JSON array.** Append-only writes survive a
//!   crash mid-turn without corrupting the file, and `jq -s` or a
//!   tail-based tool can stream the log trivially. A single-array
//!   format would force a rewrite on every append.
//! * **Load replays into memory.** The agent needs the full transcript
//!   in RAM to build the next request, so `load` deserializes every
//!   line up front. Sessions are bounded by the context window and
//!   compacted when they get big, so this is never a real cost.
//! * **Workspace-scoped.** Sessions live under
//!   `<workspace>/.aegis/sessions/<id>.jsonl`. Keeping them next to the
//!   project means resuming a session and switching to a different
//!   repository can't silently mix unrelated conversations.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use aegis_api::{ChatMessage, Role};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error on `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("session line {line} is not valid JSON: {source}")]
    Decode {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Summary row for a single session on disk, used by
/// [`SessionStore::list`] to power the REPL's `/sessions` view.
/// `message_count` is derived from a cheap non-blank line count so
/// listing a workspace with many large sessions stays fast.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub message_count: usize,
    pub modified: Option<std::time::SystemTime>,
    pub parent_id: Option<String>,
}

/// Atakan: Trigger B return value — a session with content that was
/// never (or not recently) ingested into mnemonics. Caller decides how
/// to surface it (banner, slash command, silent skip).
#[derive(Debug, Clone)]
pub struct UnsavedSessionHint {
    pub id: String,
    pub path: PathBuf,
    pub message_count: usize,
    /// Seconds since the JSONL was last written.
    pub age_secs: u64,
}

/// Counts produced by a tolerant load. `recovered` is the number of
/// JSONL lines that parsed cleanly into messages; `skipped` is the
/// number that were dropped because they were truncated, not valid
/// UTF-8, or not valid JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    pub recovered: usize,
    pub skipped: usize,
}

/// Optional metadata stored alongside a session in a `.meta.json` file.
/// Currently tracks fork parentage; future fields can be added without
/// touching the JSONL message format.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    /// The session id this was forked from (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Message index in the parent where the fork was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_point: Option<usize>,
    /// Atakan: last permission mode the user was in when this session
    /// was running. On `--resume`, the TUI restores this so Bypass /
    /// Plan / AcceptEdits state survives Ctrl+C and process restart.
    /// Values: "default" | "accept-edits" | "plan" | "bypass". None →
    /// Default mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Atakan: Trigger B — set to the unix-seconds timestamp of the last
    /// successful `mnemonics ingest` for this session. If the JSONL's
    /// mtime is newer than this, the session has unsaved content and
    /// `/recall-prev` can offer to summarize it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_save_ingested_unix: Option<u64>,
}

/// A session log bound to a single `.jsonl` file on disk. Holds an
/// in-memory mirror of the messages so the agent can seed a resumed
/// transcript without rereading the file.
pub struct SessionStore {
    path: PathBuf,
    messages: Vec<ChatMessage>,
    recovery: RecoveryStats,
    meta: SessionMeta,
}

impl SessionStore {
    /// Opens (and creates if needed) a session file. If the file
    /// already contains JSONL messages, they are loaded into memory
    /// so `messages()` returns the replay.
    pub fn open(workspace_root: &Path, id: &str) -> Result<Self, SessionError> {
        let dir = workspace_root.join(".metis").join("sessions");
        fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = dir.join(format!("{id}.jsonl"));
        let (messages, recovery) = if path.exists() {
            Self::load(&path)?
        } else {
            // Touch the file so subsequent appends have something to open.
            File::create(&path).map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
            (Vec::new(), RecoveryStats::default())
        };
        let meta = Self::load_meta(&path);
        Ok(Self {
            path,
            messages,
            recovery,
            meta,
        })
    }

    /// Stats from the last `open` load. Zero/zero on a fresh file.
    /// Callers (CLI) inspect this on startup to warn the user when
    /// `--resume` had to skip corrupt lines.
    pub fn recovery_stats(&self) -> RecoveryStats {
        self.recovery
    }

    /// The session id derived from the filename stem.
    pub fn id(&self) -> &str {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    }

    /// Read-only access to session metadata.
    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    /// Path for the companion `.meta.json` sidecar.
    fn meta_path(jsonl_path: &Path) -> PathBuf {
        jsonl_path.with_extension("meta.json")
    }

    /// Load meta sidecar if it exists, otherwise return defaults.
    fn load_meta(jsonl_path: &Path) -> SessionMeta {
        let mp = Self::meta_path(jsonl_path);
        if mp.exists() {
            if let Ok(bytes) = fs::read(&mp) {
                if let Ok(meta) = serde_json::from_slice(&bytes) {
                    return meta;
                }
            }
        }
        SessionMeta::default()
    }

    /// Atakan: TUI'den çağrı için public setter. Permission mode değişince
    /// session sidecar'ı güncel kalsın diye write yapar. Hata sessiz olamaz
    /// — fail-loud (premortem F3 dersi).
    pub fn set_permission_mode(&mut self, mode: Option<String>) -> Result<(), SessionError> {
        self.meta.permission_mode = mode;
        self.save_meta()
    }

    /// Atakan: Trigger B — convenience for `/recall-prev`. Returns the
    /// most recent (user, assistant) text pair from the loaded session,
    /// using empty strings if either side is absent. Strips multimodal
    /// payloads — only `content` text is considered.
    pub fn last_user_assistant_pair(&self) -> (String, String) {
        let mut last_user = String::new();
        let mut last_assistant = String::new();
        for msg in self.messages.iter().rev() {
            let text = msg.content.clone().unwrap_or_default();
            match msg.role {
                Role::User if last_user.is_empty() => last_user = text,
                Role::Assistant if last_assistant.is_empty() => last_assistant = text,
                _ => {}
            }
            if !last_user.is_empty() && !last_assistant.is_empty() {
                break;
            }
        }
        (last_user, last_assistant)
    }

    /// Atakan: Trigger B — mark this session as ingested at `now`. Called
    /// after a successful `mnemonics ingest` (session-end keyword path or
    /// `/recall-prev` recovery). The mtime/last_save comparison is what
    /// drives the "unsaved" hint on next boot.
    pub fn mark_ingested_now(&mut self) -> Result<(), SessionError> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.meta.last_save_ingested_unix = Some(secs);
        self.save_meta()
    }

    /// Persist the current metadata to the sidecar file.
    fn save_meta(&self) -> Result<(), SessionError> {
        let mp = Self::meta_path(&self.path);
        let json = serde_json::to_string_pretty(&self.meta).unwrap();
        fs::write(&mp, json).map_err(|source| SessionError::Io {
            path: mp.display().to_string(),
            source,
        })
    }

    /// Generates a short, sortable session id of the form
    /// `YYYYMMDDHHMMSS-<rand>`. Deterministic enough for humans to
    /// grep, random enough to avoid collisions within the same second.
    pub fn new_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Process-wide monotonic counter so two `new_id()` calls in
        // the same nanosecond from the same process can never collide.
        // Session 22 fix for the `new_id_is_unique_and_shortish` flake.
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let nanos = now.subsec_nanos();
        let pid = std::process::id();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("{secs:x}-{pid:x}-{nanos:x}-{seq:x}")
    }

    /// The on-disk location of the session file. Exposed so the CLI
    /// can print it to the user on startup.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read-only view of the messages currently mirrored in memory.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Replace the in-memory transcript and rewrite the on-disk JSONL.
    /// Used by manual compaction (`/compact`).
    ///
    /// Atomic: writes to a sibling `.tmp` file first, then renames into
    /// place so a crash mid-write can't leave the session half-gone.
    /// The in-memory transcript is only updated after the disk rewrite
    /// succeeds — previously a disk error would silently diverge memory
    /// from disk, making `/compact` look successful while the next
    /// `--resume` re-loaded the old uncompacted transcript.
    pub fn replace_messages(&mut self, messages: Vec<ChatMessage>) -> Result<(), SessionError> {
        use std::io::Write;
        let tmp_path = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&tmp_path).map_err(|source| SessionError::Io {
                path: tmp_path.display().to_string(),
                source,
            })?;
            for msg in &messages {
                let line = serde_json::to_string(msg)
                    .map_err(|source| SessionError::Decode { line: 0, source })?;
                writeln!(file, "{line}").map_err(|source| SessionError::Io {
                    path: tmp_path.display().to_string(),
                    source,
                })?;
            }
            file.sync_all().map_err(|source| SessionError::Io {
                path: tmp_path.display().to_string(),
                source,
            })?;
        }
        fs::rename(&tmp_path, &self.path).map_err(|source| SessionError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        self.messages = messages;
        Ok(())
    }

    /// Append one message, both to the on-disk file and to the
    /// in-memory mirror. Called by the agent loop for every
    /// system/user/assistant/tool message it produces.
    pub fn append(&mut self, msg: &ChatMessage) -> Result<(), SessionError> {
        let line = serde_json::to_string(msg).map_err(|source| SessionError::Decode {
            line: self.messages.len() + 1,
            source,
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| SessionError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        // If a previous run died mid-`append` the file may end with a
        // truncated line that was never `\n`-terminated. The tolerant
        // loader already skips that bad tail in memory, but we must
        // also keep it from being glued onto the *next* append on disk
        // — otherwise the new line would be silently merged into the
        // broken one and skipped on the subsequent reload.
        let needs_leading_newline = match std::fs::metadata(&self.path) {
            Ok(m) if m.len() > 0 => {
                let mut tail = [0u8; 1];
                let mut f = File::open(&self.path).map_err(|source| SessionError::Io {
                    path: self.path.display().to_string(),
                    source,
                })?;
                use std::io::{Seek, SeekFrom};
                f.seek(SeekFrom::End(-1))
                    .map_err(|source| SessionError::Io {
                        path: self.path.display().to_string(),
                        source,
                    })?;
                f.read_exact(&mut tail).map_err(|source| SessionError::Io {
                    path: self.path.display().to_string(),
                    source,
                })?;
                tail[0] != b'\n'
            }
            _ => false,
        };
        if needs_leading_newline {
            file.write_all(b"\n").map_err(|source| SessionError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        }
        file.write_all(line.as_bytes())
            .map_err(|source| SessionError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        file.write_all(b"\n").map_err(|source| SessionError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        self.messages.push(msg.clone());
        Ok(())
    }

    /// Copies the first `take` messages (or all of them when `take` is
    /// `None`) into a fresh session file under the same workspace and
    /// returns a new handle bound to it. The parent session file is
    /// left untouched, so the caller can freely diverge on the fork
    /// while still being able to `--resume` the parent later.
    ///
    /// Used by the REPL's `/fork` command to implement conversation
    /// branching: the user keeps the transcript so far, then the next
    /// prompt takes the conversation down a new path.
    pub fn fork(&self, new_id: &str, take: Option<usize>) -> Result<Self, SessionError> {
        // The workspace dir is the parent of the `sessions/` dir which
        // is in turn the parent of the session file — two pops.
        let sessions_dir = self
            .path
            .parent()
            .expect("session path always lives under .metis/sessions");
        let workspace_root = sessions_dir
            .parent()
            .and_then(|p| p.parent())
            .expect(".metis/sessions lives under a workspace root");

        let slice_end = match take {
            Some(n) => n.min(self.messages.len()),
            None => self.messages.len(),
        };
        let slice = &self.messages[..slice_end];

        let mut forked = Self::open(workspace_root, new_id)?;
        // The new file must be empty — forking onto an existing session
        // id would otherwise silently concatenate two unrelated
        // conversations.
        if !forked.messages.is_empty() {
            return Err(SessionError::Io {
                path: forked.path.display().to_string(),
                source: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "fork target session already has messages",
                ),
            });
        }
        // Record parentage in the forked session's metadata.
        forked.meta = SessionMeta {
            parent_id: Some(self.id().to_string()),
            fork_point: Some(slice_end),
            permission_mode: self.meta.permission_mode.clone(),
            last_save_ingested_unix: None,
        };
        forked.save_meta()?;
        for msg in slice {
            forked.append(msg)?;
        }
        Ok(forked)
    }

    /// Convenience wrapper around [`SessionStore::list`] for the
    /// `metis --resume` flow: returns the id of the most recently
    /// modified session, or `None` if the workspace has no sessions
    /// on disk yet. Cheaper to call than [`list`] when only the id is
    /// needed, since it short-circuits after the first entry.
    pub fn latest_id(workspace_root: &Path) -> Result<Option<String>, SessionError> {
        Ok(Self::list(workspace_root)?.into_iter().next().map(|s| s.id))
    }

    /// Enumerates every session stored under a workspace. Returns a
    /// list of [`SessionSummary`] entries sorted by modification time,
    /// newest first — the ordering the `/sessions` REPL command wants.
    pub fn list(workspace_root: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        let dir = workspace_root.join(".metis").join("sessions");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&dir).map_err(|source| SessionError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let mut out: Vec<SessionSummary> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SessionError::Io {
                path: dir.display().to_string(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let meta = entry.metadata().map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let modified = meta.modified().ok();
            // Cheap message count: count non-empty lines without
            // deserializing — a long-running branch might have
            // thousands of messages and `/sessions` should not stall.
            let file = File::open(&path).map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let count = BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter(|l| !l.trim().is_empty())
                .count();
            let meta = Self::load_meta(&path);
            out.push(SessionSummary {
                id,
                path,
                message_count: count,
                modified,
                parent_id: meta.parent_id,
            });
        }
        // Newest first. Sessions with no mtime sink to the bottom.
        out.sort_by(|a, b| match (b.modified, a.modified) {
            (Some(bm), Some(am)) => bm.cmp(&am),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        Ok(out)
    }

    /// Atakan: Trigger B — find the most-recently-modified session in
    /// the workspace that was NOT marked as ingested (or whose JSONL
    /// is newer than the last ingest mark). The returned tuple has the
    /// session id and an age in seconds (from now). The optional
    /// `exclude_id` skips a specific session — pass the id of the
    /// session about to be opened so a fresh launch doesn't flag itself.
    ///
    /// Returns `None` if there are no candidates. Performance: only
    /// inspects mtime + the .meta.json sidecar, never parses JSONL.
    pub fn previous_unsaved_session(
        workspace_root: &Path,
        exclude_id: Option<&str>,
    ) -> Result<Option<UnsavedSessionHint>, SessionError> {
        let summaries = Self::list(workspace_root)?;
        let now = std::time::SystemTime::now();
        for s in summaries {
            if Some(s.id.as_str()) == exclude_id {
                continue;
            }
            // Empty sessions never carry signal worth recovering.
            if s.message_count == 0 {
                continue;
            }
            let mtime = match s.modified {
                Some(t) => t,
                None => continue,
            };
            let meta = Self::load_meta(&s.path);
            let saved_after_last_write = match meta.last_save_ingested_unix {
                Some(saved_secs) => {
                    let mtime_secs = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // 2-second slack: ingest itself touches mtime via the
                    // sidecar write; without slack we'd loop forever.
                    saved_secs + 2 >= mtime_secs
                }
                None => false,
            };
            if saved_after_last_write {
                continue;
            }
            let age_secs = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
            return Ok(Some(UnsavedSessionHint {
                id: s.id,
                path: s.path,
                message_count: s.message_count,
                age_secs,
            }));
        }
        Ok(None)
    }

    /// Tolerant JSONL reader.
    ///
    /// Reads the file as raw bytes, splits on `\n`, and tries to parse
    /// each chunk. Lines that are not valid UTF-8, not valid JSON, or
    /// truncated (e.g. the file ends mid-line because the previous run
    /// was killed mid-write) are silently skipped and counted in
    /// `RecoveryStats::skipped`. The remainder is loaded.
    ///
    /// Why "skip and continue" instead of erroring? A user invoking
    /// `metis --resume` after a crash would otherwise be locked out of
    /// the session entirely by a single bad byte. Losing the tail of
    /// the transcript is bad; losing the entire history is worse.
    fn load(path: &Path) -> Result<(Vec<ChatMessage>, RecoveryStats), SessionError> {
        let mut file = File::open(path).map_err(|source| SessionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;

        let mut out = Vec::new();
        let mut skipped = 0usize;
        for chunk in bytes.split(|b| *b == b'\n') {
            if chunk.is_empty() {
                continue;
            }
            let line = match std::str::from_utf8(chunk) {
                Ok(s) => s,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ChatMessage>(line) {
                Ok(msg) => out.push(msg),
                Err(_) => {
                    skipped += 1;
                }
            }
        }
        let recovered = out.len();
        Ok((out, RecoveryStats { recovered, skipped }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("metis-session-{}-{}", std::process::id(), n,));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn new_session_is_empty_and_creates_file() {
        let dir = tempdir();
        let store = SessionStore::open(&dir, "s1").unwrap();
        assert!(store.messages().is_empty());
        assert!(store.path().exists());
    }

    #[test]
    fn append_persists_and_mirrors() {
        let dir = tempdir();
        let mut store = SessionStore::open(&dir, "s2").unwrap();
        store.append(&ChatMessage::system("hi")).unwrap();
        store.append(&ChatMessage::user("hello")).unwrap();
        assert_eq!(store.messages().len(), 2);

        // Reopen and verify the on-disk state round-trips.
        let reopened = SessionStore::open(&dir, "s2").unwrap();
        assert_eq!(reopened.messages().len(), 2);
        assert_eq!(reopened.messages()[0].content.as_deref(), Some("hi"));
        assert_eq!(reopened.messages()[1].content.as_deref(), Some("hello"));
    }

    #[test]
    fn fork_copies_prefix_and_leaves_parent_untouched() {
        let dir = tempdir();
        let mut parent = SessionStore::open(&dir, "parent").unwrap();
        parent.append(&ChatMessage::system("sys")).unwrap();
        parent.append(&ChatMessage::user("first")).unwrap();
        parent
            .append(&ChatMessage::assistant_text("reply"))
            .unwrap();
        parent.append(&ChatMessage::user("second")).unwrap();

        // Fork with take=Some(2) — keep system + first user, drop the
        // assistant reply and the second user turn.
        let mut child = parent.fork("child", Some(2)).unwrap();
        assert_eq!(child.messages().len(), 2);
        assert_eq!(child.messages()[0].content.as_deref(), Some("sys"));
        assert_eq!(child.messages()[1].content.as_deref(), Some("first"));

        // Append on the child must not touch the parent's file.
        child.append(&ChatMessage::user("alternate path")).unwrap();

        // Reopen parent and verify it still has its four original
        // messages and nothing from the child branch.
        let reopened = SessionStore::open(&dir, "parent").unwrap();
        assert_eq!(reopened.messages().len(), 4);
        assert_eq!(
            reopened.messages()[3].content.as_deref(),
            Some("second"),
            "parent's tail was clobbered by the fork"
        );
    }

    #[test]
    fn fork_with_none_copies_entire_transcript() {
        let dir = tempdir();
        let mut parent = SessionStore::open(&dir, "p").unwrap();
        parent.append(&ChatMessage::system("s")).unwrap();
        parent.append(&ChatMessage::user("u")).unwrap();
        let child = parent.fork("c", None).unwrap();
        assert_eq!(child.messages().len(), 2);
    }

    #[test]
    fn fork_records_parent_id_and_fork_point() {
        let dir = tempdir();
        let mut parent = SessionStore::open(&dir, "root").unwrap();
        parent.append(&ChatMessage::system("sys")).unwrap();
        parent.append(&ChatMessage::user("hi")).unwrap();
        parent
            .append(&ChatMessage::assistant_text("hello"))
            .unwrap();

        let child = parent.fork("branch1", Some(2)).unwrap();
        assert_eq!(child.meta().parent_id.as_deref(), Some("root"));
        assert_eq!(child.meta().fork_point, Some(2));

        // Meta survives reopen
        let reopened = SessionStore::open(&dir, "branch1").unwrap();
        assert_eq!(reopened.meta().parent_id.as_deref(), Some("root"));
        assert_eq!(reopened.meta().fork_point, Some(2));
    }

    #[test]
    fn list_includes_parent_id() {
        let dir = tempdir();
        let mut parent = SessionStore::open(&dir, "main").unwrap();
        parent.append(&ChatMessage::user("x")).unwrap();
        let _child = parent.fork("side", None).unwrap();

        let list = SessionStore::list(&dir).unwrap();
        let side = list.iter().find(|s| s.id == "side").unwrap();
        assert_eq!(side.parent_id.as_deref(), Some("main"));

        let main = list.iter().find(|s| s.id == "main").unwrap();
        assert!(main.parent_id.is_none());
    }

    #[test]
    fn session_id_derived_from_filename() {
        let dir = tempdir();
        let store = SessionStore::open(&dir, "my-session").unwrap();
        assert_eq!(store.id(), "my-session");
    }

    #[test]
    fn fork_refuses_to_overwrite_existing_session() {
        let dir = tempdir();
        let mut parent = SessionStore::open(&dir, "p2").unwrap();
        parent.append(&ChatMessage::user("hi")).unwrap();
        // Pre-create the target so the fork hits a non-empty file.
        let mut target = SessionStore::open(&dir, "taken").unwrap();
        target.append(&ChatMessage::user("already here")).unwrap();

        let err = parent.fork("taken", None).err().expect("should fail");
        assert!(
            format!("{err}").contains("already has messages"),
            "got: {err}"
        );
    }

    #[test]
    fn list_returns_known_sessions_newest_first() {
        let dir = tempdir();
        let mut a = SessionStore::open(&dir, "alpha").unwrap();
        a.append(&ChatMessage::user("one")).unwrap();
        // A tiny sleep so mtimes differ on filesystems with 1-second
        // granularity; otherwise the ordering test is flaky on ext4.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut b = SessionStore::open(&dir, "beta").unwrap();
        b.append(&ChatMessage::user("x")).unwrap();
        b.append(&ChatMessage::assistant_text("y")).unwrap();

        let list = SessionStore::list(&dir).unwrap();
        assert_eq!(list.len(), 2);
        // Newest first → beta before alpha.
        assert_eq!(list[0].id, "beta");
        assert_eq!(list[0].message_count, 2);
        assert_eq!(list[1].id, "alpha");
        assert_eq!(list[1].message_count, 1);
    }

    #[test]
    fn list_on_fresh_workspace_returns_empty() {
        let dir = tempdir();
        let list = SessionStore::list(&dir).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn latest_id_returns_none_on_fresh_workspace() {
        let dir = tempdir();
        assert!(SessionStore::latest_id(&dir).unwrap().is_none());
    }

    #[test]
    fn latest_id_returns_newest_session() {
        let dir = tempdir();
        let mut a = SessionStore::open(&dir, "alpha").unwrap();
        a.append(&ChatMessage::user("one")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut b = SessionStore::open(&dir, "beta").unwrap();
        b.append(&ChatMessage::user("two")).unwrap();
        assert_eq!(
            SessionStore::latest_id(&dir).unwrap().as_deref(),
            Some("beta")
        );
    }

    #[test]
    fn new_id_is_unique_and_shortish() {
        let a = SessionStore::new_id();
        let b = SessionStore::new_id();
        assert_ne!(a, b);
        assert!(a.len() < 64, "{a}");
    }

    #[test]
    fn replace_messages_is_atomic_on_disk() {
        let dir = tempdir();
        let mut store = SessionStore::open(&dir, "s_atomic").unwrap();
        store.append(&ChatMessage::user("a")).unwrap();
        store.append(&ChatMessage::user("b")).unwrap();
        store.append(&ChatMessage::user("c")).unwrap();

        let replacement = vec![ChatMessage::user("only")];
        store.replace_messages(replacement).unwrap();

        // No .jsonl.tmp left behind after a successful rewrite.
        let tmp_leftover = store.path.with_extension("jsonl.tmp");
        assert!(
            !tmp_leftover.exists(),
            "tmp file must be renamed, not left around"
        );

        // Re-opening reflects the rewrite.
        let reopened = SessionStore::open(&dir, "s_atomic").unwrap();
        assert_eq!(reopened.messages().len(), 1);
        assert_eq!(reopened.messages()[0].content.as_deref(), Some("only"));
    }

    #[test]
    fn replace_messages_propagates_write_errors() {
        // Rewriting a session whose parent dir has been removed should
        // surface an error instead of silently diverging memory from
        // disk (the old behavior).
        let dir = tempdir();
        let mut store = SessionStore::open(&dir, "s_err").unwrap();
        store.append(&ChatMessage::user("a")).unwrap();
        // Yank the sessions directory out from under the store.
        let sessions_dir = store.path.parent().unwrap().to_path_buf();
        fs::remove_dir_all(&sessions_dir).unwrap();
        let r = store.replace_messages(vec![ChatMessage::user("x")]);
        assert!(r.is_err(), "disk failure must propagate, got {r:?}");
    }

    // -- Trigger B: previous_unsaved_session ----------------------------

    #[test]
    fn previous_unsaved_returns_none_on_empty_workspace() {
        let dir = tempdir();
        assert!(SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn previous_unsaved_skips_empty_sessions() {
        let dir = tempdir();
        // Touch a session but never append → message_count == 0.
        let _ = SessionStore::open(&dir, "empty").unwrap();
        assert!(SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn previous_unsaved_flags_unmarked_session() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "unsaved_one").unwrap();
        s.append(&ChatMessage::user("hello")).unwrap();
        let hint = SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .expect("expected an unsaved hint");
        assert_eq!(hint.id, "unsaved_one");
        assert_eq!(hint.message_count, 1);
    }

    #[test]
    fn previous_unsaved_excludes_self_id() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "current").unwrap();
        s.append(&ChatMessage::user("hi")).unwrap();
        // The currently-active session shouldn't flag itself.
        assert!(SessionStore::previous_unsaved_session(&dir, Some("current"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn mark_ingested_suppresses_subsequent_hint() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "saved_one").unwrap();
        s.append(&ChatMessage::user("hi")).unwrap();
        // Pre-mark: it shows up.
        assert!(SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .is_some());
        s.mark_ingested_now().unwrap();
        // Post-mark: it doesn't.
        assert!(SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn mark_ingested_re_flags_after_new_append() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "rolling").unwrap();
        s.append(&ChatMessage::user("first")).unwrap();
        s.mark_ingested_now().unwrap();
        // Sleep long enough that the new mtime is clearly past the
        // 2-second slack in `previous_unsaved_session`.
        std::thread::sleep(std::time::Duration::from_secs(3));
        s.append(&ChatMessage::user("second")).unwrap();
        let hint = SessionStore::previous_unsaved_session(&dir, None)
            .unwrap()
            .expect("new content after ingest should re-flag");
        assert_eq!(hint.id, "rolling");
    }

    #[test]
    fn last_user_assistant_pair_picks_most_recent() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "pair").unwrap();
        s.append(&ChatMessage::user("first user")).unwrap();
        s.append(&ChatMessage::assistant_text("first asst")).unwrap();
        s.append(&ChatMessage::user("second user")).unwrap();
        s.append(&ChatMessage::assistant_text("second asst")).unwrap();
        let (u, a) = s.last_user_assistant_pair();
        assert_eq!(u, "second user");
        assert_eq!(a, "second asst");
    }

    #[test]
    fn last_user_assistant_pair_handles_missing_side() {
        let dir = tempdir();
        let mut s = SessionStore::open(&dir, "user_only").unwrap();
        s.append(&ChatMessage::user("alone")).unwrap();
        let (u, a) = s.last_user_assistant_pair();
        assert_eq!(u, "alone");
        assert_eq!(a, "");
    }
}
