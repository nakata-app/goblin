//! WebAssembly plugin sandbox.
//!
//! Plugins are pure-compute Wasm modules with no host imports — they can
//! transform input bytes into output bytes and nothing else (no fs, no
//! network, no syscalls). This is intentionally stricter than ClawHub:
//! the host surface is tiny and adversarially auditable, and a malicious
//! plugin cannot reach beyond its own linear memory.
//!
//! ABI (a plugin must export, at minimum):
//!   - `memory`                         the linear memory
//!   - `alloc(size: i32) -> i32`        return a freshly allocated pointer
//!   - `dealloc(ptr: i32, size: i32)`   free a previously alloc'd region
//!   - `plugin_run(in_ptr: i32, in_len: i32) -> i64`
//!         takes a UTF-8 input slice owned by the host (the host alloc'd
//!         it via the plugin's `alloc`), returns a packed pointer/length
//!         pair where the high 32 bits are the output ptr and the low 32
//!         bits are the output length. The host frees that region after
//!         reading it.

use std::path::Path;
use std::sync::Arc;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

pub struct PluginHost {
    engine: Engine,
}

#[derive(Debug)]
pub struct LoadedPlugin {
    pub name: String,
    module: Module,
}

impl PluginHost {
    pub fn new() -> Result<Self, String> {
        let mut cfg = wasmtime::Config::new();
        // Fuel-based execution metering so a misbehaving plugin cannot
        // burn CPU forever. The exact budget is consumed per call below.
        cfg.consume_fuel(true);
        // Wasm proposals we do not need are off-by-default in wasmtime; we
        // do not opt into threads or other expansion proposals. Module
        // import resolution happens with an empty import list further down,
        // so any plugin that does request host functions fails to
        // instantiate.
        let engine = Engine::new(&cfg).map_err(|e| format!("Wasm engine init failed: {}", e))?;
        Ok(Self { engine })
    }

    #[allow(dead_code)]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Load a plugin from raw bytes. `name` is the public handle the agent
    /// will refer to. `wasm_bytes` may be either a .wasm binary or its .wat
    /// textual representation — wasmtime decides which.
    pub fn load_from_bytes(&self, name: String, wasm_bytes: &[u8]) -> Result<Arc<LoadedPlugin>, String> {
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| format!("Wasm compile failed: {}", e))?;

        // A plugin is only loadable if it actually conforms to the ABI we
        // promised. We do not let unknown imports through — `imports()`
        // listing anything we don't supply is a hard error.
        if module.imports().count() != 0 {
            let names: Vec<String> = module.imports()
                .map(|i| format!("{}::{}", i.module(), i.name()))
                .collect();
            return Err(format!(
                "Plugin '{}' requests host imports which are not allowed in the sandbox: {}",
                name, names.join(", ")
            ));
        }

        // Verify the required exports exist so failures surface at load
        // time rather than on first call.
        let required = ["memory", "alloc", "dealloc", "plugin_run"];
        for r in &required {
            if module.get_export(r).is_none() {
                return Err(format!("Plugin '{}' missing required export: {}", name, r));
            }
        }

        Ok(Arc::new(LoadedPlugin { name, module }))
    }

    pub fn load_from_path<P: AsRef<Path>>(&self, path: P) -> Result<Arc<LoadedPlugin>, String> {
        let p = path.as_ref();
        let bytes = std::fs::read(p)
            .map_err(|e| format!("Failed to read plugin {:?}: {}", p, e))?;
        let name = p.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        self.load_from_bytes(name, &bytes)
    }

    /// Run a plugin against an input string and collect the output. Each
    /// call gets a fresh `Store` (so plugin state never leaks between
    /// invocations) and a fresh fuel budget.
    pub fn run(&self, plugin: &LoadedPlugin, input: &str) -> Result<String, String> {
        let mut store: Store<()> = Store::new(&self.engine, ());
        // Generous but bounded: one billion fuel units is enough for
        // realistic transformations but caps pathological inputs.
        store.set_fuel(1_000_000_000)
            .map_err(|e| format!("Fuel init failed: {}", e))?;

        let instance = Instance::new(&mut store, &plugin.module, &[])
            .map_err(|e| format!("Instantiate failed: {}", e))?;

        let memory: Memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("Plugin missing 'memory' export")?;
        let alloc: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, "alloc")
            .map_err(|e| format!("alloc signature mismatch: {}", e))?;
        let dealloc: TypedFunc<(i32, i32), ()> = instance
            .get_typed_func(&mut store, "dealloc")
            .map_err(|e| format!("dealloc signature mismatch: {}", e))?;
        let plugin_run: TypedFunc<(i32, i32), i64> = instance
            .get_typed_func(&mut store, "plugin_run")
            .map_err(|e| format!("plugin_run signature mismatch: {}", e))?;

        let input_bytes = input.as_bytes();
        let in_len = input_bytes.len() as i32;
        let in_ptr = alloc.call(&mut store, in_len)
            .map_err(|e| format!("alloc input failed: {}", e))?;
        memory.write(&mut store, in_ptr as usize, input_bytes)
            .map_err(|e| format!("write input failed: {}", e))?;

        let packed = plugin_run.call(&mut store, (in_ptr, in_len))
            .map_err(|e| format!("plugin_run failed: {}", e))?;
        // Plugin owns the input region until plugin_run returns; we free it
        // afterwards so a misbehaving plugin can't shift the contract.
        dealloc.call(&mut store, (in_ptr, in_len)).ok();

        let out_ptr = (packed >> 32) as i32;
        let out_len = (packed & 0xffff_ffff) as i32;
        if out_len < 0 || out_ptr < 0 {
            return Err(format!("Plugin returned invalid result (ptr={}, len={})", out_ptr, out_len));
        }
        // Defensively bound the output size so a buggy plugin can't make
        // us allocate gigabytes when reading it back.
        const MAX_OUTPUT: i32 = 16 * 1024 * 1024;
        if out_len > MAX_OUTPUT {
            return Err(format!("Plugin output too large ({} bytes, max {})", out_len, MAX_OUTPUT));
        }

        let mut buf = vec![0u8; out_len as usize];
        memory.read(&store, out_ptr as usize, &mut buf)
            .map_err(|e| format!("read output failed: {}", e))?;
        dealloc.call(&mut store, (out_ptr, out_len)).ok();

        String::from_utf8(buf).map_err(|e| format!("Plugin output not valid UTF-8: {}", e))
    }
}

/// Registry of all plugins loaded for this Goblin process. Owned by
/// `AppState` so Tauri commands and agent tool dispatch can both reach
/// it. Thread-safe behind a single Mutex; calls are short, contention
/// is not a concern for the plugin counts we expect (<100).
pub struct PluginRegistry {
    host: PluginHost,
    plugins: std::sync::Mutex<std::collections::HashMap<String, Arc<LoadedPlugin>>>,
}

impl PluginRegistry {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            host: PluginHost::new()?,
            plugins: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Discover and load every `*.wasm` file in `dir`. Returns the names of
    /// the plugins that loaded successfully; load failures are logged to
    /// stderr but do not abort the scan, so one broken plugin can never
    /// disable the whole subsystem.
    pub fn load_dir<P: AsRef<Path>>(&self, dir: P) -> Vec<String> {
        let dir = dir.as_ref();
        let mut loaded = Vec::new();
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return loaded, // missing dir is a normal cold-start state
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                continue;
            }
            match self.host.load_from_path(&path) {
                Ok(plugin) => {
                    let name = plugin.name.clone();
                    self.plugins.lock().unwrap().insert(name.clone(), plugin);
                    loaded.push(name);
                }
                Err(e) => {
                    eprintln!("[plugin] failed to load {:?}: {}", path, e);
                }
            }
        }
        loaded
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.plugins.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn run(&self, name: &str, input: &str) -> Result<String, String> {
        let plugin = self.plugins.lock().unwrap().get(name).cloned();
        let plugin = plugin.ok_or_else(|| format!("Plugin not found: {}", name))?;
        self.host.run(&plugin, input)
    }

    /// Load a plugin from raw bytes (used by Tauri install command and
    /// by tests). The provided `name` becomes the public handle.
    pub fn load_bytes(&self, name: String, bytes: &[u8]) -> Result<(), String> {
        let plugin = self.host.load_from_bytes(name.clone(), bytes)?;
        self.plugins.lock().unwrap().insert(name, plugin);
        Ok(())
    }

    pub fn unload(&self, name: &str) -> bool {
        self.plugins.lock().unwrap().remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal plugin written in WAT that uppercases ASCII input.
    /// Allocator is a bump allocator anchored at $heap_base; tests reset by
    /// using a fresh Store per call (the host does this already).
    const UPPERCASE_PLUGIN_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (global $heap_base (mut i32) (i32.const 1024))

          (func (export "alloc") (param $size i32) (result i32)
            (local $ptr i32)
            (local.set $ptr (global.get $heap_base))
            (global.set $heap_base
              (i32.add (global.get $heap_base) (local.get $size)))
            (local.get $ptr))

          (func (export "dealloc") (param $ptr i32) (param $size i32)
            ;; bump allocator: no-op. real impls would track free list.
            (nop))

          (func (export "plugin_run") (param $in_ptr i32) (param $in_len i32) (result i64)
            (local $i i32)
            (local $c i32)
            (local $out_ptr i32)
            ;; allocate output region equal in size to input
            (local.set $out_ptr (call $alloc_inline (local.get $in_len)))
            (local.set $i (i32.const 0))
            (block $done (loop $loop
              (br_if $done (i32.ge_s (local.get $i) (local.get $in_len)))
              (local.set $c (i32.load8_u (i32.add (local.get $in_ptr) (local.get $i))))
              (if (i32.and
                    (i32.ge_u (local.get $c) (i32.const 97))
                    (i32.le_u (local.get $c) (i32.const 122)))
                (then (local.set $c (i32.sub (local.get $c) (i32.const 32)))))
              (i32.store8 (i32.add (local.get $out_ptr) (local.get $i)) (local.get $c))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br $loop)))
            ;; pack (out_ptr, out_len) into i64
            (i64.or
              (i64.shl (i64.extend_i32_u (local.get $out_ptr)) (i64.const 32))
              (i64.extend_i32_u (local.get $in_len))))

          (func $alloc_inline (param $size i32) (result i32)
            (local $ptr i32)
            (local.set $ptr (global.get $heap_base))
            (global.set $heap_base
              (i32.add (global.get $heap_base) (local.get $size)))
            (local.get $ptr)))
    "#;

    /// Plugin that imports something forbidden — must be rejected at load.
    const FORBIDDEN_IMPORT_WAT: &str = r#"
        (module
          (import "env" "exfiltrate" (func $exfil (param i32)))
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "dealloc") (param i32) (param i32))
          (func (export "plugin_run") (param i32) (param i32) (result i64) (i64.const 0)))
    "#;

    /// Plugin missing a required export — must be rejected at load.
    const MISSING_EXPORT_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "plugin_run") (param i32) (param i32) (result i64) (i64.const 0)))
    "#;

    /// Plugin that loops forever — fuel limiting must abort it.
    const INFINITE_LOOP_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "dealloc") (param i32) (param i32))
          (func (export "plugin_run") (param i32) (param i32) (result i64)
            (block $done (loop $loop (br $loop)))
            (i64.const 0)))
    "#;

    fn host() -> PluginHost {
        PluginHost::new().expect("init")
    }

    #[test]
    fn loads_and_runs_uppercase_plugin() {
        let host = host();
        let plugin = host.load_from_bytes("uppercase".to_string(), UPPERCASE_PLUGIN_WAT.as_bytes()).unwrap();
        let out = host.run(&plugin, "hello, goblin").unwrap();
        assert_eq!(out, "HELLO, GOBLIN");
    }

    #[test]
    fn rejects_plugin_with_host_imports() {
        let host = host();
        let err = host.load_from_bytes("evil".to_string(), FORBIDDEN_IMPORT_WAT.as_bytes()).unwrap_err();
        assert!(err.contains("env::exfiltrate"), "expected import name in error, got: {}", err);
    }

    #[test]
    fn rejects_plugin_missing_required_export() {
        let host = host();
        let err = host.load_from_bytes("incomplete".to_string(), MISSING_EXPORT_WAT.as_bytes()).unwrap_err();
        assert!(err.contains("dealloc"), "expected missing export in error, got: {}", err);
    }

    #[test]
    fn infinite_loop_is_killed_by_fuel_limit() {
        let host = host();
        let plugin = host.load_from_bytes("loop".to_string(), INFINITE_LOOP_WAT.as_bytes()).unwrap();
        let err = host.run(&plugin, "anything").unwrap_err();
        // Wasmtime surfaces fuel exhaustion as a trap during execution.
        // The exact message wording varies, but the point is the plugin
        // call must terminate with an error instead of hanging.
        assert!(err.starts_with("plugin_run failed"), "expected plugin_run trap, got: {}", err);
    }

    #[test]
    fn empty_input_returns_empty_output() {
        let host = host();
        let plugin = host.load_from_bytes("up".to_string(), UPPERCASE_PLUGIN_WAT.as_bytes()).unwrap();
        let out = host.run(&plugin, "").unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn fresh_store_per_call_prevents_state_leak() {
        // The bump allocator inside the plugin grows monotonically; if state
        // leaked between calls a second invocation would eventually OOM.
        let host = host();
        let plugin = host.load_from_bytes("up".to_string(), UPPERCASE_PLUGIN_WAT.as_bytes()).unwrap();
        for _ in 0..50 {
            let out = host.run(&plugin, "abc").unwrap();
            assert_eq!(out, "ABC");
        }
    }

    #[test]
    fn registry_load_and_run() {
        let reg = PluginRegistry::new().unwrap();
        reg.load_bytes("up".to_string(), UPPERCASE_PLUGIN_WAT.as_bytes()).unwrap();
        assert_eq!(reg.list(), vec!["up".to_string()]);
        assert_eq!(reg.run("up", "abc").unwrap(), "ABC");
    }

    #[test]
    fn registry_run_unknown_plugin_errors() {
        let reg = PluginRegistry::new().unwrap();
        let err = reg.run("ghost", "x").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn registry_unload_removes_plugin() {
        let reg = PluginRegistry::new().unwrap();
        reg.load_bytes("tmp".to_string(), UPPERCASE_PLUGIN_WAT.as_bytes()).unwrap();
        assert!(reg.unload("tmp"));
        assert!(reg.list().is_empty());
        assert!(!reg.unload("tmp"));
    }

    #[test]
    fn registry_missing_dir_returns_empty() {
        let reg = PluginRegistry::new().unwrap();
        let loaded = reg.load_dir("/nonexistent/path/goblin-plugins-xyz");
        assert!(loaded.is_empty());
        assert!(reg.list().is_empty());
    }
}
