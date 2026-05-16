//! Self-update: check for new releases on GitHub and replace the
//! running binary with the latest version.
//!
//! Flow:
//! 1. Query `GET /repos/{owner}/{repo}/releases/latest` for the tag.
//! 2. Compare semver against the compiled-in `CURRENT_VERSION`.
//! 3. Download the platform-specific tarball asset.
//! 4. Extract the binary and atomically replace the running executable.

use std::path::PathBuf;
use thiserror::Error;

const REPO_OWNER: &str = "nakata-app";
const REPO_NAME: &str = "metis";

/// The version baked into this binary at compile time.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("github api error: {0}")]
    Api(String),
    #[error("no matching asset for platform {0}")]
    NoAsset(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("already up to date (v{0})")]
    AlreadyLatest(String),
}

/// Result of a version check against GitHub releases.
#[derive(Debug, Clone)]
pub struct VersionCheck {
    pub current: String,
    pub latest: String,
    pub download_url: Option<String>,
    pub is_newer: bool,
}

/// Detect the current platform's target triple.
pub fn current_target() -> &'static str {
    // Compile-time target detection
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        "unknown"
    }
}

/// Construct the expected asset filename for a given version and target.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("metis-v{version}-{target}.tar.gz")
}

/// Compare two semver strings. Returns true if `latest` is newer than `current`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<&str> = s.split('.').collect();
        let major = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(latest) > parse(current)
}

/// Check the latest release version from GitHub.
pub async fn check_latest(client: &reqwest::Client) -> Result<VersionCheck, UpdateError> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let resp = client
        .get(&url)
        .header("User-Agent", format!("metis/{CURRENT_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(UpdateError::Api(format!(
            "GitHub API returned {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp.json().await?;
    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| UpdateError::Api("missing tag_name".into()))?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let target = current_target();
    let expected_asset = asset_name(version, target);

    // Find matching asset URL
    let download_url = body["assets"].as_array().and_then(|assets| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            if name == expected_asset {
                a["browser_download_url"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
    });

    Ok(VersionCheck {
        current: CURRENT_VERSION.to_string(),
        latest: version.to_string(),
        download_url,
        is_newer: is_newer(CURRENT_VERSION, version),
    })
}

/// Download the release asset, extract, and replace the current binary.
pub async fn perform_update(
    client: &reqwest::Client,
    check: &VersionCheck,
) -> Result<PathBuf, UpdateError> {
    if !check.is_newer {
        return Err(UpdateError::AlreadyLatest(check.current.clone()));
    }
    let url = check
        .download_url
        .as_deref()
        .ok_or_else(|| UpdateError::NoAsset(current_target().to_string()))?;

    // Download tarball
    let resp = client
        .get(url)
        .header("User-Agent", format!("metis/{CURRENT_VERSION}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(UpdateError::Api(format!(
            "asset download failed: {}",
            resp.status()
        )));
    }

    let bytes = resp.bytes().await?;

    // Extract to a temp file
    let current_exe = std::env::current_exe()?;
    let parent = current_exe
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let tmp_path = parent.join(".metis-update-tmp");

    // Decompress .tar.gz and find the `metis` binary inside
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(&bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut found = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if filename == "metis" {
            let mut out = std::fs::File::create(&tmp_path)?;
            std::io::copy(&mut entry, &mut out)?;
            found = true;
            break;
        }
    }
    if !found {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(UpdateError::Api("binary not found in tarball".into()));
    }

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Atomic replace: rename old → .backup, new → old
    let backup_path = parent.join(".metis-old");
    let _ = std::fs::remove_file(&backup_path);
    std::fs::rename(&current_exe, &backup_path)?;
    std::fs::rename(&tmp_path, &current_exe)?;
    let _ = std::fs::remove_file(&backup_path);

    Ok(current_exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.3.0", "0.4.0"));
        assert!(is_newer("0.3.0", "0.3.1"));
        assert!(is_newer("0.3.0", "1.0.0"));
        assert!(!is_newer("0.4.0", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(is_newer("0.3.0", "v0.4.0"));
    }

    #[test]
    fn current_target_is_known() {
        let t = current_target();
        assert_ne!(t, "unknown", "target should be detected");
        assert!(
            t.contains("apple") || t.contains("linux"),
            "unexpected target: {t}"
        );
    }

    #[test]
    fn asset_name_format() {
        let name = asset_name("0.4.0", "aarch64-apple-darwin");
        assert_eq!(name, "metis-v0.4.0-aarch64-apple-darwin.tar.gz");
    }

    #[test]
    fn current_version_is_set() {
        assert!(!CURRENT_VERSION.is_empty());
        assert!(
            CURRENT_VERSION.contains('.'),
            "version should be semver: {CURRENT_VERSION}"
        );
    }
}
