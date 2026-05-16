//! Language-aware tool hints for the system prompt.
//!
//! Walks the workspace root for the manifest files that uniquely
//! identify a project's primary language(s) — `Cargo.toml`,
//! `package.json`, `pyproject.toml` / `requirements.txt`, `go.mod` —
//! and emits a short "you are in a $LANG project, use these commands"
//! block the agent loop appends to the system prompt at boot. The
//! intent is to short-circuit the "model spends three turns running
//! `ls` to figure out the language" pattern on a fresh clone.
//!
//! The detection is intentionally cheap (top-level filename checks
//! only, no recursive glob, no parsing) so the boot cost stays under
//! a millisecond. Multiple manifests are honoured — a polyglot repo
//! emits one hint block per detected language.
//!
//! No env reads, no commands run; the hints are pure suggestions the
//! model can execute via `/test` / `/lint` / `/run` or its bash tool.

use std::path::Path;

/// One detected language and its boilerplate hint paragraph. Kept as
/// a struct (rather than a free-form string) so callers — system
/// prompt enrichment, future status-bar badge, doc generators — can
/// inspect the language id and the human-readable text separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangHint {
    /// Short id matching the conventional toolchain name: `rust`,
    /// `node`, `python`, `go`. Used for stable test assertions and
    /// downstream consumers that want to filter without parsing the
    /// rendered prose.
    pub id: &'static str,
    /// Human-readable label for the prompt header.
    pub label: &'static str,
    /// Multi-line hint body — already prefixed with the conventional
    /// `- ` bullets so callers can concatenate without re-templating.
    pub body: &'static str,
}

impl LangHint {
    /// All built-in hint definitions. Order matches the order they
    /// appear in `detect`'s output for a polyglot workspace; keeping
    /// the order stable means deterministic prompts (good for cache
    /// hits + golden tests).
    pub const ALL: &'static [LangHint] = &[
        LangHint {
            id: "rust",
            label: "Rust (Cargo.toml detected)",
            body: "- build:    `cargo build`\n\
                   - test:     `cargo test --workspace`\n\
                   - lint:     `cargo clippy --workspace --all-targets`\n\
                   - fmt:      `cargo fmt --all`\n\
                   - run:      `cargo run -p <crate> -- <args>`\n\
                   - bench:    `cargo bench`\n\
                   Tip: prefer `cargo check` for fast feedback during edits.",
        },
        LangHint {
            id: "node",
            label: "Node / TypeScript (package.json detected)",
            body: "- install:  `npm install` (or `pnpm install` / `yarn install` if lockfile says so)\n\
                   - run:      `npm run <script>` — see `scripts` block in package.json\n\
                   - test:     `npm test` (or the explicit script)\n\
                   - lint:     `npm run lint` (eslint / biome / oxlint, project-specific)\n\
                   - build:    `npm run build`\n\
                   Tip: `npx <bin>` runs a tool without a global install.",
        },
        LangHint {
            id: "python",
            label: "Python (pyproject.toml or requirements.txt detected)",
            body: "- install:  `pip install -r requirements.txt` or `pip install -e .`\n\
                   - test:     `pytest` (or `python -m unittest`)\n\
                   - lint:     `ruff check .` / `flake8` / `pylint`\n\
                   - fmt:      `ruff format .` or `black .`\n\
                   - run:      `python -m <pkg>` or the project's CLI entrypoint\n\
                   Tip: use a venv (`python -m venv .venv && source .venv/bin/activate`) before installs.",
        },
        LangHint {
            id: "go",
            label: "Go (go.mod detected)",
            body: "- build:    `go build ./...`\n\
                   - test:     `go test ./...`\n\
                   - lint:     `go vet ./...` (`golangci-lint run` if configured)\n\
                   - fmt:      `gofmt -w .` or `goimports -w .`\n\
                   - run:      `go run ./cmd/<binary>`\n\
                   Tip: `go test -run TestName ./pkg/...` scopes a single test.",
        },
    ];
}

/// Inspect the workspace root for the conventional manifest of each
/// supported language and return the matching hint definitions in
/// the order they appear in `LangHint::ALL`. A polyglot project (e.g.
/// a Tauri repo with both `Cargo.toml` and `package.json`) returns
/// multiple entries; a workspace with no recognised manifest returns
/// an empty vec.
pub fn detect(workspace: &Path) -> Vec<&'static LangHint> {
    let mut out = Vec::new();
    let candidates = [
        ("rust", &["Cargo.toml"][..]),
        ("node", &["package.json"][..]),
        ("python", &["pyproject.toml", "requirements.txt"][..]),
        ("go", &["go.mod"][..]),
    ];
    for (id, files) in candidates {
        if files.iter().any(|f| workspace.join(f).exists()) {
            if let Some(hint) = LangHint::ALL.iter().find(|h| h.id == id) {
                out.push(hint);
            }
        }
    }
    out
}

/// Build the prompt-ready hints block. Returns `None` (rather than an
/// empty string) when no manifest matched, so the caller can append
/// without `format!`-padding empty whitespace into the system prompt.
pub fn render(hints: &[&LangHint]) -> Option<String> {
    if hints.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("# Project toolchain\n\n");
    out.push_str(
        "Detected from manifests at the workspace root. Prefer these \
         commands when running tests / lints / builds via the bash \
         tool, instead of re-deriving them from scratch.\n",
    );
    for h in hints {
        out.push_str("\n## ");
        out.push_str(h.label);
        out.push('\n');
        out.push_str(h.body);
        out.push('\n');
    }
    Some(out)
}

/// One-shot helper: detect + render. Returns `None` when nothing
/// matched, so callers can use `.unwrap_or_default()` and concat
/// without branching.
pub fn block_for(workspace: &Path) -> Option<String> {
    render(&detect(workspace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_blank(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, "").unwrap();
    }

    #[test]
    fn detect_empty_workspace_returns_no_hints() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_empty());
        assert!(block_for(tmp.path()).is_none());
    }

    #[test]
    fn detect_rust_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        write_blank(&tmp.path().join("Cargo.toml"));
        let hints = detect(tmp.path());
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].id, "rust");
        let block = block_for(tmp.path()).unwrap();
        assert!(block.contains("Rust (Cargo.toml detected)"));
        assert!(block.contains("cargo test"));
    }

    #[test]
    fn detect_node_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        write_blank(&tmp.path().join("package.json"));
        let hints = detect(tmp.path());
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].id, "node");
        assert!(block_for(tmp.path()).unwrap().contains("npm install"));
    }

    #[test]
    fn detect_python_either_manifest_works() {
        // requirements.txt alone is enough; users without pyproject
        // pip-install from a flat list and shouldn't be ignored.
        let tmp = tempfile::tempdir().unwrap();
        write_blank(&tmp.path().join("requirements.txt"));
        assert_eq!(detect(tmp.path())[0].id, "python");

        let tmp2 = tempfile::tempdir().unwrap();
        write_blank(&tmp2.path().join("pyproject.toml"));
        assert_eq!(detect(tmp2.path())[0].id, "python");
    }

    #[test]
    fn detect_go_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        write_blank(&tmp.path().join("go.mod"));
        let hints = detect(tmp.path());
        assert_eq!(hints[0].id, "go");
        assert!(block_for(tmp.path()).unwrap().contains("go test ./..."));
    }

    #[test]
    fn detect_polyglot_workspace_returns_all_matching_hints_in_canonical_order() {
        // A Tauri-shaped repo (Rust backend + Node frontend) is the
        // canonical polyglot case the prompt should advertise both
        // toolchains for. Ordering must match LangHint::ALL so the
        // generated prompt is byte-stable across boots — important
        // for prompt-cache hits.
        let tmp = tempfile::tempdir().unwrap();
        write_blank(&tmp.path().join("Cargo.toml"));
        write_blank(&tmp.path().join("package.json"));
        let hints = detect(tmp.path());
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].id, "rust");
        assert_eq!(hints[1].id, "node");
        let block = block_for(tmp.path()).unwrap();
        // Both labels appear in the rendered block, Rust first.
        let rust_pos = block.find("Rust").unwrap();
        let node_pos = block.find("Node").unwrap();
        assert!(rust_pos < node_pos);
    }

    #[test]
    fn detect_ignores_unrelated_files() {
        // Random TXT file at the root should not trip the detector.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("README.md"), "hi").unwrap();
        fs::write(tmp.path().join("notes.txt"), "meh").unwrap();
        assert!(detect(tmp.path()).is_empty());
    }
}
