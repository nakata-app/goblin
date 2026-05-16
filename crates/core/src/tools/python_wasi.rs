//! `python_wasi` tool — execute Python code inside the WASI sandbox.
//!
//! Wraps a pre-downloaded Python WASI interpreter (~30MB) in the
//! `wasi_sandbox` runtime, hands the model's code to it via `python -c`,
//! and captures stdout/stderr. The interpreter runs with no host
//! filesystem access, no network, no env vars — same hard caps as
//! `wasm_run` (fuel, memory, wall-clock).
//!
//! Why this exists: bash gives the model the user's full shell
//! authority; for code-execution tasks (data parsing, regex, math,
//! one-off scripts) that's wildly over-privileged. `python_wasi`
//! gives the model a real interpreter without the blast radius.
//!
//! Runtime path: `~/.aegis/wasm-runtimes/python-3.11.wasm` by default,
//! overridable with `METIS_PYTHON_WASI_PATH`. The runtime is NOT
//! shipped with Aegis — too large to bundle, and license/version
//! choice belongs to the user. If the runtime is missing, the tool
//! returns an error that names the exact download URL the user can
//! curl, so the failure is self-explanatory and one paste away from
//! fixing.
//!
//! Compiled only when the `wasm` feature is enabled.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};
use crate::wasi_sandbox::{execute_wasi_with_args, SandboxLimits};

/// Default location for the Python WASI interpreter binary.
const DEFAULT_RUNTIME_FILENAME: &str = "python-3.11.wasm";

/// Where the user can grab a working build from. Pre-built by VMware
/// Labs (Apache-2.0); this exact filename matches the asset name on
/// the release page.
const DOWNLOAD_HINT: &str = "https://github.com/vmware-labs/webassembly-language-runtimes/releases (look for python-3.11.*.wasm)";

pub struct PythonWasi;

#[derive(Debug, Deserialize)]
struct PyArgs {
    /// Python source code to evaluate. Handed to the interpreter via
    /// `python -c "<code>"`. Stdin is passed separately.
    code: String,
    /// Optional input written to the interpreter's WASI stdin (the
    /// script can read it via `sys.stdin.read()`).
    #[serde(default)]
    stdin: String,
    /// Optional wall-clock cap in ms. Default 10000, hard ceiling 60_000.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn runtime_path() -> PathBuf {
    if let Ok(p) = std::env::var("METIS_PYTHON_WASI_PATH") {
        return PathBuf::from(p);
    }
    if let Some(home) = dirs::home_dir() {
        return home
            .join(".metis")
            .join("wasm-runtimes")
            .join(DEFAULT_RUNTIME_FILENAME);
    }
    PathBuf::from(DEFAULT_RUNTIME_FILENAME)
}

#[async_trait]
impl Tool for PythonWasi {
    fn name(&self) -> &str {
        "python_wasi"
    }
    fn description(&self) -> &str {
        "Execute Python code inside a WASI sandbox. Pre-downloaded \
         Python 3.11 interpreter runs with NO host filesystem, NO \
         network, NO env vars, and hard fuel/memory/wall-clock caps. \
         Use this for safe code execution — data parsing, regex, math, \
         JSON munging — anything where bash's full shell authority is \
         too much. Returns captured stdout, stderr, and the fuel \
         consumed. Requires a Python WASI runtime to be installed at \
         ~/.metis/wasm-runtimes/python-3.11.wasm (overridable via \
         METIS_PYTHON_WASI_PATH); if missing, an error with the \
         download URL is returned."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source code (passed via `python -c`)."
                },
                "stdin": {
                    "type": "string",
                    "description": "Optional input written to sys.stdin."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Wall-clock cap in milliseconds. Default 10000, max 60000.",
                    "maximum": 60000
                }
            },
            "required": ["code"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: PyArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let path = runtime_path();
        if !path.is_file() {
            return Ok(format!(
                "[python_wasi error] interpreter not found at {}\n\
                 install with:\n  \
                 mkdir -p ~/.metis/wasm-runtimes && \\\n  \
                 curl -L -o ~/.metis/wasm-runtimes/python-3.11.wasm \\\n    \
                 <pick a release asset from {}>",
                path.display(),
                DOWNLOAD_HINT
            ));
        }
        let bytes = std::fs::read(&path).map_err(|source| ToolError::Io {
            path: path.display().to_string(),
            source,
        })?;

        // argv shape mirrors a real `python -c "<code>"` invocation:
        // argv[0] = program name, argv[1..] = flags + code.
        let argv = vec!["python".to_string(), "-c".to_string(), args.code.clone()];

        // Python WASI needs a high table-elements cap (interpreter
        // boot allocates 5400+ table entries) and more memory than the
        // default 64MB — a small numpy import alone is 100MB+. Use
        // 256MB + 256k table elements + a roomier fuel budget than
        // arbitrary-WASM users get.
        let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(10_000).min(60_000));
        let limits = SandboxLimits {
            memory_bytes: 256 * 1024 * 1024,
            fuel: 5_000_000_000,
            timeout,
            table_elements: 256 * 1024,
        };
        let stdin_bytes = args.stdin.into_bytes();

        let result = tokio::task::spawn_blocking(move || {
            execute_wasi_with_args(&bytes, &stdin_bytes, &argv, &limits)
        })
        .await
        .map_err(|e| ToolError::Spawn(format!("python_wasi join: {e}")))?;

        let out = match result {
            Ok(o) => o,
            Err(e) => return Ok(format!("[python_wasi error] {e:#}")),
        };

        Ok(format!(
            "[stdout]\n{}\n[stderr]\n{}\n[fuel] {}",
            out.stdout.trim_end(),
            out.stderr.trim_end(),
            out.fuel_consumed
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_in(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    /// We can't ship a 30MB interpreter as a fixture, so the integration
    /// path (real Python eval) lives behind a manual smoke test. What we
    /// CAN test cheaply: the missing-runtime path returns a useful
    /// error string that names both the expected location AND the
    /// download URL — the model gets enough info to surface the fix to
    /// the user.
    #[tokio::test]
    async fn missing_runtime_returns_actionable_error_with_download_url() {
        // Force the runtime path to a non-existent file via env.
        let dir = TempDir::new().unwrap();
        let nope = dir.path().join("does-not-exist.wasm");
        std::env::set_var("METIS_PYTHON_WASI_PATH", &nope);

        let ctx = ctx_in(dir.path());
        let out = PythonWasi
            .execute(json!({"code": "print('hi')"}), &ctx)
            .await
            .unwrap();

        std::env::remove_var("METIS_PYTHON_WASI_PATH");

        assert!(
            out.contains("interpreter not found"),
            "expected missing-runtime hint, got: {out}"
        );
        assert!(
            out.contains("does-not-exist.wasm"),
            "error must name the path it looked at: {out}"
        );
        assert!(
            out.contains("vmware-labs/webassembly-language-runtimes"),
            "error must include the download URL: {out}"
        );
        let _ = Arc::new(()); // suppress unused-import warning
    }

    #[tokio::test]
    async fn invalid_args_returns_invalid_args_error() {
        let dir = TempDir::new().unwrap();
        let ctx = ctx_in(dir.path());
        let err = PythonWasi
            .execute(json!({"not_code": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
    }
}
