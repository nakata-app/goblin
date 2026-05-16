//! Prompt self-modification — let the agent modify its own system prompt
//! with feedback-driven edits, git-based rollback, and auto-rebuild.
//!
//! Each modification is git-committed with a descriptive message so the
//! user can rollback via `git log` / `git revert`.

use std::path::PathBuf;
use std::process::Command;

/// Relative path from aegis workspace root to system_prompt.md.
const SYSTEM_PROMPT_RELATIVE: &str = "crates/cli/src/system_prompt.md";

/// Errors that can occur during prompt self-modification.
#[derive(Debug, thiserror::Error)]
pub enum PromptModError {
    #[error("aegis workspace root not configured (set ToolContext.aegis_root)")]
    NoAegisRoot,
    #[error("system_prompt.md not found at {0}")]
    PromptFileNotFound(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("edit failed: old_string not found in system_prompt.md")]
    OldStringNotFound,
    #[error("old_string matched multiple times — add more context to make it unique")]
    MultipleMatches,
    #[error("build failed: {0}")]
    BuildFailed(String),
    #[error("git error: {0}")]
    GitError(String),
}

/// Result of a prompt modification operation.
#[derive(Debug)]
pub struct PromptModResult {
    pub path: PathBuf,
    pub replaced: bool,
    pub commit_hash: Option<String>,
    pub build_status: String,
}

/// Apply an edit to system_prompt.md: find `old_string`, replace with `new_string`.
/// Then `cargo build --release` and git commit with `reason`.
pub fn modify_prompt(
    aegis_root: &std::path::Path,
    old_string: &str,
    new_string: &str,
    reason: &str,
) -> Result<PromptModResult, PromptModError> {
    let prompt_path = aegis_root.join(SYSTEM_PROMPT_RELATIVE);

    if !prompt_path.exists() {
        return Err(PromptModError::PromptFileNotFound(prompt_path));
    }

    let content = std::fs::read_to_string(&prompt_path)?;

    let matches: Vec<_> = content.match_indices(old_string).collect();

    if matches.is_empty() {
        return Err(PromptModError::OldStringNotFound);
    }
    if matches.len() > 1 {
        return Err(PromptModError::MultipleMatches);
    }

    let (pos, _) = matches[0];
    let mut new_content = String::with_capacity(content.len() + new_string.len());
    new_content.push_str(&content[..pos]);
    new_content.push_str(new_string);
    new_content.push_str(&content[pos + old_string.len()..]);

    std::fs::write(&prompt_path, &new_content)?;

    // Stage and commit the change
    let commit_hash = git_commit(aegis_root, &prompt_path, reason);

    // Rebuild
    let build_status = cargo_rebuild(aegis_root);

    Ok(PromptModResult {
        path: prompt_path,
        replaced: true,
        commit_hash,
        build_status,
    })
}

/// Revert the last modification to system_prompt.md.
pub fn rollback_prompt(aegis_root: &std::path::Path) -> Result<PromptModResult, PromptModError> {
    let prompt_path = aegis_root.join(SYSTEM_PROMPT_RELATIVE);

    if !prompt_path.exists() {
        return Err(PromptModError::PromptFileNotFound(prompt_path));
    }

    // git checkout the last committed version of this file
    let output = Command::new("git")
        .args(["checkout", "HEAD~1", "--"])
        .arg(SYSTEM_PROMPT_RELATIVE)
        .current_dir(aegis_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PromptModError::GitError(format!(
            "rollback failed: {}",
            stderr
        )));
    }

    // Stage the reverted file
    let _ = Command::new("git")
        .args(["add"])
        .arg(SYSTEM_PROMPT_RELATIVE)
        .current_dir(aegis_root)
        .output()?;

    // Commit the rollback
    let commit_hash = git_commit(aegis_root, &prompt_path, "rollback: revert last prompt change");

    let build_status = cargo_rebuild(aegis_root);

    Ok(PromptModResult {
        path: prompt_path,
        replaced: true,
        commit_hash,
        build_status,
    })
}

/// Show recent changes to system_prompt.md via git log.
pub fn show_changes(aegis_root: &std::path::Path) -> Result<String, PromptModError> {
    let output = Command::new("git")
        .args([
            "log",
            "--oneline",
            "-10",
            "--",
            SYSTEM_PROMPT_RELATIVE,
        ])
        .current_dir(aegis_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PromptModError::GitError(format!(
            "git log failed: {}",
            stderr
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `cargo build --release` in the aegis root. Returns status string.
fn cargo_rebuild(aegis_root: &std::path::Path) -> String {
    match Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(aegis_root)
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                "build succeeded".to_string()
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Extract only the error lines, not full output
                let errors: Vec<&str> = stderr
                    .lines()
                    .filter(|l| l.contains("error") || l.contains("Error"))
                    .take(3)
                    .collect();
                format!("build failed: {}", errors.join("\n"))
            }
        }
        Err(e) => format!("build command failed: {}", e),
    }
}

/// Stage and commit with a message. Returns commit hash on success.
fn git_commit(repo_root: &std::path::Path, file_path: &std::path::Path, reason: &str) -> Option<String> {
    // Stage
    let _ = Command::new("git")
        .args(["add"])
        .arg(file_path.to_string_lossy().as_ref())
        .current_dir(repo_root)
        .output();

    // Commit
    let output = Command::new("git")
        .args(["commit", "-m", &format!("prompt(self-modify): {}", reason)])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // Get the commit hash
    let hash_output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    Some(String::from_utf8_lossy(&hash_output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modify_prompt_missing_root() {
        let result = modify_prompt(
            std::path::Path::new("/nonexistent"),
            "old",
            "new",
            "test reason",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_show_changes_missing_root() {
        let result = show_changes(std::path::Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git command failed to launch")
    }

    fn setup_fake_aegis_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let prompt_path = root.join(SYSTEM_PROMPT_RELATIVE);
        std::fs::create_dir_all(prompt_path.parent().unwrap()).unwrap();
        std::fs::write(&prompt_path, "hello WORLD\nline two\n").unwrap();

        assert!(run_git(&root, &["init", "-q", "-b", "main"]).status.success());
        run_git(&root, &["config", "user.email", "test@example.com"]);
        run_git(&root, &["config", "user.name", "tester"]);
        run_git(&root, &["config", "commit.gpgsign", "false"]);
        run_git(&root, &["add", "-A"]);
        assert!(run_git(&root, &["commit", "-q", "-m", "init"]).status.success());
        (tmp, root)
    }

    #[test]
    fn test_modify_and_rollback_end_to_end() {
        let (_tmp, root) = setup_fake_aegis_repo();
        let prompt_path = root.join(SYSTEM_PROMPT_RELATIVE);

        let result = modify_prompt(&root, "WORLD", "AEGIS", "swap world for aegis")
            .expect("modify_prompt succeeded");
        assert!(result.replaced);
        assert!(result.commit_hash.is_some(), "commit hash returned");

        let after = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(after.contains("hello AEGIS"), "file content updated, got: {after}");
        assert!(!after.contains("hello WORLD"), "old string removed");

        let log = run_git(&root, &["log", "--oneline"]);
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_str.contains("prompt(self-modify): swap world for aegis"),
            "commit message recorded, got: {log_str}"
        );

        let rb = rollback_prompt(&root).expect("rollback_prompt succeeded");
        assert!(rb.replaced);
        let restored = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(restored.contains("hello WORLD"), "rolled back, got: {restored}");
        assert!(!restored.contains("AEGIS"), "new string removed after rollback");
    }

    #[test]
    fn test_modify_old_string_not_found() {
        let (_tmp, root) = setup_fake_aegis_repo();
        let err = modify_prompt(&root, "DOES_NOT_EXIST", "x", "reason").unwrap_err();
        assert!(matches!(err, PromptModError::OldStringNotFound));
    }

    #[test]
    fn test_modify_old_string_multiple_matches() {
        let (_tmp, root) = setup_fake_aegis_repo();
        let prompt_path = root.join(SYSTEM_PROMPT_RELATIVE);
        std::fs::write(&prompt_path, "dup\ndup\n").unwrap();
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-q", "-m", "dup"]);

        let err = modify_prompt(&root, "dup", "single", "reason").unwrap_err();
        assert!(matches!(err, PromptModError::MultipleMatches));
    }

    #[test]
    fn test_show_changes_after_modify() {
        let (_tmp, root) = setup_fake_aegis_repo();
        modify_prompt(&root, "WORLD", "AEGIS", "first").expect("modify ok");
        let log = show_changes(&root).expect("show_changes ok");
        assert!(log.contains("first"), "log mentions reason: {log}");
    }
}
