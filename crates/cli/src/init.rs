//! `aegis init` — interactive workspace bootstrap.
//!
//! Closes the v0.10 onboarding gap: instead of asking new users to
//! discover the config schema by reading source, this subcommand walks
//! them through the four decisions that actually matter on a fresh
//! clone — provider, model, daily budget, auto-router on/off — and
//! writes a minimal `<workspace>/.metis/config.toml` from the answers.
//!
//! Design constraints:
//! - **Stdin-driven, no TUI dependency.** Boots before the agent loop
//!   so we can't reuse the ratatui surface; the prompts are plain
//!   stderr lines and a `[1] / [2] / …` numeric pick.
//! - **Non-interactive sessions abort cleanly.** When stdin is not a
//!   terminal (CI, piped script) we refuse to invent defaults, so a
//!   wrong answer never silently lands in someone's config.
//! - **Don't clobber an existing config.** If the workspace already
//!   has a `.metis/config.toml`, we ask before overwriting and print
//!   the path on bail so the user can edit it directly.
//! - **Provider list comes from `aegis_api::Provider::BUILTINS`.** No
//!   hard-coded second source — keeps the wizard in sync with the
//!   real provider table automatically.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Entry point invoked from `main::run` when the user types
/// `aegis init`. Performs the wizard and writes config; returns Ok
/// after a successful write or a user-confirmed no-op.
pub fn run(workspace: &Path) -> Result<()> {
    let target = workspace.join(".metis").join("config.toml");
    if target.exists() {
        eprintln!(
            "\x1b[1;33m[aegis init]\x1b[0m {} already exists",
            target.display()
        );
        eprintln!("Overwriting will replace your existing settings.");
        if !confirm("Overwrite?", false)? {
            eprintln!("\x1b[2m[aegis init] aborted, no changes\x1b[0m");
            return Ok(());
        }
    }

    eprintln!("\x1b[1;36maegis init\x1b[0m — minimal config wizard");
    eprintln!("press \x1b[1mEnter\x1b[0m to accept the [default] in brackets, or Ctrl+C to bail.\n");

    let providers = aegis_api::Provider::BUILTINS;
    eprintln!("\x1b[1mProviders:\x1b[0m");
    for (i, p) in providers.iter().enumerate() {
        let envset = if p.env_var.is_empty() {
            "no key needed".to_string()
        } else if std::env::var(p.env_var).is_ok() {
            format!("{} \x1b[32m✓ set\x1b[0m", p.env_var)
        } else {
            format!("{} \x1b[31m✗ missing\x1b[0m", p.env_var)
        };
        eprintln!(
            "  [{:>2}] {:<12} default={:<32} {}",
            i + 1,
            p.id,
            p.default_model,
            envset
        );
    }
    let provider_idx = pick_index("Provider number", providers.len(), default_provider_index())?;
    let provider = &providers[provider_idx];

    let model = prompt_string(
        &format!("Default model for `{}`", provider.id),
        Some(provider.default_model),
    )?;

    let budget_raw = prompt_string("Daily USD budget (empty = no cap)", None)?;
    let daily_budget_usd: Option<f64> = if budget_raw.trim().is_empty() {
        None
    } else {
        Some(
            budget_raw
                .trim()
                .parse::<f64>()
                .with_context(|| format!("`{budget_raw}` is not a number"))?,
        )
    };

    let auto_route = confirm("Enable auto-router (cheap fast / strong slow swap)?", false)?;
    let (fast_model, strong_model) = if auto_route {
        let fast = prompt_string(
            "Fast model (Plan / simple prompts)",
            Some(provider.default_model),
        )?;
        let strong = prompt_string("Strong model (AcceptEdits / complex prompts)", None)?;
        (Some(fast), if strong.trim().is_empty() { None } else { Some(strong) })
    } else {
        (None, None)
    };

    let yes = confirm(
        "Auto-approve all tool calls (yolo / CI mode)?",
        false,
    )?;

    write_config(
        &target,
        WizardChoices {
            provider: provider.id.to_string(),
            model,
            daily_budget_usd,
            auto_route,
            fast_model,
            strong_model,
            yes,
        },
    )?;

    eprintln!(
        "\n\x1b[1;32m[aegis init]\x1b[0m wrote {}",
        target.display()
    );
    if provider.env_var.is_empty() {
        eprintln!("Provider needs no API key, you're set.");
    } else if std::env::var(provider.env_var).is_err() {
        eprintln!(
            "\x1b[1;33mNote:\x1b[0m export {} before the next run.",
            provider.env_var
        );
    }
    eprintln!("Run goblin to start the REPL.");
    Ok(())
}

/// Emit a yes/no prompt; uppercase letter shows the default. Returns
/// `Err` on non-TTY stdin so unattended invocations don't accept the
/// silent default.
fn confirm(question: &str, default: bool) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    eprint!("{question} {suffix} ");
    let _ = std::io::stderr().flush();
    if !std::io::stdin().is_terminal() {
        bail!(
            "stdin is not a TTY — `aegis init` is interactive only. \
             Edit .metis/config.toml directly for headless setups."
        );
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read stdin")?;
    let trimmed = line.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default);
    }
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

/// Free-form string prompt with optional `[default]` echo. Empty input
/// returns the default (or empty string when no default).
fn prompt_string(question: &str, default: Option<&str>) -> Result<String> {
    if let Some(d) = default {
        eprint!("{question} [{d}]: ");
    } else {
        eprint!("{question}: ");
    }
    let _ = std::io::stderr().flush();
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a TTY");
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read stdin")?;
    let raw = line.trim();
    if raw.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(raw.to_string())
}

/// Pick a 1-based index from a list. Validates the parse + range and
/// re-prompts until the user supplies a usable number or hits EOF.
fn pick_index(question: &str, len: usize, default_one_based: usize) -> Result<usize> {
    loop {
        eprint!("{question} [1-{len}, default {default_one_based}]: ");
        let _ = std::io::stderr().flush();
        if !std::io::stdin().is_terminal() {
            bail!("stdin is not a TTY");
        }
        let mut line = String::new();
        if std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("failed to read stdin")?
            == 0
        {
            bail!("EOF on stdin while picking provider");
        }
        let raw = line.trim();
        if raw.is_empty() {
            return Ok(default_one_based.saturating_sub(1));
        }
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= len => return Ok(n - 1),
            Ok(_) => {
                eprintln!("\x1b[33m → number out of range, try again.\x1b[0m");
            }
            Err(_) => {
                eprintln!("\x1b[33m → not a number, try again.\x1b[0m");
            }
        }
    }
}

/// Reasonable default highlight: the first provider in `BUILTINS`
/// whose env var is already set. Falls back to slot 1 (`deepseek`)
/// when nothing is exported, so a brand-new shell still has a
/// recommended pick to press Enter on.
fn default_provider_index() -> usize {
    for (i, p) in aegis_api::Provider::BUILTINS.iter().enumerate() {
        if p.env_var.is_empty() {
            continue;
        }
        if std::env::var(p.env_var).is_ok() {
            return i + 1;
        }
    }
    1
}

#[derive(Debug)]
pub(crate) struct WizardChoices {
    pub provider: String,
    pub model: String,
    pub daily_budget_usd: Option<f64>,
    pub auto_route: bool,
    pub fast_model: Option<String>,
    pub strong_model: Option<String>,
    pub yes: bool,
}

/// Render the chosen settings to TOML and persist them under
/// `<workspace>/.metis/`. Pulled out of `run` so the unit tests can
/// hit the formatter without spinning up an interactive prompt.
pub(crate) fn write_config(target: &Path, c: WizardChoices) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create config dir {}", parent.display())
        })?;
    }
    let mut out = String::new();
    out.push_str("# Generated by `aegis init`. Edit freely; rerun the\n");
    out.push_str("# wizard to overwrite or use any TOML editor.\n\n");
    out.push_str(&format!("provider = \"{}\"\n", c.provider));
    out.push_str(&format!("model    = \"{}\"\n", c.model));
    if let Some(b) = c.daily_budget_usd {
        out.push_str(&format!("daily_budget_usd = {b}\n"));
    }
    if c.yes {
        out.push_str("yes = true\n");
    }
    if c.auto_route {
        out.push_str("\n[routing]\n");
        out.push_str("auto_route = true\n");
        if let Some(fm) = c.fast_model.as_ref() {
            out.push_str(&format!("fast_model = \"{fm}\"\n"));
        }
        if let Some(sm) = c.strong_model.as_ref() {
            out.push_str(&format!("strong_model = \"{sm}\"\n"));
        }
    }
    let bytes = out.as_bytes();
    std::fs::write(target, bytes)
        .with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

/// Public test helper exposed only behind `cfg(test)` so the unit
/// tests in `mod tests` below can validate the writer against a
/// `tempdir` without mutating the user's real workspace.
#[cfg(test)]
pub(crate) fn write_for_test(target: &Path, c: WizardChoices) -> Result<()> {
    write_config(target, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn write_config_emits_minimal_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".metis").join("config.toml");
        write_for_test(
            &target,
            WizardChoices {
                provider: "deepseek".into(),
                model: "deepseek-v4-flash".into(),
                daily_budget_usd: None,
                auto_route: false,
                fast_model: None,
                strong_model: None,
                yes: false,
            },
        )
        .unwrap();
        let got = read(&target);
        assert!(got.contains("provider = \"deepseek\""));
        assert!(got.contains("model    = \"deepseek-v4-flash\""));
        // Optional fields must not leak as empty/false placeholders —
        // roundtrip through MetisConfig::default() depends on absence.
        assert!(!got.contains("daily_budget_usd"));
        assert!(!got.contains("yes ="));
        assert!(!got.contains("[routing]"));
    }

    #[test]
    fn write_config_includes_routing_block_when_auto_route_on() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".metis").join("config.toml");
        write_for_test(
            &target,
            WizardChoices {
                provider: "deepseek".into(),
                model: "deepseek-v4-flash".into(),
                daily_budget_usd: Some(5.0),
                auto_route: true,
                fast_model: Some("deepseek-v4-flash".into()),
                strong_model: Some("deepseek-v4-pro".into()),
                yes: true,
            },
        )
        .unwrap();
        let got = read(&target);
        assert!(got.contains("daily_budget_usd = 5"));
        assert!(got.contains("yes = true"));
        assert!(got.contains("[routing]"));
        assert!(got.contains("auto_route = true"));
        assert!(got.contains("fast_model = \"deepseek-v4-flash\""));
        assert!(got.contains("strong_model = \"deepseek-v4-pro\""));
    }

    #[test]
    fn write_config_creates_metis_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Note no .metis pre-created — write_config must mkdir -p.
        let target = tmp.path().join(".metis").join("config.toml");
        write_for_test(
            &target,
            WizardChoices {
                provider: "nvidia".into(),
                model: "deepseek-ai/deepseek-v4-flash".into(),
                daily_budget_usd: None,
                auto_route: false,
                fast_model: None,
                strong_model: None,
                yes: false,
            },
        )
        .unwrap();
        assert!(target.exists());
        assert!(target.parent().unwrap().is_dir());
    }
}

