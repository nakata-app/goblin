//! `wasm_run` tool — execute a WebAssembly module inside the WASI sandbox.
//!
//! Lets the model run untrusted code without giving it shell-level
//! authority: no network, no host filesystem, no inherited env vars,
//! hard fuel + memory + wall-clock caps. Inputs come from a path under
//! the workspace; outputs are stdout/stderr captured to in-memory pipes.
//!
//! Compiled only when the `wasm` feature is enabled.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};
use crate::wasi_sandbox::{execute_wasi, SandboxLimits};

pub struct WasmRun;

#[derive(Debug, Deserialize)]
struct WasmArgs {
    /// Path to the `.wasm` (or `.wat`) file, relative to the workspace root.
    path: String,
    /// Optional stdin handed to the module via the WASI stdin pipe.
    #[serde(default)]
    stdin: String,
    /// Optional wall-clock cap in ms. Default 5000, hard ceiling 60_000.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for WasmRun {
    fn name(&self) -> &str {
        "wasm_run"
    }
    fn description(&self) -> &str {
        "Execute a WebAssembly module inside a WASI sandbox. The module \
         runs with no network access, no host filesystem, no inherited \
         env vars, and hard fuel/memory/wall-clock caps. Returns captured \
         stdout, stderr, and the fuel consumed. Use this to run untrusted \
         compute, language interpreters compiled to wasm (Python WASI, \
         JS/QuickJS), or pure-function kernels — anything that should not \
         be allowed to touch the host system."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a .wasm or .wat file, relative to workspace root."
                },
                "stdin": {
                    "type": "string",
                    "description": "Optional input written to the module's WASI stdin."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Wall-clock cap in milliseconds. Default 5000, max 60000.",
                    "maximum": 60000
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: WasmArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        // Canonicalize path inside workspace root — same escape check the
        // file tools use, so the model can't point at /etc/passwd.wasm.
        let root = ctx.effective_root();
        let joined = root.join(&args.path);
        let canonical = joined.canonicalize().map_err(|source| ToolError::Io {
            path: joined.display().to_string(),
            source,
        })?;
        let canonical_root = root.canonicalize().unwrap_or(root.clone());
        if !canonical.starts_with(&canonical_root) {
            return Err(ToolError::PathEscape(args.path.clone()));
        }

        let bytes = std::fs::read(&canonical).map_err(|source| ToolError::Io {
            path: canonical.display().to_string(),
            source,
        })?;

        let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(5000).min(60_000));
        let limits = SandboxLimits {
            timeout,
            ..Default::default()
        };
        let stdin_bytes = args.stdin.into_bytes();

        // wasmtime API is sync; isolate it from the tokio runtime so a
        // long-running module doesn't park an async worker.
        let result =
            tokio::task::spawn_blocking(move || execute_wasi(&bytes, &stdin_bytes, &limits))
                .await
                .map_err(|e| ToolError::Spawn(format!("wasm join: {e}")))?;

        let out = match result {
            Ok(o) => o,
            Err(e) => {
                // wasmtime errors (trap, fuel exhausted, link failure) come
                // back as the model's signal that something went wrong with
                // the module — not a tool spawn failure. Surface as text
                // so the model can adjust and retry.
                return Ok(format!("[wasm error] {e:#}"));
            }
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

    #[tokio::test]
    async fn runs_hello_world_wasi_from_workspace() {
        let dir = TempDir::new().unwrap();
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory 1)
              (export "memory" (memory 0))
              (data (i32.const 0) "hello tool\n")
              (data (i32.const 16) "\00\00\00\00\0b\00\00\00")
              (func (export "_start")
                (drop (call $fd_write (i32.const 1) (i32.const 16) (i32.const 1) (i32.const 32))))
            )
        "#;
        let wasm = wat::parse_str(wat).unwrap();
        let path = dir.path().join("hello.wasm");
        std::fs::write(&path, &wasm).unwrap();

        let ctx = ctx_in(dir.path());
        let out = WasmRun
            .execute(json!({"path": "hello.wasm"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("hello tool"), "got: {out}");
        assert!(out.contains("[fuel]"), "expected fuel line: {out}");
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let dir = TempDir::new().unwrap();
        let ctx = ctx_in(dir.path());
        let err = WasmRun
            .execute(json!({"path": "../../etc/passwd"}), &ctx)
            .await
            .unwrap_err();
        // canonicalize fails (file may not exist) OR path-escape fires —
        // either is acceptable; the contract is "model can't escape root".
        let msg = err.to_string();
        assert!(
            msg.contains("escape") || msg.contains("io error") || msg.contains("No such"),
            "expected escape/io error, got: {msg}"
        );
        let _ = Arc::new(()); // suppress unused-import warning when only one test runs
    }
}
