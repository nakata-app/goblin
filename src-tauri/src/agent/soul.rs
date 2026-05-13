//! Persona loader. Reads `~/.goblin/SOUL.md` once per send_message and
//! returns it as plain text. Missing file is a normal cold-start state
//! (we return None). I/O errors are logged but never propagated — a
//! broken persona file should not block the agent from answering.

use std::path::PathBuf;

/// Resolve the on-disk path of the persona file. Honours $GOBLIN_HOME
/// for tests; otherwise defaults to `~/.goblin/SOUL.md`.
pub fn soul_path() -> PathBuf {
    if let Ok(override_dir) = std::env::var("GOBLIN_HOME") {
        return PathBuf::from(override_dir).join("SOUL.md");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".goblin").join("SOUL.md");
    }
    PathBuf::from(".goblin/SOUL.md")
}

/// Load the persona file if present. Returns None on missing-file (cold
/// start) and Some("") on empty file — the prompt builder treats both
/// as "no SOUL injection".
pub fn load_soul() -> Option<String> {
    let path = soul_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("[soul] failed to read {:?}: {}", path, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // GOBLIN_HOME is process-global, so we serialise the tests that
    // mutate it; otherwise running cargo test (parallel by default)
    // makes them race and randomly fail.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn fresh_tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("goblin-soul-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_returns_none() {
        let _g = ENV_GUARD.lock().unwrap();
        let dir = fresh_tmp_dir("missing");
        std::env::set_var("GOBLIN_HOME", &dir);
        assert!(load_soul().is_none());
        std::env::remove_var("GOBLIN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn present_file_returns_contents() {
        let _g = ENV_GUARD.lock().unwrap();
        let dir = fresh_tmp_dir("present");
        std::fs::write(dir.join("SOUL.md"), "I speak Turkish.\nNo em-dash.").unwrap();
        std::env::set_var("GOBLIN_HOME", &dir);
        let body = load_soul().expect("should load");
        assert!(body.contains("Turkish"));
        assert!(body.contains("em-dash"));
        std::env::remove_var("GOBLIN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_returns_some_empty() {
        let _g = ENV_GUARD.lock().unwrap();
        let dir = fresh_tmp_dir("empty");
        std::fs::write(dir.join("SOUL.md"), "").unwrap();
        std::env::set_var("GOBLIN_HOME", &dir);
        let body = load_soul().expect("should load");
        assert!(body.is_empty());
        std::env::remove_var("GOBLIN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
