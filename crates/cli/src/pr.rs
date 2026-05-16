//! `aegis pr [title]` — commit-push-open shortcut.
//!
//! This is the "quick ship" command for long autonomous sessions. It does
//! not replace a carefully-crafted PR workflow; it exists so that after
//! aegis has driven a refactor and the user is ready to push, one command
//! takes them from local branch to open PR.
//!
//! Flow:
//!
//! 1. If `--commit` is set and the worktree is dirty, stage everything
//!    and create a single auto-commit (message defaults to the
//!    `--title`, or "wip" when no title is given). Without `--commit`
//!    a dirty worktree still bails — `git status -s` is surfaced so
//!    the next step is obvious.
//! 2. Push the current branch to `origin` (upstream-tracking if missing).
//! 3. `gh pr create --fill [--title <title>]` and print the URL.
//!
//! Scope is deliberately narrow — no interactive title prompts, no
//! draft-PR flag, no reviewers, no templates. Those belong to a full
//! `prp-pr` style skill; this is the one-command shortcut.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Entry point invoked from `main::run` when the user types `metis pr
/// [title]`. `title` is an optional free-form PR title; if omitted,
/// `gh pr create --fill` picks up the latest commit message. When
/// `auto_commit` is true a dirty worktree is stage+committed before
/// the push; otherwise the dirty-worktree guard fires unchanged.
pub fn run(title: Option<&str>, auto_commit: bool, workspace: &Path) -> Result<()> {
    if auto_commit {
        maybe_auto_commit(workspace, title)?;
    }
    ensure_clean_worktree(workspace)?;
    let branch = current_branch(workspace)?;
    // Detached HEAD reports as the literal "HEAD" from `rev-parse
    // --abbrev-ref`. Pushing it would produce "git push -u origin HEAD"
    // which has no upstream to track and almost always means the user
    // is at a checked-out commit by accident. Bail with a clear
    // remediation rather than delegating to git's generic error.
    if branch == "HEAD" || branch.is_empty() {
        bail!(
            "HEAD is detached — check out a branch before running `metis pr` (git switch -c my-feature)"
        );
    }
    if branch == "main" || branch == "master" {
        bail!(
            "refusing to push/open a PR from `{branch}` — create a feature branch first (git switch -c my-feature)"
        );
    }
    push_branch(workspace, &branch)?;
    open_pr(workspace, title)
}

fn run_git(workspace: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .current_dir(workspace)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))
}

fn ensure_clean_worktree(workspace: &Path) -> Result<()> {
    let out = run_git(workspace, &["status", "--porcelain"])?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if !out.stdout.is_empty() {
        eprintln!("\x1b[1;33m[metis pr] worktree has uncommitted changes:\x1b[0m");
        eprint!("{}", String::from_utf8_lossy(&out.stdout));
        bail!(
            "commit or stash your changes before running `metis pr` — \
             pass `--commit` to auto-stage + commit them in one go \
             (default off so unintended files don't sneak into history)"
        );
    }
    Ok(())
}

/// Stage everything tracked + untracked and create one auto-commit.
/// Skips silently when the worktree is already clean so the caller can
/// invoke unconditionally without a pre-check round-trip. Commit
/// message: the supplied `title` if any, else `"wip"`. Uses `git add
/// -A` rather than per-file targeting on purpose — this is the
/// "throw it all in" path; the safety gate is the opt-in `--commit`
/// flag itself, not granular staging.
fn maybe_auto_commit(workspace: &Path, title: Option<&str>) -> Result<()> {
    let status = run_git(workspace, &["status", "--porcelain"])?;
    if !status.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
    if status.stdout.is_empty() {
        return Ok(());
    }
    eprintln!("\x1b[2m[metis pr --commit] → git add -A && git commit\x1b[0m");
    let add = run_git(workspace, &["add", "-A"])?;
    if !add.status.success() {
        bail!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let msg = title.unwrap_or("wip");
    let commit = run_git(workspace, &["commit", "-m", msg])?;
    if !commit.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }
    Ok(())
}

fn current_branch(workspace: &Path) -> Result<String> {
    let out = run_git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !out.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn push_branch(workspace: &Path, branch: &str) -> Result<()> {
    eprintln!("\x1b[2m[metis pr] → git push -u origin {branch}\x1b[0m");
    let status = Command::new("git")
        .current_dir(workspace)
        .args(["push", "-u", "origin", branch])
        .status()
        .context("failed to spawn git push")?;
    if !status.success() {
        bail!("git push failed (exit {})", status.code().unwrap_or(-1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Initialize a fresh git repo in `dir` with one committed file so
    /// `maybe_auto_commit` has a HEAD to commit on top of. Configures
    /// user.name / user.email locally because CI runners often lack
    /// global git identity.
    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]).expect("git init");
        run_git(dir, &["config", "user.email", "test@example.com"]).expect("config email");
        run_git(dir, &["config", "user.name", "test"]).expect("config name");
        fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        run_git(dir, &["add", "seed.txt"]).expect("git add seed");
        run_git(dir, &["commit", "-q", "-m", "seed"]).expect("git commit seed");
    }

    fn head_subject(dir: &Path) -> String {
        let out = run_git(dir, &["log", "-1", "--format=%s"]).expect("git log");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn auto_commit_clean_worktree_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let head_before = head_subject(tmp.path());
        maybe_auto_commit(tmp.path(), Some("anything"))
            .expect("noop should not error on clean worktree");
        assert_eq!(head_subject(tmp.path()), head_before);
    }

    #[test]
    fn auto_commit_dirty_worktree_uses_title_as_message() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("new.txt"), "wip\n").unwrap();
        maybe_auto_commit(tmp.path(), Some("ship: my refactor"))
            .expect("auto-commit should land on dirty worktree");
        assert_eq!(head_subject(tmp.path()), "ship: my refactor");
        // worktree should be clean after the commit so the caller's
        // ensure_clean_worktree gate now passes.
        let status = run_git(tmp.path(), &["status", "--porcelain"]).unwrap();
        assert!(status.stdout.is_empty(), "expected clean worktree post-commit");
    }

    #[test]
    fn auto_commit_dirty_worktree_falls_back_to_wip_when_no_title() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("new.txt"), "wip\n").unwrap();
        maybe_auto_commit(tmp.path(), None).expect("auto-commit");
        assert_eq!(head_subject(tmp.path()), "wip");
    }

    #[test]
    fn auto_commit_picks_up_untracked_files() {
        // Regression: an earlier draft used `git add -u` which only
        // stages tracked changes — a fresh untracked file would slip
        // past --commit and immediately re-trip ensure_clean_worktree.
        // `git add -A` includes untracked, which is what the docstring
        // ("throw it all in") promises.
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("brand_new.rs"), "fn main() {}\n").unwrap();
        maybe_auto_commit(tmp.path(), Some("untracked")).unwrap();
        let ls = run_git(tmp.path(), &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        let names = String::from_utf8_lossy(&ls.stdout);
        assert!(
            names.lines().any(|l| l == "brand_new.rs"),
            "untracked file should be in HEAD tree, got: {names}"
        );
    }
}

fn open_pr(workspace: &Path, title: Option<&str>) -> Result<()> {
    let mut args: Vec<&str> = vec!["pr", "create", "--fill"];
    if let Some(t) = title {
        args.push("--title");
        args.push(t);
    }
    eprintln!("\x1b[2m[metis pr] → gh {}\x1b[0m", args.join(" "));
    let status = Command::new("gh")
        .current_dir(workspace)
        .args(&args)
        .status()
        .context("failed to spawn gh — is the GitHub CLI installed?")?;
    if !status.success() {
        bail!(
            "gh pr create failed (exit {}) — check the gh output above",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}
