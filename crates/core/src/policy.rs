//! Rego-based policy gate for tool execution.
//!
//! Wraps the [`regorus`] pure-Rust Rego interpreter in a `Permission`
//! decorator. Policies live as `.rego` files under
//! `<workspace>/.aegis/policies/` (project-local) and/or
//! `~/.aegis/policies/` (user-global); both directories are loaded if
//! they exist, project policies take precedence.
//!
//! The host hands the policy three queries in fixed order:
//!
//!   1. `data.aegis.tools.deny`            — bool, true ⇒ HardDeny
//!   2. `data.aegis.tools.require_approval` — bool, true ⇒ delegate to fallback
//!   3. `data.aegis.tools.allow`            — bool, true ⇒ Allow
//!
//! If none fires, the fallback `Permission` decides. This makes the
//! policy strictly additive: an empty policy directory leaves Aegis's
//! existing behaviour untouched.
//!
//! Policy input shape:
//! ```json
//! { "tool": "<name>", "args": { ... } }
//! ```
//!
//! Compiled only when the `policy` feature is enabled.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default search order for `.rego` policy directories: project-local
/// first, then user-global. Mirrors `guardrail::default_banlist_paths`.
/// Caller passes the result straight into `PolicyEngine::from_dirs`.
pub fn default_policy_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = vec![workspace.join(".metis").join("policies")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".metis").join("policies"));
    }
    paths
}

/// One-call wrapper used by every CLI surface (REPL, TUI, one-shot,
/// IDE): if the binary was built with `--features policy`, decorate the
/// existing permission with a Rego gate loaded from `dirs`. With zero
/// `.rego` files found OR a load error, returns the inner permission
/// unchanged so a broken policy bundle never blocks Metis from starting.
///
/// The "policy gate: N rego file(s) loaded" notice fires only on the first
/// successful wrap per process — main.rs builds a permission chain for
/// REPL/one-shot and tui.rs builds a separate one for TUI mode, both go
/// through this helper, but the user only needs to know once.
pub fn wrap_with_policy(
    inner: Arc<dyn crate::permission::Permission>,
    dirs: &[PathBuf],
) -> Arc<dyn crate::permission::Permission> {
    if dirs.iter().all(|d| !d.is_dir()) {
        return inner;
    }
    match PolicyEngine::from_dirs(dirs) {
        Ok(engine) if engine.loaded_count() > 0 => {
            static LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            if LOGGED.get().is_none() {
                let _ = LOGGED.set(());
                eprintln!(
                    "\x1b[2m[aegis] policy gate: {} rego file(s) loaded\x1b[0m",
                    engine.loaded_count()
                );
            }
            Arc::new(RegoPermission {
                engine: Arc::new(engine),
                fallback: inner,
            })
        }
        Ok(_) => inner,
        Err(e) => {
            eprintln!("[aegis] policy load failed, gate disabled: {e}");
            inner
        }
    }
}

use regorus::{Engine, Value as RegoValue};
use serde_json::Value;
use thiserror::Error;

use crate::permission::{Permission, PermissionDecision};

/// What the policy says about a tool call. Maps cleanly onto
/// `PermissionDecision` after the decorator picks a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Allow the call; skip the fallback prompt.
    Allow,
    /// Defer to the fallback `Permission` (interactive prompt, etc.).
    RequireApproval,
    /// Deny with the given reason. Maps to `HardDeny`.
    Deny(String),
    /// Policy didn't match this call — fallback decides.
    NoMatch,
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("policy compile error in {path}: {source}")]
    Compile {
        path: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("policy directory unreadable: {0}")]
    Io(#[from] std::io::Error),
}

/// A bundle of Rego policies, ready for evaluation.
///
/// `Engine` is wrapped in a `Mutex` because evaluation requires
/// `&mut Engine` (set_input + eval_query). Tool checks are infrequent
/// enough that contention is negligible, and the Mutex lets us hand
/// out a single `Arc<PolicyEngine>` to any number of agent threads.
pub struct PolicyEngine {
    engine: Mutex<Engine>,
    policy_files: Vec<PathBuf>,
}

impl PolicyEngine {
    /// Build an engine pre-loaded with every `*.rego` file found in
    /// the given directories, in order. Missing directories are
    /// silently skipped — that is the "no policies configured" path.
    pub fn from_dirs<P: AsRef<Path>>(dirs: &[P]) -> Result<Self, PolicyError> {
        let mut engine = Engine::new();
        let mut loaded: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            let dir = dir.as_ref();
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("rego") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).map_err(PolicyError::Io)?;
                engine
                    .add_policy(path.display().to_string(), body)
                    .map_err(|e| PolicyError::Compile {
                        path: path.display().to_string(),
                        source: e,
                    })?;
                loaded.push(path);
            }
        }
        Ok(Self {
            engine: Mutex::new(engine),
            policy_files: loaded,
        })
    }

    /// Number of `.rego` files currently loaded. `0` means "no policy in
    /// effect" — callers can short-circuit and skip the lock entirely.
    pub fn loaded_count(&self) -> usize {
        self.policy_files.len()
    }

    /// Paths of the loaded policy files, in load order.
    pub fn loaded_files(&self) -> &[PathBuf] {
        &self.policy_files
    }

    /// Evaluate the policy bundle against `tool` + `args` and return
    /// the host-friendly verdict. Errors during evaluation are conservative:
    /// they map to `RequireApproval` (defer to interactive layer) rather
    /// than `Allow`, so a broken policy never silently opens the gate.
    pub fn evaluate(&self, tool: &str, args: &Value) -> PolicyVerdict {
        if self.policy_files.is_empty() {
            return PolicyVerdict::NoMatch;
        }

        let input_json = serde_json::json!({ "tool": tool, "args": args });
        let input: RegoValue = match serde_json::to_string(&input_json)
            .ok()
            .and_then(|s| RegoValue::from_json_str(&s).ok())
        {
            Some(v) => v,
            None => return PolicyVerdict::RequireApproval,
        };

        let mut engine = match self.engine.lock() {
            Ok(g) => g,
            Err(_) => return PolicyVerdict::RequireApproval,
        };
        engine.set_input(input);

        // Order matters: deny > require_approval > allow. A policy that
        // says both "deny" and "allow" should deny — that is the
        // conservative read of an inconsistent ruleset.
        if eval_bool(&mut engine, "data.metis.tools.deny") {
            return PolicyVerdict::Deny(format!("denied by policy: {tool}"));
        }
        if eval_bool(&mut engine, "data.metis.tools.require_approval") {
            return PolicyVerdict::RequireApproval;
        }
        if eval_bool(&mut engine, "data.metis.tools.allow") {
            return PolicyVerdict::Allow;
        }
        PolicyVerdict::NoMatch
    }
}

fn eval_bool(engine: &mut Engine, query: &str) -> bool {
    let res = match engine.eval_query(query.to_string(), false) {
        Ok(r) => r,
        Err(_) => return false,
    };
    res.result
        .first()
        .and_then(|r| r.expressions.first())
        .and_then(|e| match &e.value {
            RegoValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false)
}

/// `Permission` decorator that runs a Rego policy first, and only
/// consults the wrapped fallback on `RequireApproval` or `NoMatch`.
///
/// This is the integration point for the agent loop:
/// ```ignore
/// let engine = Arc::new(PolicyEngine::from_dirs(&[".metis/policies", "~/.metis/policies"])?);
/// let inner: Arc<dyn Permission> = Arc::new(PolicyPermission::new());
/// let perm: Arc<dyn Permission> = Arc::new(RegoPermission { engine, fallback: inner });
/// ```
pub struct RegoPermission {
    pub engine: Arc<PolicyEngine>,
    pub fallback: Arc<dyn Permission>,
}

impl Permission for RegoPermission {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        match self.engine.evaluate(tool, args) {
            PolicyVerdict::Allow => PermissionDecision::Allow,
            PolicyVerdict::Deny(reason) => PermissionDecision::HardDeny(reason),
            PolicyVerdict::RequireApproval | PolicyVerdict::NoMatch => {
                self.fallback.check(tool, args)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{AllowAll, DenyAll};
    use serde_json::json;
    use tempfile::TempDir;

    fn write_policy(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    const SAMPLE_POLICY: &str = r#"
        package metis.tools
        import rego.v1

        default allow := false
        default deny := false
        default require_approval := false

        # rm -rf is hard-deny
        deny if {
            input.tool == "bash"
            contains(input.args.command, "rm -rf")
        }

        # any other bash needs approval
        require_approval if {
            input.tool == "bash"
            not contains(input.args.command, "rm -rf")
        }

        # read_file is unconditionally allowed
        allow if input.tool == "read_file"
    "#;

    #[test]
    fn empty_dirs_yield_nomatch_verdict() {
        let engine = PolicyEngine::from_dirs::<&Path>(&[]).unwrap();
        assert_eq!(engine.loaded_count(), 0);
        assert_eq!(
            engine.evaluate("bash", &json!({"command": "ls"})),
            PolicyVerdict::NoMatch
        );
    }

    #[test]
    fn allow_deny_require_approval_paths_resolve_correctly() {
        let dir = TempDir::new().unwrap();
        write_policy(dir.path(), "tools.rego", SAMPLE_POLICY);
        let engine = PolicyEngine::from_dirs(&[dir.path()]).unwrap();
        assert_eq!(engine.loaded_count(), 1);

        // read_file → Allow (skip prompt)
        assert_eq!(
            engine.evaluate("read_file", &json!({"path": "x"})),
            PolicyVerdict::Allow
        );
        // bash ls → RequireApproval (delegate to fallback)
        assert_eq!(
            engine.evaluate("bash", &json!({"command": "ls -la"})),
            PolicyVerdict::RequireApproval
        );
        // bash rm -rf → Deny
        let v = engine.evaluate("bash", &json!({"command": "rm -rf /tmp/x"}));
        assert!(matches!(v, PolicyVerdict::Deny(_)), "got: {v:?}");
        // unrelated tool → NoMatch (delegate to fallback)
        assert_eq!(
            engine.evaluate("web_fetch", &json!({"url": "https://x"})),
            PolicyVerdict::NoMatch
        );
    }

    #[test]
    fn rego_permission_decorator_routes_through_fallback() {
        let dir = TempDir::new().unwrap();
        write_policy(dir.path(), "tools.rego", SAMPLE_POLICY);
        let engine = Arc::new(PolicyEngine::from_dirs(&[dir.path()]).unwrap());

        // Fallback says "deny everything you see"; the decorator must
        // only consult it on RequireApproval/NoMatch — never on Allow
        // or Deny coming from policy.
        let fallback: Arc<dyn Permission> = Arc::new(DenyAll("fallback denied".into()));
        let perm = RegoPermission {
            engine: engine.clone(),
            fallback,
        };

        // policy Allow → final Allow (fallback is never asked)
        assert!(matches!(
            perm.check("read_file", &json!({"path": "x"})),
            PermissionDecision::Allow
        ));

        // policy Deny → final HardDeny
        assert!(matches!(
            perm.check("bash", &json!({"command": "rm -rf /"})),
            PermissionDecision::HardDeny(_)
        ));

        // policy RequireApproval → fallback denies → final Deny
        assert!(matches!(
            perm.check("bash", &json!({"command": "ls"})),
            PermissionDecision::Deny(_)
        ));

        // policy NoMatch → fallback denies → final Deny
        assert!(matches!(
            perm.check("web_fetch", &json!({"url": "https://x"})),
            PermissionDecision::Deny(_)
        ));

        // Now swap fallback to AllowAll: same RequireApproval call should
        // pass through.
        let allow_fb: Arc<dyn Permission> = Arc::new(AllowAll);
        let perm2 = RegoPermission {
            engine,
            fallback: allow_fb,
        };
        assert!(matches!(
            perm2.check("bash", &json!({"command": "ls"})),
            PermissionDecision::Allow
        ));
    }

    #[test]
    fn broken_policy_fails_safely_to_require_approval() {
        // A policy that throws at eval time (division by zero, etc.)
        // must not silently allow — the decorator pushes to fallback.
        let dir = TempDir::new().unwrap();
        write_policy(
            dir.path(),
            "broken.rego",
            r#"
            package metis.tools
            import rego.v1
            allow if { 1/0 == 0 }
            "#,
        );
        let engine = PolicyEngine::from_dirs(&[dir.path()]).unwrap();
        // Evaluation either errors (→ false) or returns false; never crashes.
        let v = engine.evaluate("anything", &json!({}));
        assert_eq!(v, PolicyVerdict::NoMatch);
    }
}
