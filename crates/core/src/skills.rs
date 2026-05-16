//! Skills — user-invocable prompt shortcuts.
//!
//! A skill is a markdown file with YAML frontmatter and a prompt body.
//! When invoked (e.g. `/commit -m "fix bug"`), the prompt body is
//! expanded with any arguments and fed to the agent as a user message.
//!
//! ## File layout
//!
//! ```text
//! ~/.aegis/skills/commit.md      (user-level)
//! .aegis/skills/commit.md        (workspace-level, overrides user)
//! ```
//!
//! ## Frontmatter
//!
//! ```yaml
//! ---
//! name: commit
//! description: Create a git commit with a message
//! user_invocable: true
//! ---
//! ```
//!
//! ## Prompt body
//!
//! Everything after the closing `---` is the prompt template.
//! `$ARGS` is replaced with the arguments passed to the skill.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Per-skill usage statistics, persisted in `~/.metis/skill_stats.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillStats {
    pub name: String,
    pub use_count: u32,
    pub success_count: u32,
    pub failure_count: u32,
    /// ISO-8601 timestamp of last use.
    pub last_used: Option<String>,
}

impl SkillStats {
    pub fn success_rate(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            return 1.0;
        }
        self.success_count as f64 / total as f64
    }

    /// True when a skill has enough data and is consistently underperforming.
    pub fn needs_improvement(&self) -> bool {
        self.use_count >= 3 && self.success_rate() < 0.5
    }
}

/// Path to the skill stats journal.
pub fn stats_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".metis").join("skill_stats.jsonl"))
}

/// Load stats for a single skill (returns default if not found).
pub fn load_skill_stats(name: &str) -> SkillStats {
    let path = match stats_path() {
        Some(p) => p,
        None => {
            return SkillStats {
                name: name.to_string(),
                ..Default::default()
            }
        }
    };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            return SkillStats {
                name: name.to_string(),
                ..Default::default()
            }
        }
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(s) = serde_json::from_str::<SkillStats>(&line) {
            if s.name == name {
                return s;
            }
        }
    }
    SkillStats {
        name: name.to_string(),
        ..Default::default()
    }
}

/// Persist updated stats (rewrites the journal with the new entry).
pub fn save_skill_stats(updated: &SkillStats) {
    let path = match stats_path() {
        Some(p) => p,
        None => return,
    };
    // Read all existing entries except the one we're updating
    let mut entries: Vec<SkillStats> = Vec::new();
    if let Ok(file) = std::fs::File::open(&path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(s) = serde_json::from_str::<SkillStats>(&line) {
                if s.name != updated.name {
                    entries.push(s);
                }
            }
        }
    }
    entries.push(updated.clone());
    if let Ok(mut file) = std::fs::File::create(&path) {
        for e in &entries {
            if let Ok(line) = serde_json::to_string(e) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

/// Record a success or failure outcome for a skill.
pub fn record_skill_outcome(name: &str, success: bool) {
    let mut stats = load_skill_stats(name);
    stats.use_count += 1;
    if success {
        stats.success_count += 1;
    } else {
        stats.failure_count += 1;
    }
    stats.last_used = Some(crate::telemetry::now_iso8601());
    save_skill_stats(&stats);
}

/// Parsed skill definition.
#[derive(Debug, Clone, Default)]
pub struct Skill {
    /// Unique name (filename without extension).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether users can invoke this skill via `/name`.
    pub user_invocable: bool,
    /// The prompt template body (everything after frontmatter).
    pub prompt: String,
    /// Where this skill was loaded from.
    pub source: PathBuf,
    /// Author name (optional).
    pub author: Option<String>,
    /// Semantic version (optional).
    pub version: Option<String>,
    /// Comma-separated tags for search/filtering.
    pub tags: Vec<String>,
}

/// YAML frontmatter fields.
#[derive(Debug, Clone)]
struct Frontmatter {
    name: String,
    description: String,
    user_invocable: bool,
    author: Option<String>,
    version: Option<String>,
    tags: Vec<String>,
}

/// Parse a skill file's content into frontmatter + body.
fn parse_skill(content: &str, source: &Path) -> Option<Skill> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    // Find the closing ---
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let yaml_block = &rest[..end].trim();
    let body = rest[end + 4..].trim();

    let fm = parse_frontmatter(yaml_block, source)?;

    Some(Skill {
        name: fm.name,
        description: fm.description,
        user_invocable: fm.user_invocable,
        prompt: body.to_string(),
        source: source.to_path_buf(),
        author: fm.author,
        version: fm.version,
        tags: fm.tags,
    })
}

/// Minimal YAML-like parser for frontmatter. We avoid pulling in a
/// full YAML crate for 3 fields — just line-based key: value parsing.
fn parse_frontmatter(block: &str, source: &Path) -> Option<Frontmatter> {
    let mut map: HashMap<String, String> = HashMap::new();
    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            map.insert(
                key.trim().to_lowercase(),
                val.trim().trim_matches('"').trim_matches('\'').to_string(),
            );
        }
    }

    let name = map.get("name").cloned().unwrap_or_else(|| {
        source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    let description = map.get("description").cloned().unwrap_or_default();

    let user_invocable = map
        .get("user_invocable")
        .map(|v| v == "true" || v == "yes" || v == "1")
        .unwrap_or(true); // default: invocable

    let author = map.get("author").cloned();
    let version = map.get("version").cloned();
    let tags: Vec<String> = map
        .get("tags")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Some(Frontmatter {
        name,
        description,
        user_invocable,
        author,
        version,
        tags,
    })
}

/// Expand the skill prompt template with arguments and context variables.
///
/// Supported variables:
/// - `$ARGS` — user-supplied arguments
/// - `$WORKSPACE` — workspace root path
/// - `$GIT_BRANCH` — current git branch name
/// - `$MODEL` — current model name
pub fn expand_prompt(skill: &Skill, args: &str) -> String {
    let mut prompt = skill.prompt.clone();
    prompt = prompt.replace("$ARGS", args);
    prompt
}

/// Extended expand that injects workspace context variables.
pub fn expand_prompt_full(skill: &Skill, args: &str, workspace: &Path, model: &str) -> String {
    let mut prompt = skill.prompt.clone();
    prompt = prompt.replace("$ARGS", args);
    prompt = prompt.replace("$WORKSPACE", &workspace.display().to_string());
    prompt = prompt.replace("$MODEL", model);

    // Resolve git branch lazily
    if prompt.contains("$GIT_BRANCH") {
        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(workspace)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());
        prompt = prompt.replace("$GIT_BRANCH", &branch);
    }

    prompt
}

/// Registry of discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Load skills from the standard locations:
    /// 1. `~/.metis/skills/` (user-level)
    /// 2. `.metis/skills/` (workspace-level, overrides user)
    pub fn discover(workspace: &Path) -> Self {
        let mut reg = Self::new();

        // User-level skills
        if let Some(home) = dirs::home_dir() {
            let user_dir = home.join(".metis").join("skills");
            reg.load_dir(&user_dir);
        }

        // Workspace-level skills (override user-level by name)
        let ws_dir = workspace.join(".metis").join("skills");
        reg.load_dir(&ws_dir);

        reg
    }

    /// Public accessor for reloading a directory (e.g. after saving a new skill).
    pub fn load_dir_pub(&mut self, dir: &Path) {
        self.load_dir(dir);
    }

    fn load_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(skill) = parse_skill(&content, &path) {
                        // Override any existing skill with the same name
                        self.skills.retain(|s| s.name != skill.name);
                        self.skills.push(skill);
                    }
                }
            }
        }
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// All user-invocable skills (for `/help` listing).
    pub fn user_invocable(&self) -> Vec<&Skill> {
        self.skills.iter().filter(|s| s.user_invocable).collect()
    }

    /// Register a skill programmatically (for built-in skills).
    pub fn register(&mut self, skill: Skill) {
        self.skills.retain(|s| s.name != skill.name);
        self.skills.push(skill);
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Install a skill from a git repository URL. Clones the repo
    /// (or fetches a single file via raw URL) into the user-level
    /// skills directory `~/.metis/skills/`.
    ///
    /// Supported sources:
    /// - Git repo URL: `https://github.com/user/repo` — clones and
    ///   copies all `.md` files from the repo root and `skills/` subdir.
    /// - Raw file URL: `https://.../*.md` — downloads single skill file.
    /// - Local path: `/path/to/skill.md` — copies the file.
    pub fn install(&mut self, source: &str) -> Result<Vec<String>, String> {
        let home = dirs::home_dir().ok_or("could not determine home directory")?;
        let skill_dir = home.join(".metis").join("skills");
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("could not create skills dir: {e}"))?;

        let mut installed = Vec::new();

        if source.ends_with(".md")
            && (source.starts_with("http://") || source.starts_with("https://"))
        {
            // Direct raw URL to a single .md file
            let filename = source.rsplit('/').next().unwrap_or("skill.md");
            let dest = skill_dir.join(filename);
            let output = std::process::Command::new("curl")
                .args(["-fsSL", "-o", dest.to_str().unwrap(), source])
                .output()
                .map_err(|e| format!("curl failed: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "download failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            // Validate it parses as a skill
            if let Ok(content) = std::fs::read_to_string(&dest) {
                if let Some(skill) = parse_skill(&content, &dest) {
                    installed.push(skill.name.clone());
                    self.skills.retain(|s| s.name != skill.name);
                    self.skills.push(skill);
                } else {
                    let _ = std::fs::remove_file(&dest);
                    return Err(format!(
                        "{filename} is not a valid skill file (missing frontmatter)"
                    ));
                }
            }
        } else if source.starts_with("http://")
            || source.starts_with("https://")
            || source.contains("github.com")
        {
            // Git repository URL — clone to temp dir, copy skill files
            let tmp = std::env::temp_dir().join(format!(
                "metis-skill-install-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            ));
            let output = std::process::Command::new("git")
                .args(["clone", "--depth", "1", source, tmp.to_str().unwrap()])
                .output()
                .map_err(|e| format!("git clone failed: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            // Scan root and skills/ subdir for .md files
            for search_dir in &[tmp.clone(), tmp.join("skills")] {
                if let Ok(entries) = std::fs::read_dir(search_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().map(|e| e == "md").unwrap_or(false) {
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                if let Some(skill) = parse_skill(&content, &path) {
                                    let dest = skill_dir.join(format!("{}.md", skill.name));
                                    let _ = std::fs::copy(&path, &dest);
                                    installed.push(skill.name.clone());
                                    self.skills.retain(|s| s.name != skill.name);
                                    self.skills.push(Skill {
                                        source: dest,
                                        ..skill
                                    });
                                }
                            }
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&tmp);
            if installed.is_empty() {
                return Err("no valid skill files found in repository".to_string());
            }
        } else {
            // Local path
            let path = std::path::Path::new(source);
            if !path.exists() {
                return Err(format!("file not found: {source}"));
            }
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("could not read {source}: {e}"))?;
            let skill = parse_skill(&content, path)
                .ok_or_else(|| format!("{source} is not a valid skill file"))?;
            let dest = skill_dir.join(format!("{}.md", skill.name));
            std::fs::copy(path, &dest).map_err(|e| format!("copy failed: {e}"))?;
            installed.push(skill.name.clone());
            self.skills.retain(|s| s.name != skill.name);
            self.skills.push(Skill {
                source: dest,
                ..skill
            });
        }

        Ok(installed)
    }

    /// List all available skill sources (installed skill names and paths).
    pub fn installed_skills(&self) -> Vec<(&str, &Path)> {
        self.skills
            .iter()
            .map(|s| (s.name.as_str(), s.source.as_path()))
            .collect()
    }

    /// Remove an installed skill by name. Deletes the file from
    /// `~/.metis/skills/` and removes it from the in-memory registry.
    pub fn uninstall(&mut self, name: &str) -> Result<(), String> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("skill `{name}` not found"))?;

        if skill.source == Path::new("<builtin>") {
            return Err(format!(
                "`{name}` is a built-in skill and cannot be uninstalled"
            ));
        }

        // Remove the file
        if skill.source.exists() {
            std::fs::remove_file(&skill.source)
                .map_err(|e| format!("could not remove {}: {e}", skill.source.display()))?;
        }

        self.skills.retain(|s| s.name != name);
        Ok(())
    }

    /// Search skills by query string (matches name, description, and tags).
    pub fn search(&self, query: &str) -> Vec<&Skill> {
        let q = query.to_lowercase();
        self.skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&q)
                    || s.description.to_lowercase().contains(&q)
                    || s.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }
}

/// Keyword-based auto-skill classifier.
///
/// Scores each skill by counting token overlap between the user prompt
/// and the skill's name + description. Returns the best match when the
/// score is at least `MIN_SCORE` (avoids false positives on sparse prompts).
///
/// This replaces the previous approach of making a blocking LLM classification
/// call — identical accuracy for exact-name invocations, zero latency, zero cost.
pub fn classify_skill(prompt: &str, skills: &[(String, String)]) -> Option<String> {
    const MIN_SCORE: u32 = 2;

    let lower = prompt.to_lowercase();
    let prompt_words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).collect();

    let mut best: Option<(String, u32)> = None;

    for (name, description) in skills {
        let mut score: u32 = 0;

        // Exact name match in prompt is a strong signal
        if lower.contains(&name.to_lowercase()) {
            score += 4;
        }

        // Word overlap between prompt and (name + description)
        let skill_text = format!("{name} {description}").to_lowercase();
        let skill_words: Vec<&str> = skill_text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .collect();

        for pw in &prompt_words {
            if skill_words.contains(pw) {
                score += 1;
            }
        }

        if score >= MIN_SCORE {
            if best.as_ref().map_or(true, |(_, bs)| score > *bs) {
                best = Some((name.clone(), score));
            }
        }
    }

    best.map(|(name, _)| name)
}

/// Built-in skill definitions. These are registered by default and
/// can be overridden by user/workspace skill files.
pub fn builtin_skills() -> Vec<Skill> {
    let builtin = |name: &str, desc: &str, prompt: &str, tags: &[&str]| Skill {
        name: name.into(),
        description: desc.into(),
        user_invocable: true,
        prompt: prompt.into(),
        source: PathBuf::from("<builtin>"),
        author: Some("metis".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    };
    vec![
        builtin(
            "commit",
            "Create a git commit from staged/unstaged changes",
            "Look at the current git status and diff. Create a well-crafted \
             git commit with a concise message that describes the changes. \
             Stage relevant files first. $ARGS",
            &["git", "workflow"],
        ),
        builtin(
            "review-pr",
            "Review a pull request",
            "Review the pull request. Check the diff for bugs, style issues, \
             missing tests, and security concerns. Provide actionable feedback. \
             $ARGS",
            &["git", "review"],
        ),
        builtin(
            "test",
            "Run the project's test suite",
            "Run the project's test suite and report results. If tests fail, \
             investigate the failures. $ARGS",
            &["testing", "ci"],
        ),
    ]
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_basic() {
        let content = r#"---
name: commit
description: Create a git commit
user_invocable: true
---
Stage and commit changes. $ARGS"#;
        let skill = parse_skill(content, Path::new("commit.md")).unwrap();
        assert_eq!(skill.name, "commit");
        assert_eq!(skill.description, "Create a git commit");
        assert!(skill.user_invocable);
        assert!(skill.prompt.contains("$ARGS"));
    }

    #[test]
    fn parse_skill_defaults() {
        let content = "---\ndescription: test\n---\ndo something";
        let skill = parse_skill(content, Path::new("my-skill.md")).unwrap();
        assert_eq!(skill.name, "my-skill"); // from filename
        assert!(skill.user_invocable); // default true
    }

    #[test]
    fn parse_skill_no_frontmatter_returns_none() {
        assert!(parse_skill("just plain text", Path::new("x.md")).is_none());
    }

    #[test]
    fn expand_prompt_replaces_args() {
        let skill = Skill {
            name: "test".into(),
            description: "".into(),
            user_invocable: true,
            prompt: "Run tests $ARGS in verbose mode".into(),
            source: PathBuf::from("test.md"),
            ..Default::default()
        };
        let expanded = expand_prompt(&skill, "--filter unit");
        assert_eq!(expanded, "Run tests --filter unit in verbose mode");
    }

    #[test]
    fn expand_prompt_empty_args() {
        let skill = Skill {
            name: "test".into(),
            description: "".into(),
            user_invocable: true,
            prompt: "Run tests $ARGS".into(),
            source: PathBuf::from("test.md"),
            ..Default::default()
        };
        let expanded = expand_prompt(&skill, "");
        assert_eq!(expanded, "Run tests ");
    }

    #[test]
    fn registry_discover_from_disk() {
        let dir = std::env::temp_dir().join(format!("metis-skill-test-{}", std::process::id()));
        let skill_dir = dir.join(".metis").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("deploy.md"),
            "---\nname: deploy\ndescription: Deploy app\n---\nDeploy the app $ARGS",
        )
        .unwrap();

        let reg = SkillRegistry::discover(&dir);
        assert!(reg.get("deploy").is_some());
        assert_eq!(reg.get("deploy").unwrap().description, "Deploy app");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_workspace_overrides_user() {
        let mut reg = SkillRegistry::new();
        reg.register(Skill {
            name: "commit".into(),
            description: "user version".into(),
            user_invocable: true,
            prompt: "user prompt".into(),
            source: PathBuf::from("user"),
            ..Default::default()
        });
        reg.register(Skill {
            name: "commit".into(),
            description: "workspace version".into(),
            user_invocable: true,
            prompt: "workspace prompt".into(),
            source: PathBuf::from("workspace"),
            ..Default::default()
        });
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("commit").unwrap().description, "workspace version");
    }

    #[test]
    fn builtin_skills_exist() {
        let builtins = builtin_skills();
        assert!(builtins.len() >= 3);
        let names: Vec<_> = builtins.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"commit"));
        assert!(names.contains(&"review-pr"));
        assert!(names.contains(&"test"));
    }

    #[test]
    fn parse_skill_with_extended_metadata() {
        let content = r#"---
name: deploy
description: Deploy the application
author: nakata
version: 1.2.0
tags: devops, ci, deploy
user_invocable: true
---
Deploy $ARGS"#;
        let skill = parse_skill(content, Path::new("deploy.md")).unwrap();
        assert_eq!(skill.author.as_deref(), Some("nakata"));
        assert_eq!(skill.version.as_deref(), Some("1.2.0"));
        assert_eq!(skill.tags, vec!["devops", "ci", "deploy"]);
    }

    #[test]
    fn search_matches_name_description_tags() {
        let mut reg = SkillRegistry::new();
        reg.register(Skill {
            name: "deploy".into(),
            description: "Deploy app to production".into(),
            tags: vec!["devops".into(), "ci".into()],
            ..Default::default()
        });
        reg.register(Skill {
            name: "lint".into(),
            description: "Run linter".into(),
            tags: vec!["quality".into()],
            ..Default::default()
        });

        assert_eq!(reg.search("deploy").len(), 1);
        assert_eq!(reg.search("production").len(), 1); // matches description
        assert_eq!(reg.search("devops").len(), 1); // matches tag
        assert_eq!(reg.search("xyz").len(), 0);
    }

    #[test]
    fn uninstall_removes_skill() {
        let dir = std::env::temp_dir().join(format!("metis-uninst-{}", std::process::id()));
        let skill_dir = dir.join(".metis").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let file = skill_dir.join("removeme.md");
        std::fs::write(
            &file,
            "---\nname: removeme\ndescription: temp\n---\ndo stuff",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        reg.load_dir(&skill_dir);
        assert!(reg.get("removeme").is_some());

        reg.uninstall("removeme").unwrap();
        assert!(reg.get("removeme").is_none());
        assert!(!file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_rejects_builtin() {
        let mut reg = SkillRegistry::new();
        for s in builtin_skills() {
            reg.register(s);
        }
        assert!(reg.uninstall("commit").is_err());
    }

    #[test]
    fn expand_prompt_full_replaces_variables() {
        let skill = Skill {
            prompt: "In $WORKSPACE using $MODEL: $ARGS".into(),
            ..Default::default()
        };
        let result = super::expand_prompt_full(&skill, "test", Path::new("/tmp/proj"), "gpt-4o");
        assert!(result.contains("/tmp/proj"));
        assert!(result.contains("gpt-4o"));
        assert!(result.contains("test"));
    }

    #[test]
    fn user_invocable_filters_correctly() {
        let mut reg = SkillRegistry::new();
        reg.register(Skill {
            name: "public".into(),
            description: "".into(),
            user_invocable: true,
            prompt: "".into(),
            source: PathBuf::from("a"),
            ..Default::default()
        });
        reg.register(Skill {
            name: "internal".into(),
            description: "".into(),
            user_invocable: false,
            prompt: "".into(),
            source: PathBuf::from("b"),
            ..Default::default()
        });
        let invocable = reg.user_invocable();
        assert_eq!(invocable.len(), 1);
        assert_eq!(invocable[0].name, "public");
    }

    #[test]
    fn classify_skill_exact_name_match() {
        let skills = vec![
            ("commit".to_string(), "Create a git commit".to_string()),
            ("deploy".to_string(), "Deploy the application".to_string()),
        ];
        assert_eq!(classify_skill("commit the changes", &skills).as_deref(), Some("commit"));
    }

    #[test]
    fn classify_skill_description_overlap() {
        let skills = vec![
            ("commit".to_string(), "Create a git commit from staged changes".to_string()),
            ("test".to_string(), "Run the project test suite".to_string()),
        ];
        // "test" appears in description of "test" skill
        assert_eq!(classify_skill("run the test suite now", &skills).as_deref(), Some("test"));
    }

    #[test]
    fn classify_skill_no_match_returns_none() {
        let skills = vec![
            ("commit".to_string(), "Create a git commit".to_string()),
            ("deploy".to_string(), "Deploy the application".to_string()),
        ];
        assert_eq!(classify_skill("what is the weather", &skills), None);
    }

    #[test]
    fn classify_skill_empty_skills_returns_none() {
        assert_eq!(classify_skill("commit everything", &[]), None);
    }

    #[test]
    fn classify_skill_prefers_higher_score() {
        let skills = vec![
            ("commit".to_string(), "Create git commit staged changes".to_string()),
            ("test".to_string(), "Run test suite verify code".to_string()),
        ];
        // "commit" has higher overlap with this prompt
        let result = classify_skill("create and commit all staged changes to git", &skills);
        assert_eq!(result.as_deref(), Some("commit"));
    }
}
