//! `aegis deploy` — deploy to configured targets.
//!
//! Supports multiple deployment backends:
//! - **Cloudflare**: runs `wrangler deploy` (or a custom command).
//! - **GitHub**: creates a git tag and a GitHub release via `gh`.
//! - **SSH**: builds locally, scp's the binary, restarts a remote service.
//! - **Auto**: reads `[deploy]` from config and runs all configured targets.
//!
//! Configuration lives in `.aegis/config.toml` under `[deploy]`:
//!
//! ```toml
//! [deploy]
//! targets = ["cloudflare", "github"]
//!
//! [deploy.cloudflare]
//! command = "wrangler deploy"
//!
//! [deploy.github]
//! tag_prefix = "v"
//! draft = false
//!
//! [deploy.ssh]
//! host = "user@server"
//! path = "/opt/app"
//! service = "myapp"
//! build_command = "cargo build --release"
//! ```

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// The turquoise colour used across Metis output.
const C: &str = "\x1b[38;2;0;229;209m";
/// Dim style.
const DIM: &str = "\x1b[2m";
/// Reset style.
const R: &str = "\x1b[0m";

// ── Target enum ──────────────────────────────────────────────────────

/// Which deployment target to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployTarget {
    Cloudflare,
    GitHub,
    Ssh,
    /// Read targets from config and run them in sequence.
    Auto,
}

impl DeployTarget {
    /// Parse a user-supplied target name. Returns `None` for unrecognised
    /// values — the caller should surface an error with the list of valid
    /// names.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cloudflare" | "cf" | "wrangler" => Some(Self::Cloudflare),
            "github" | "gh" => Some(Self::GitHub),
            "ssh" | "remote" => Some(Self::Ssh),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::GitHub => "github",
            Self::Ssh => "ssh",
            Self::Auto => "auto",
        }
    }
}

// ── Config structs ───────────────────────────────────────────────────

/// Top-level `[deploy]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeployConfig {
    /// Ordered list of target names to run when no explicit target is
    /// given on the command line.
    pub targets: Vec<String>,
    /// Cloudflare-specific settings.
    pub cloudflare: CloudflareConfig,
    /// GitHub-specific settings.
    pub github: GitHubConfig,
    /// SSH-specific settings.
    pub ssh: SshConfig,
}

/// `[deploy.cloudflare]`
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CloudflareConfig {
    /// Shell command to execute. Defaults to `wrangler deploy`.
    pub command: String,
}

impl Default for CloudflareConfig {
    fn default() -> Self {
        Self {
            command: "wrangler deploy".into(),
        }
    }
}

/// `[deploy.github]`
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GitHubConfig {
    /// Prefix prepended to the version when creating tags, e.g. `"v"`.
    pub tag_prefix: String,
    /// Create the release as a draft.
    pub draft: bool,
    /// Explicit tag name — overrides auto-detection from Cargo.toml.
    pub tag: Option<String>,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            tag_prefix: "v".into(),
            draft: false,
            tag: None,
        }
    }
}

/// `[deploy.ssh]`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    /// SSH destination, e.g. `"user@server"`.
    pub host: String,
    /// Remote directory to place the binary in.
    pub path: String,
    /// systemd (or other) service name to restart after deploying.
    pub service: String,
    /// Local build command. Defaults to `cargo build --release`.
    pub build_command: String,
    /// Name of the binary to scp. If empty, derived from the current
    /// directory name.
    pub binary: String,
}

// ── Public entry point ───────────────────────────────────────────────

/// Run the deploy pipeline for the given target (or all configured
/// targets when `Auto`).
pub async fn run_deploy(
    target: DeployTarget,
    workspace: &Path,
    config: &DeployConfig,
) -> Result<()> {
    eprintln!("{C}[deploy]{R} starting deploy");

    if target == DeployTarget::Auto {
        if config.targets.is_empty() {
            bail!(
                "no deploy targets configured — add [deploy] targets = [\"cloudflare\"] \
                 to .metis/config.toml, or specify a target: metis deploy <target>"
            );
        }
        for name in &config.targets {
            let t = DeployTarget::parse(name).with_context(|| {
                format!(
                    "unknown deploy target `{name}` in config — valid targets: \
                     cloudflare, github, ssh"
                )
            })?;
            run_single(&t, workspace, config).await?;
        }
    } else {
        run_single(&target, workspace, config).await?;
    }

    eprintln!("{C}[deploy]{R} all targets complete");
    Ok(())
}

async fn run_single(target: &DeployTarget, workspace: &Path, config: &DeployConfig) -> Result<()> {
    eprintln!("{C}[deploy]{R} target: {}", target.label());
    match target {
        DeployTarget::Cloudflare => deploy_cloudflare(workspace, &config.cloudflare),
        DeployTarget::GitHub => deploy_github(workspace, &config.github),
        DeployTarget::Ssh => deploy_ssh(workspace, &config.ssh),
        DeployTarget::Auto => unreachable!(),
    }
}

// ── Individual targets ───────────────────────────────────────────────

fn shell(cmd: &str, workspace: &Path) -> Result<()> {
    eprintln!("{DIM}$ {cmd}{R}");
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .status()
        .with_context(|| format!("failed to spawn: {cmd}"))?;
    if !status.success() {
        bail!(
            "command failed (exit {}): {cmd}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

#[cfg(test)]
fn shell_output(cmd: &str, workspace: &Path) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output()
        .with_context(|| format!("failed to spawn: {cmd}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed: {cmd}\n{stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Deploy to Cloudflare Workers via wrangler.
fn deploy_cloudflare(workspace: &Path, cfg: &CloudflareConfig) -> Result<()> {
    eprintln!("{C}[cloudflare]{R} running: {}", cfg.command);
    shell(&cfg.command, workspace).context("cloudflare deploy failed")
}

/// Create a git tag and GitHub release via the `gh` CLI.
fn deploy_github(workspace: &Path, cfg: &GitHubConfig) -> Result<()> {
    // Check gh CLI is available
    if !std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        bail!("gh CLI not found — install via https://cli.github.com");
    }
    // Determine the tag name.
    let tag = if let Some(ref explicit) = cfg.tag {
        explicit.clone()
    } else {
        // Try to read version from Cargo.toml in workspace root.
        let version = read_cargo_version(workspace).unwrap_or_else(|_| "0.0.0".into());
        format!("{}{}", cfg.tag_prefix, version)
    };

    eprintln!("{C}[github]{R} creating tag: {tag}");
    shell(&format!("git tag -a {tag} -m \"Release {tag}\""), workspace)
        .context("git tag failed")?;
    shell("git push --tags", workspace).context("git push --tags failed")?;

    let draft_flag = if cfg.draft { " --draft" } else { "" };
    let cmd = format!("gh release create {tag} --generate-notes{draft_flag}");
    eprintln!("{C}[github]{R} creating release: {tag}");
    shell(&cmd, workspace).context("gh release create failed")
}

/// Build locally, scp binary to remote host, restart service.
fn deploy_ssh(workspace: &Path, cfg: &SshConfig) -> Result<()> {
    if cfg.host.is_empty() {
        bail!("deploy.ssh.host is not configured");
    }
    if cfg.path.is_empty() {
        bail!("deploy.ssh.path is not configured");
    }
    // Check scp is available
    if !std::process::Command::new("which")
        .arg("scp")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        bail!("scp not found — required for SSH deployment");
    }

    // Build
    let build_cmd = if cfg.build_command.is_empty() {
        "cargo build --release"
    } else {
        &cfg.build_command
    };
    eprintln!("{C}[ssh]{R} building: {build_cmd}");
    shell(build_cmd, workspace).context("build failed")?;

    // Determine binary name.
    let binary = if cfg.binary.is_empty() {
        workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into())
    } else {
        cfg.binary.clone()
    };

    let local_path = workspace.join("target").join("release").join(&binary);
    if !local_path.exists() {
        bail!(
            "expected binary at {} — check build_command and binary name",
            local_path.display()
        );
    }

    let remote_dest = format!("{}:{}/{}", cfg.host, cfg.path, binary);
    eprintln!("{C}[ssh]{R} uploading to {remote_dest}");
    shell(
        &format!("scp {} {}", local_path.display(), remote_dest),
        workspace,
    )
    .context("scp failed")?;

    // Restart service if configured.
    if !cfg.service.is_empty() {
        eprintln!("{C}[ssh]{R} restarting service: {}", cfg.service);
        shell(
            &format!("ssh {} sudo systemctl restart {}", cfg.host, cfg.service),
            workspace,
        )
        .context("service restart failed")?;
    }

    Ok(())
}

/// Read the `version` field from `Cargo.toml` in the given directory.
fn read_cargo_version(workspace: &Path) -> Result<String> {
    let cargo_toml = workspace.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml).context("could not read Cargo.toml")?;

    // Minimal parse — look for `version = "X.Y.Z"` in the [package] section.
    // Using toml crate for robustness.
    let table: toml::Table = toml::from_str(&content).context("could not parse Cargo.toml")?;
    let version = table
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .context("no package.version in Cargo.toml")?;
    Ok(version.to_string())
}

// ── Config loading helper ────────────────────────────────────────────

/// Load deploy config from the workspace's `.metis/config.toml`.
/// Returns defaults if the section is missing.
pub fn load_deploy_config(workspace: &Path) -> DeployConfig {
    // Try workspace-local first, then global.
    for dir in &[
        workspace.join(".metis"),
        dirs::home_dir().unwrap_or_default().join(".metis"),
    ] {
        let path = dir.join("config.toml");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(table) = toml::from_str::<toml::Table>(&content) {
                if let Some(deploy_val) = table.get("deploy") {
                    if let Ok(dc) = deploy_val.clone().try_into::<DeployConfig>() {
                        return dc;
                    }
                }
            }
        }
    }
    DeployConfig::default()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_variants() {
        assert_eq!(
            DeployTarget::parse("cloudflare"),
            Some(DeployTarget::Cloudflare)
        );
        assert_eq!(DeployTarget::parse("cf"), Some(DeployTarget::Cloudflare));
        assert_eq!(
            DeployTarget::parse("wrangler"),
            Some(DeployTarget::Cloudflare)
        );
        assert_eq!(DeployTarget::parse("github"), Some(DeployTarget::GitHub));
        assert_eq!(DeployTarget::parse("gh"), Some(DeployTarget::GitHub));
        assert_eq!(DeployTarget::parse("ssh"), Some(DeployTarget::Ssh));
        assert_eq!(DeployTarget::parse("remote"), Some(DeployTarget::Ssh));
        assert_eq!(DeployTarget::parse("unknown"), None);
    }

    #[test]
    fn parse_target_case_insensitive() {
        assert_eq!(
            DeployTarget::parse("CloudFlare"),
            Some(DeployTarget::Cloudflare)
        );
        assert_eq!(DeployTarget::parse("GITHUB"), Some(DeployTarget::GitHub));
        assert_eq!(DeployTarget::parse("SSH"), Some(DeployTarget::Ssh));
    }

    #[test]
    fn target_label() {
        assert_eq!(DeployTarget::Cloudflare.label(), "cloudflare");
        assert_eq!(DeployTarget::GitHub.label(), "github");
        assert_eq!(DeployTarget::Ssh.label(), "ssh");
        assert_eq!(DeployTarget::Auto.label(), "auto");
    }

    #[test]
    fn default_cloudflare_config() {
        let cfg = CloudflareConfig::default();
        assert_eq!(cfg.command, "wrangler deploy");
    }

    #[test]
    fn default_github_config() {
        let cfg = GitHubConfig::default();
        assert_eq!(cfg.tag_prefix, "v");
        assert!(!cfg.draft);
        assert!(cfg.tag.is_none());
    }

    #[test]
    fn parse_full_deploy_config() {
        let toml_str = r#"
            targets = ["cloudflare", "github"]

            [cloudflare]
            command = "wrangler publish"

            [github]
            tag_prefix = "release-"
            draft = true

            [ssh]
            host = "deploy@prod"
            path = "/opt/myapp"
            service = "myapp"
            build_command = "cargo build --release --target x86_64-unknown-linux-gnu"
            binary = "myapp"
        "#;
        let cfg: DeployConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.targets, vec!["cloudflare", "github"]);
        assert_eq!(cfg.cloudflare.command, "wrangler publish");
        assert_eq!(cfg.github.tag_prefix, "release-");
        assert!(cfg.github.draft);
        assert_eq!(cfg.ssh.host, "deploy@prod");
        assert_eq!(cfg.ssh.path, "/opt/myapp");
        assert_eq!(cfg.ssh.service, "myapp");
        assert_eq!(cfg.ssh.binary, "myapp");
    }

    #[test]
    fn parse_minimal_deploy_config() {
        let toml_str = r#"
            targets = ["cloudflare"]
        "#;
        let cfg: DeployConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.targets, vec!["cloudflare"]);
        assert_eq!(cfg.cloudflare.command, "wrangler deploy");
    }

    #[test]
    fn load_deploy_config_missing_dir() {
        let cfg = load_deploy_config(Path::new("/nonexistent/path"));
        assert!(cfg.targets.is_empty());
    }

    #[test]
    fn shell_output_helper() {
        let result = shell_output("echo hello", Path::new("/tmp"));
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn shell_failure_propagates() {
        let result = shell("false", Path::new("/tmp"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn auto_target_with_empty_config_errors() {
        let cfg = DeployConfig::default();
        let result = run_deploy(DeployTarget::Auto, Path::new("/tmp"), &cfg).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("no deploy targets configured"));
    }

    #[test]
    fn ssh_deploy_requires_host() {
        let cfg = SshConfig::default();
        let result = deploy_ssh(Path::new("/tmp"), &cfg);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("host"));
    }
}
