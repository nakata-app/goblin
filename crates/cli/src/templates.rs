//! Prompt template store.
//!
//! Lives at `<workspace>/.metis/templates/<name>.txt`. Slash commands
//! `/save-template <name>`, `/use <name>`, and `/templates` are the
//! only callers; the on-disk shape is intentionally trivial (one flat
//! file per template) so users can edit templates by hand.
//!
//! Names are constrained to ASCII alnum + `-_` (≤64 chars) to keep
//! traversal and exotic-filename concerns out of scope. Anything else
//! is rejected at the boundary so callers never see a `..`-shaped
//! lookup.

use std::io;
use std::path::{Path, PathBuf};

fn templates_dir(workspace: &Path) -> PathBuf {
    workspace.join(".metis").join("templates")
}

fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn invalid_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "template name must be ASCII letters/digits/-/_ and ≤64 chars",
    )
}

pub fn save(workspace: &Path, name: &str, body: &str) -> io::Result<PathBuf> {
    if !is_safe_name(name) {
        return Err(invalid_name());
    }
    let dir = templates_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.txt"));
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn load(workspace: &Path, name: &str) -> io::Result<String> {
    if !is_safe_name(name) {
        return Err(invalid_name());
    }
    let path = templates_dir(workspace).join(format!("{name}.txt"));
    std::fs::read_to_string(path)
}

pub fn list(workspace: &Path) -> Vec<String> {
    let dir = templates_dir(workspace);
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("txt") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if is_safe_name(stem) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn save_then_load_roundtrip() {
        let ws = tmp();
        save(ws.path(), "bug-report", "describe the bug").unwrap();
        let loaded = load(ws.path(), "bug-report").unwrap();
        assert_eq!(loaded, "describe the bug");
    }

    #[test]
    fn list_returns_sorted_names() {
        let ws = tmp();
        save(ws.path(), "z_one", "x").unwrap();
        save(ws.path(), "a_two", "y").unwrap();
        save(ws.path(), "m_three", "z").unwrap();
        assert_eq!(list(ws.path()), vec!["a_two", "m_three", "z_one"]);
    }

    #[test]
    fn invalid_names_are_rejected() {
        let ws = tmp();
        for bad in [
            "", "../escape", "with space", "slash/in", "dot.in", "tab\there",
        ] {
            assert!(save(ws.path(), bad, "x").is_err(), "save accepted: {bad:?}");
            assert!(load(ws.path(), bad).is_err(), "load accepted: {bad:?}");
        }
    }

    #[test]
    fn list_skips_non_txt_and_unsafe_stems() {
        let ws = tmp();
        save(ws.path(), "ok", "x").unwrap();
        let dir = ws.path().join(".metis").join("templates");
        std::fs::write(dir.join("ignored.md"), "x").unwrap();
        std::fs::write(dir.join("dot.in.the.middle.txt"), "x").unwrap();
        assert_eq!(list(ws.path()), vec!["ok"]);
    }

    #[test]
    fn missing_template_load_errors() {
        let ws = tmp();
        let err = load(ws.path(), "nope").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
