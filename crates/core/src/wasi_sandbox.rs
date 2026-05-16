//! WASI sandbox for safe in-process WASM execution.
//!
//! Wraps `wasmtime` with sane defaults (fuel limit, memory cap, epoch
//! interruption for timeouts, no inherited stdio/env). Designed for
//! running short-lived WASM modules — language interpreters (Python WASI)
//! or pure-function compute kernels — with hard resource caps the host
//! can enforce.
//!
//! Compiled only when the `wasm` feature is enabled; pulls in ~50MB of
//! cranelift JIT and adds ~5-10 minutes to a cold build.

use std::time::Duration;

use anyhow::{Context, Result};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimitsBuilder};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::WasiCtxBuilder;

/// Resource limits applied to every sandbox execution.
#[derive(Debug, Clone)]
pub struct SandboxLimits {
    /// Linear memory cap in bytes. Default 64 MiB.
    pub memory_bytes: usize,
    /// CPU fuel cap. ~1 unit per wasm instruction. Default 1B (~seconds of compute).
    pub fuel: u64,
    /// Wall-clock timeout enforced via epoch interruption. Default 5s.
    pub timeout: Duration,
    /// Max table elements (Python WASI needs ≥10k). Default 64k.
    pub table_elements: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            fuel: 1_000_000_000,
            timeout: Duration::from_secs(5),
            table_elements: 64 * 1024,
        }
    }
}

/// Captured execution result.
#[derive(Debug, Clone)]
pub struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub fuel_consumed: u64,
}

/// Build a `wasmtime::Engine` configured for sandboxed execution: fuel
/// metering on, epoch interruption on (so we can enforce timeouts from
/// the host), async support off (we run sync inside `spawn_blocking`).
fn make_engine() -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    cfg.epoch_interruption(true);
    Engine::new(&cfg).context("wasmtime engine init")
}

/// Execute a WASI module and capture its stdout/stderr. Convenience
/// wrapper around `execute_wasi_with_args` for callers that have no
/// argv to pass — same defaults, no behavioural change.
pub fn execute_wasi(
    wasm_bytes: &[u8],
    stdin: &[u8],
    limits: &SandboxLimits,
) -> Result<SandboxOutput> {
    execute_wasi_with_args(wasm_bytes, stdin, &[], limits)
}

/// Execute a WASI module with a custom argv and capture its stdout/stderr.
///
/// `wasm_bytes` may be a `.wasm` binary or `.wat` text — `wasmtime`
/// accepts both. `stdin` is passed to the module via WASI stdin pipe.
/// `argv` becomes the module's argv at startup (WASI `args_get` /
/// `args_sizes_get`). Most language interpreters need this — Python
/// for `-c "code"`, Node for the script path, Ruby for `-e`. argv\[0\]
/// is the program name by convention; pass it as the first element.
///
/// Synchronous on purpose: callers should wrap in `tokio::task::spawn_blocking`
/// when invoking from async contexts.
pub fn execute_wasi_with_args(
    wasm_bytes: &[u8],
    stdin: &[u8],
    argv: &[String],
    limits: &SandboxLimits,
) -> Result<SandboxOutput> {
    let engine = make_engine()?;
    let module = Module::new(&engine, wasm_bytes).context("compile wasm module")?;

    // Spawn an epoch ticker thread that bumps the engine's epoch every
    // 100ms. The store is configured to trap once `timeout / 100ms` ticks
    // have elapsed — that's how we enforce a wall-clock cap on a synchronous
    // wasm call without preemption support.
    let ticks_until_trap = (limits.timeout.as_millis() / 100).max(1) as u64;
    let engine_for_ticker = engine.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let ticker = std::thread::spawn(move || {
        while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            engine_for_ticker.increment_epoch();
        }
    });

    // Build WASI context: in-memory pipes for stdio, no env, no preopened dirs.
    let stdin_pipe = MemoryInputPipe::new(stdin.to_vec());
    let stdout_pipe = MemoryOutputPipe::new(1024 * 1024);
    let stderr_pipe = MemoryOutputPipe::new(1024 * 1024);
    let stdout_reader = stdout_pipe.clone();
    let stderr_reader = stderr_pipe.clone();

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .stdin(stdin_pipe)
        .stdout(stdout_pipe)
        .stderr(stderr_pipe);
    if !argv.is_empty() {
        wasi_builder.args(argv);
    }
    let wasi_ctx: WasiP1Ctx = wasi_builder.build_p1();

    struct HostState {
        wasi: WasiP1Ctx,
        limits: wasmtime::StoreLimits,
    }

    let store_limits = StoreLimitsBuilder::new()
        .memory_size(limits.memory_bytes)
        .table_elements(limits.table_elements)
        .instances(8)
        .memories(4)
        .tables(8)
        .trap_on_grow_failure(false)
        .build();

    let mut store = Store::new(
        &engine,
        HostState {
            wasi: wasi_ctx,
            limits: store_limits,
        },
    );
    store.limiter(|s: &mut HostState| &mut s.limits);
    store.set_fuel(limits.fuel).context("set fuel")?;
    store.set_epoch_deadline(ticks_until_trap);

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s: &mut HostState| &mut s.wasi)
        .context("link wasi preview1")?;

    let result = (|| -> Result<u64> {
        let instance = linker
            .instantiate(&mut store, &module)
            .context("instantiate module")?;
        // WASI command modules export `_start`. If the module is a pure
        // library without an entrypoint, instantiation alone is enough.
        if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            start.call(&mut store, ()).context("call _start")?;
        }
        Ok(limits.fuel.saturating_sub(store.get_fuel().unwrap_or(0)))
    })();

    // Stop the ticker before propagating any execution error.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ticker.join();

    let fuel_consumed = result?;

    // Drop the store so the pipe writers go out of scope and the readers
    // can see the fully-flushed contents.
    drop(store);

    let stdout = String::from_utf8_lossy(&stdout_reader.contents()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.contents()).into_owned();

    Ok(SandboxOutput {
        stdout,
        stderr,
        fuel_consumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WASI module: writes "hello sandbox\n" to stdout via fd_write.
    /// Built from the .wat below at test time so we don't need a .wasm fixture.
    fn hello_wasi_wat() -> &'static str {
        r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (memory 1)
          (export "memory" (memory 0))
          (data (i32.const 0) "hello sandbox\n")
          ;; iovec at offset 16: ptr=0, len=14
          (data (i32.const 16) "\00\00\00\00\0e\00\00\00")
          (func (export "_start")
            (drop (call $fd_write
              (i32.const 1)   ;; fd=stdout
              (i32.const 16)  ;; iovec ptr
              (i32.const 1)   ;; iovec count
              (i32.const 32)  ;; nwritten ptr
            ))
          )
        )
        "#
    }

    #[test]
    fn hello_world_wasi_runs_and_captures_stdout() {
        let wat = hello_wasi_wat();
        let wasm = wat::parse_str(wat).expect("valid wat");
        let out = execute_wasi(&wasm, b"", &SandboxLimits::default()).expect("execute ok");
        assert_eq!(out.stdout, "hello sandbox\n", "stdout: {:?}", out);
        assert!(out.stderr.is_empty(), "unexpected stderr: {:?}", out.stderr);
        assert!(out.fuel_consumed > 0, "fuel was not metered");
    }

    #[test]
    fn fuel_exhaustion_traps_runaway_loop() {
        // Tight infinite loop. With low fuel cap, must trap before timeout.
        let wat = r#"
            (module
              (func (export "_start") (loop br 0))
            )
        "#;
        let wasm = wat::parse_str(wat).expect("valid wat");
        let limits = SandboxLimits {
            fuel: 10_000,
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let err = execute_wasi(&wasm, b"", &limits).expect_err("must trap");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fuel") || msg.to_lowercase().contains("trap"),
            "expected fuel/trap error, got: {msg}"
        );
    }
}
