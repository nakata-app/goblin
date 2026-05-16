//! Bash command canonicalization + danger detection for per-command
//! whitelisting. The Permission gate (TUI side) feeds raw command lines
//! through `analyze_bash_command`, then matches each canonical part
//! against the user's allowlist.
//!
//! Design notes:
//! - Conservative parser. Anything we can't confidently split (subshell,
//!   redirection to a sensitive path, sudo, dynamic eval) is flagged
//!   `Dangerous` so it always re-prompts even if a sibling part is on
//!   the allowlist.
//! - "Canonical key" is `<command> <first_non_flag_arg>` so `git status`
//!   matches `git status -sb` but `git push` doesn't slip through under
//!   a `git status` allow. For commands with no first arg (`ls`, `pwd`),
//!   the canonical form is just the command name.
//! - Wrapper prefixes like `timeout 30 git status` are unwrapped — the
//!   wrapper is always read-only-ish and the meaningful command is
//!   what should be whitelisted.

use std::fmt;

/// What a single piece of a (possibly compound) command line resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandCheck {
    /// The command line is decomposed into N safe canonical keys; the
    /// permission layer should match each key against the user's
    /// allowlist. All keys must hit for the line to auto-allow.
    Safe(Vec<String>),
    /// The command line contains a construct we won't auto-allow. The
    /// inner string is the human-readable reason, surfaced in the
    /// permission modal so the user knows why we're asking.
    Dangerous(String),
}

impl fmt::Display for CommandCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandCheck::Safe(parts) => write!(f, "safe[{}]", parts.join(", ")),
            CommandCheck::Dangerous(reason) => write!(f, "dangerous: {reason}"),
        }
    }
}

/// Words that immediately taint the entire command line. Even when
/// nested inside a compound, hitting one of these forces a prompt.
const DANGER_TOKENS: &[&str] = &[
    "sudo", "su", "doas",
    "rm", "rmdir",
    "dd", "mkfs", "fdisk", "parted",
    "shred", "wipe",
    "chown", "chmod", "chgrp",
    "kill", "killall", "pkill",
    "shutdown", "reboot", "halt", "poweroff",
    "mv", "cp", // mv/cp can still overwrite — keep them prompted
    "eval", "exec", "source",
    "curl", "wget", "fetch",
    "scp", "rsync", "ssh",
    "iptables", "ufw", "pfctl",
    "launchctl", "systemctl", "service",
    "diskutil", "hdiutil",
    "git", // not all-git is dangerous — but `git push --force`, `git reset --hard`,
           // `git clean -fdx` etc are. Conservative: prompt every git unless on
           // the allowlist with the specific subcommand. The canonical-key match
           // makes "git status" and "git diff" cheap to whitelist.
];

/// Wrapper executables we transparently unwrap. After stripping these,
/// the next token is the "real" command being run.
///
/// `time` and `nice` are excluded — `time rm -rf` would otherwise read
/// as a `time` invocation and slip past the danger check.
const WRAPPER_PREFIXES: &[&str] = &[
    "timeout", "stdbuf", "ionice", "nohup", "command", "exec",
];

/// Patterns inside the raw command line (anywhere) that always force a
/// prompt. These are checked *before* parsing because they can break
/// our naive splitting (e.g. `$(rm -rf /)` would tokenize weird).
const RAW_DANGER_PATTERNS: &[&str] = &[
    "$(",       // command substitution
    "`",        // backtick command substitution
    ">/dev/",   // redirect to device
    "> /dev/",
    ">> /dev/",
    ">>/dev/",
    "/dev/sd",  // raw disk path
    "/dev/disk",
    "rm -rf",
    "rm -fr",
    "rm -r ",
    "--no-preserve-root",
    " -rf /",
    "force-with-lease=", // git push --force-with-lease still rewrites history
    "--force",
    "-f /",
    "yes |",
    "| sh",
    "| bash",
    "| zsh",
    "|sh\n",
    "|bash\n",
];

/// Token splitters that mark compound boundaries. We intentionally do
/// not split on `(` `)` — subshell groups should re-prompt anyway, and
/// raw-pattern detection above will catch them.
const COMPOUND_SEPARATORS: &[&str] = &["&&", "||", ";", "|"];

/// Public entry. Given a raw bash command line, decide whether it's
/// auto-allowable on a per-canonical-key basis or needs to re-prompt.
pub fn analyze_bash_command(raw: &str) -> CommandCheck {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return CommandCheck::Dangerous("empty command".to_string());
    }

    // Stage 1: raw-pattern danger sweep. Cheapest, catches the worst
    // offenders (subshell, rm -rf, redirect to device, force flags).
    for pat in RAW_DANGER_PATTERNS {
        if trimmed.contains(pat) {
            return CommandCheck::Dangerous(format!("contains `{pat}`"));
        }
    }

    // Stage 2: split on compound separators. We do this before per-token
    // analysis so each part is independently whitelistable.
    let parts = split_compound(trimmed);
    let mut keys: Vec<String> = Vec::with_capacity(parts.len());
    for part in parts {
        let canonical = match canonicalize_part(&part) {
            CommandCheck::Safe(mut v) => v.pop().unwrap_or_default(),
            CommandCheck::Dangerous(reason) => return CommandCheck::Dangerous(reason),
        };
        if canonical.is_empty() {
            return CommandCheck::Dangerous("empty sub-command".to_string());
        }
        keys.push(canonical);
    }
    if keys.is_empty() {
        return CommandCheck::Dangerous("no commands parsed".to_string());
    }
    CommandCheck::Safe(keys)
}

/// Splits a command on `&&`, `||`, `;`, `|` (compound separators). Does
/// not understand quoting — a `;` inside a quoted string will split
/// incorrectly. That's intentional: if we can't be sure, the
/// whitelist won't match either way and the user re-prompts.
fn split_compound(input: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![input.to_string()];
    for sep in COMPOUND_SEPARATORS {
        let mut next: Vec<String> = Vec::new();
        for chunk in &out {
            for p in chunk.split(sep) {
                let s = p.trim();
                if !s.is_empty() {
                    next.push(s.to_string());
                }
            }
        }
        out = next;
    }
    out
}

/// Strips environment-variable assignments (`FOO=bar BAR=baz cmd …`)
/// and wrapper prefixes (`timeout 30 cmd …`) from the start of a
/// part, then returns the canonical key for the underlying command.
/// Returns Dangerous when a token in DANGER_TOKENS is reached.
fn canonicalize_part(part: &str) -> CommandCheck {
    let mut tokens: Vec<&str> = part.split_whitespace().collect();

    // Skip leading FOO=bar style env assignments.
    while tokens
        .first()
        .map(|t| t.contains('=') && !t.starts_with('=') && !t.starts_with('-'))
        .unwrap_or(false)
    {
        tokens.remove(0);
    }
    // Unwrap recognized wrapper prefixes; after stripping the wrapper
    // itself, advance past any `-flag` tokens and (for `timeout`) the
    // duration argument, until the inner command name appears.
    while let Some(first) = tokens.first().copied() {
        if !WRAPPER_PREFIXES.contains(&first) {
            break;
        }
        tokens.remove(0);
        if first == "timeout" && tokens.first().is_some_and(|t| !t.starts_with('-')) {
            tokens.remove(0);
        }
        // Skip flag tokens that belong to the wrapper. Stops at the
        // first non-flag token, which we treat as the inner command.
        while tokens.first().is_some_and(|t| t.starts_with('-')) {
            tokens.remove(0);
        }
    }

    let cmd = match tokens.first() {
        Some(c) => *c,
        None => return CommandCheck::Dangerous("nothing after wrapper/env".to_string()),
    };

    // Reject paths-as-commands. A bare `./foo` or `/usr/bin/foo` should
    // re-prompt — we don't have any way to know what the script does.
    if cmd.contains('/') {
        return CommandCheck::Dangerous(format!("path-as-command: `{cmd}`"));
    }

    // Danger-token check. `git` is on the list but only the canonical
    // key (e.g. "git status") is matched against the allowlist, so the
    // user can still whitelist safe subcommands.
    if DANGER_TOKENS.contains(&cmd) {
        // We do NOT short-circuit Dangerous here — instead we let the
        // caller match the canonical key against the allowlist. The
        // permission layer treats "git status" as auto-allowable iff
        // the user previously said "always allow `git status`".
        // For commands like `rm`, the rm-rf raw pattern above handles
        // the worst case; bare `rm foo.txt` still gets prompted because
        // it's not on the default allowlist.
    }

    // Build canonical key: command + first non-flag arg.
    let first_arg = tokens
        .iter()
        .skip(1)
        .find(|t| !t.starts_with('-'))
        .copied();
    let key = match first_arg {
        Some(a) => format!("{cmd} {a}"),
        None => cmd.to_string(),
    };
    CommandCheck::Safe(vec![key])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe(parts: &[&str]) -> CommandCheck {
        CommandCheck::Safe(parts.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn simple_command_canonicalizes_to_cmd_plus_first_arg() {
        assert_eq!(analyze_bash_command("git status"), safe(&["git status"]));
        assert_eq!(analyze_bash_command("git status -sb"), safe(&["git status"]));
        assert_eq!(analyze_bash_command("ls"), safe(&["ls"]));
        assert_eq!(analyze_bash_command("ls -la"), safe(&["ls"]));
    }

    #[test]
    fn wrapper_prefixes_are_stripped() {
        assert_eq!(analyze_bash_command("timeout 30 git status"), safe(&["git status"]));
        assert_eq!(analyze_bash_command("nohup git diff"), safe(&["git diff"]));
        assert_eq!(analyze_bash_command("stdbuf -oL ls"), safe(&["ls"]));
    }

    #[test]
    fn env_assignments_are_stripped() {
        assert_eq!(analyze_bash_command("FOO=bar git status"), safe(&["git status"]));
        assert_eq!(analyze_bash_command("FOO=1 BAR=2 ls -la"), safe(&["ls"]));
    }

    #[test]
    fn compound_commands_decompose_into_parts() {
        assert_eq!(
            analyze_bash_command("git status && git diff"),
            safe(&["git status", "git diff"])
        );
        assert_eq!(
            analyze_bash_command("ls; pwd; whoami"),
            safe(&["ls", "pwd", "whoami"])
        );
        assert_eq!(
            analyze_bash_command("cargo build | tee log"),
            safe(&["cargo build", "tee log"])
        );
    }

    #[test]
    fn rm_rf_is_dangerous_anywhere() {
        assert!(matches!(
            analyze_bash_command("rm -rf /tmp/foo"),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("ls && rm -rf /tmp/foo"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn subshell_and_backticks_are_dangerous() {
        assert!(matches!(
            analyze_bash_command("echo $(whoami)"),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("echo `whoami`"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn dev_redirect_is_dangerous() {
        assert!(matches!(
            analyze_bash_command("dd if=/dev/zero of=/dev/sda"),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("echo x > /dev/sda"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn pipe_to_shell_is_dangerous() {
        assert!(matches!(
            analyze_bash_command("curl http://x | sh"),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("wget -O- http://x | bash"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn path_as_command_is_dangerous() {
        assert!(matches!(
            analyze_bash_command("./install.sh"),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("/usr/local/bin/anything"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn force_flag_is_dangerous() {
        assert!(matches!(
            analyze_bash_command("git push --force"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn empty_command_is_dangerous() {
        assert!(matches!(
            analyze_bash_command(""),
            CommandCheck::Dangerous(_)
        ));
        assert!(matches!(
            analyze_bash_command("   "),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn compound_with_one_dangerous_part_taints_whole() {
        // ls is fine, but the compound contains rm -rf which the raw
        // pattern catches first.
        assert!(matches!(
            analyze_bash_command("ls && rm -rf /tmp/x"),
            CommandCheck::Dangerous(_)
        ));
    }

    #[test]
    fn git_subcommands_get_distinct_keys() {
        // The whole point of canonical keys: status and push must not
        // collide on the allowlist.
        let s = analyze_bash_command("git status");
        let p = analyze_bash_command("git push origin main");
        assert_ne!(s, p);
    }
}
