//! Project alias registry — maps short names to absolute paths so
//! `aegis lingua` launches Aegis with `~/Projects/lingua` as the
//! workspace without shell-alias gymnastics.
//!
//! Storage: TOML at `~/.aegis/projects.toml`. Shape:
//!
//! ```toml
//! [projects]
//! lingua  = "/Users/macmini/Projects/lingua"
//! aegis   = "/Users/macmini/Projects/aegis"
//! wink    = "/Users/macmini/Projects/wink"
//! ```
//!
//! The file is created on first `aegis project add` and never touched
//! otherwise. Malformed TOML is treated as empty so a bad hand-edit
//! doesn't brick all future Aegis launches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub projects: BTreeMap<String, String>,
}

fn registry_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".metis").join("projects.toml"))
}

pub fn load() -> Registry {
    let Some(path) = registry_path() else {
        return Registry::default();
    };
    let Ok(s) = std::fs::read_to_string(&path) else {
        return Registry::default();
    };
    toml::from_str(&s).unwrap_or_default()
}

pub fn save(reg: &Registry) -> Result<()> {
    let path = registry_path().context("could not resolve home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let s = toml::to_string_pretty(reg).context("could not serialize registry")?;
    std::fs::write(&path, s).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Look up a project by name. Returns the absolute path if the name
/// exists AND the path still resolves to a directory. Stale entries
/// (deleted dirs) return `None` — caller falls back to "name is a
/// prompt, not a project" semantics.
pub fn resolve(name: &str) -> Option<PathBuf> {
    let reg = load();
    let raw = reg.projects.get(name)?;
    let path = PathBuf::from(raw);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

pub fn add(name: &str, path: &Path) -> Result<()> {
    if name.is_empty() {
        bail!("project name cannot be empty");
    }
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let absolute = std::fs::canonicalize(path)
        .with_context(|| format!("could not canonicalize {}", path.display()))?;
    let mut reg = load();
    reg.projects
        .insert(name.to_string(), absolute.display().to_string());
    save(&reg)
}

pub fn remove(name: &str) -> Result<bool> {
    let mut reg = load();
    let removed = reg.projects.remove(name).is_some();
    if removed {
        save(&reg)?;
    }
    Ok(removed)
}

pub fn list() -> Vec<(String, String)> {
    load().projects.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrips_empty() {
        let reg = Registry::default();
        let s = toml::to_string(&reg).unwrap();
        let parsed: Registry = toml::from_str(&s).unwrap();
        assert!(parsed.projects.is_empty());
    }

    #[test]
    fn registry_roundtrips_entries() {
        let mut reg = Registry::default();
        reg.projects.insert("foo".into(), "/tmp/foo".into());
        reg.projects.insert("bar".into(), "/tmp/bar".into());
        let s = toml::to_string(&reg).unwrap();
        let parsed: Registry = toml::from_str(&s).unwrap();
        assert_eq!(
            parsed.projects.get("foo").map(String::as_str),
            Some("/tmp/foo")
        );
        assert_eq!(
            parsed.projects.get("bar").map(String::as_str),
            Some("/tmp/bar")
        );
    }
}
