//! Tool execution wrapper that intercepts large outputs and stores
//! them in [`crate::blob_store::BlobStore`] in place of inlining the
//! raw payload back into the agent context.
//!
//! Wrap any `Box<dyn Tool>` with [`SandboxedTool::wrap`]. The wrapped
//! tool keeps the same name / description / schema — the agent loop
//! and the API are unaware of the sandbox. When the inner tool returns
//! a string longer than [`SandboxConfig::threshold_bytes`], the
//! wrapper:
//!
//! 1. stores the full payload in [`BlobStore`] (BLAKE3-content-addressed),
//! 2. indexes it in [`crate::blob_index::BlobIndex`] for `aegis ctx
//!    search`, and
//! 3. returns a short summary containing the `ctx://<hex>` reference
//!    plus a preview of the first N bytes, so the model can decide
//!    whether to fetch the full content via `aegis ctx show <id>`.
//!
//! Errors from the blob layer are non-fatal — if storage or indexing
//! fails, the original output is returned unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::blob_index::BlobIndex;
use crate::blob_store::{BlobMeta, BlobStore};
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput};

/// Default cutoff above which outputs are stashed (≈ 4 KB).
pub const DEFAULT_THRESHOLD_BYTES: usize = 4 * 1024;
/// Default preview window included alongside the ctx:// reference.
pub const DEFAULT_PREVIEW_BYTES: usize = 800;

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub threshold_bytes: usize,
    pub preview_bytes: usize,
    /// Tool names that always bypass the sandbox, even when output is
    /// large. Use for tools whose output the agent must see verbatim
    /// (diff previews, machine-parseable status, etc.).
    pub bypass_tools: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_bytes: DEFAULT_THRESHOLD_BYTES,
            preview_bytes: DEFAULT_PREVIEW_BYTES,
            bypass_tools: vec![
                "edit_file".into(),
                "write_file".into(),
                "multi_edit".into(),
                "ask_user_question".into(),
                "create_task".into(),
                "update_task".into(),
                "list_tasks".into(),
                "enter_plan_mode".into(),
                "exit_plan_mode".into(),
            ],
        }
    }
}

/// Wraps an inner [`Tool`] and stashes large outputs.
pub struct SandboxedTool {
    inner: Arc<dyn Tool>,
    store: Arc<BlobStore>,
    index: Arc<BlobIndex>,
    config: SandboxConfig,
}

impl SandboxedTool {
    pub fn wrap(
        inner: Arc<dyn Tool>,
        store: Arc<BlobStore>,
        index: Arc<BlobIndex>,
        config: SandboxConfig,
    ) -> Box<dyn Tool> {
        Box::new(Self {
            inner,
            store,
            index,
            config,
        })
    }

    fn should_stash(&self, len: usize) -> bool {
        if !self.config.enabled {
            return false;
        }
        if len < self.config.threshold_bytes {
            return false;
        }
        if self
            .config
            .bypass_tools
            .iter()
            .any(|t| t == self.inner.name())
        {
            return false;
        }
        true
    }

    fn stash(&self, raw: String, args: &Value) -> String {
        let tool_name = self.inner.name().to_string();
        let mut meta = BlobMeta::new(&tool_name);
        if let Some(src) = source_from_args(&tool_name, args) {
            meta = meta.with_source(src);
        }

        let id = match self.store.store(raw.as_bytes(), meta.clone()) {
            Ok(id) => id,
            Err(_) => return raw, // never block the tool on cache failure
        };

        // Best-effort indexing — failures here just mean the blob
        // won't appear in `ctx search`, but `ctx show` still works.
        let _ = self.index.add_and_commit(&id, &meta, &raw);

        format_summary(&id.reference(), &raw, self.config.preview_bytes)
    }
}

#[async_trait]
impl Tool for SandboxedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let raw = self.inner.execute(args.clone(), ctx).await?;
        if !self.should_stash(raw.len()) {
            return Ok(raw);
        }
        Ok(self.stash(raw, &args))
    }

    async fn execute_multimodal(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        // Multimodal results (images, documents) flow through unchanged
        // — the model needs the binary content blocks intact. Only
        // `Text` variants are eligible for stashing.
        let out = self.inner.execute_multimodal(args.clone(), ctx).await?;
        match out {
            ToolOutput::Text(raw) if self.should_stash(raw.len()) => {
                Ok(ToolOutput::Text(self.stash(raw, &args)))
            }
            other => Ok(other),
        }
    }
}

/// Best-effort source extraction from common tool argument shapes.
/// The agent uses this string in `metis ctx search` results.
fn source_from_args(tool: &str, args: &Value) -> Option<String> {
    let key = match tool {
        "read_file" | "edit_file" | "write_file" | "multi_edit" => "path",
        "web_fetch" | "web_search" => "url",
        "bash" => "command",
        "grep" | "glob" => "pattern",
        _ => return None,
    };
    args.get(key).and_then(|v| v.as_str()).map(|s| {
        if s.len() > 256 {
            let mut end = 256;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &s[..end])
        } else {
            s.to_string()
        }
    })
}

fn format_summary(reference: &str, raw: &str, preview_bytes: usize) -> String {
    let lines = raw.lines().count();
    let bytes = raw.len();
    let preview_end = char_safe_end(raw, preview_bytes);
    let preview = &raw[..preview_end];
    let truncated_marker = if preview_end < raw.len() {
        format!(
            "\n… [{} more bytes — fetch with `metis ctx show {}`]",
            bytes - preview_end,
            reference.trim_start_matches("ctx://")
        )
    } else {
        String::new()
    };
    format!(
        "[stashed: {ref_} — {bytes} bytes, {lines} lines]\n--- preview ---\n{preview}{truncated_marker}",
        ref_ = reference,
        bytes = bytes,
        lines = lines,
        preview = preview,
        truncated_marker = truncated_marker,
    )
}

fn char_safe_end(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut end = target;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Test tool: returns whatever string we hand it at construction.
    struct FakeTool {
        name: String,
        payload: Mutex<String>,
    }

    impl FakeTool {
        fn new(name: &str, payload: String) -> Self {
            Self {
                name: name.into(),
                payload: Mutex::new(payload),
            }
        }
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters_schema(&self) -> Value {
            json!({})
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
            Ok(self.payload.lock().unwrap().clone())
        }
    }

    fn setup() -> (TempDir, Arc<BlobStore>, Arc<BlobIndex>) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::open(tmp.path()).unwrap());
        let index = Arc::new(BlobIndex::open(tmp.path()).unwrap());
        (tmp, store, index)
    }

    fn ctx(workspace: &Path) -> ToolContext {
        ToolContext::new(workspace.to_path_buf())
    }

    #[tokio::test]
    async fn small_output_passes_through() {
        let (tmp, store, index) = setup();
        let inner = std::sync::Arc::new(FakeTool::new("bash", "tiny output".to_string()));
        let wrapped = SandboxedTool::wrap(inner, store, index, SandboxConfig::default());
        let out = wrapped.execute(json!({}), &ctx(tmp.path())).await.unwrap();
        assert_eq!(out, "tiny output");
    }

    #[tokio::test]
    async fn large_output_is_stashed_and_referenced() {
        let (tmp, store, index) = setup();
        let big = "A".repeat(8 * 1024);
        let inner = std::sync::Arc::new(FakeTool::new("bash", big));
        let wrapped = SandboxedTool::wrap(
            inner,
            store.clone(),
            index.clone(),
            SandboxConfig::default(),
        );
        let out = wrapped
            .execute(json!({"command": "yes"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(
            out.contains("[stashed: ctx://"),
            "expected stashed summary, got: {out}"
        );
        assert!(out.contains("8192 bytes"));
        assert_eq!(store.iter_ids().unwrap().len(), 1);
        let hits = index.search("yes", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn bypass_list_skips_stashing_even_for_large_output() {
        let (tmp, store, index) = setup();
        let big = "B".repeat(8 * 1024);
        let inner = std::sync::Arc::new(FakeTool::new("edit_file", big.clone()));
        let wrapped = SandboxedTool::wrap(inner, store.clone(), index, SandboxConfig::default());
        let out = wrapped.execute(json!({}), &ctx(tmp.path())).await.unwrap();
        assert_eq!(out, big);
        assert_eq!(store.iter_ids().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn disabled_config_passes_through() {
        let (tmp, store, index) = setup();
        let big = "C".repeat(8 * 1024);
        let inner = std::sync::Arc::new(FakeTool::new("bash", big.clone()));
        let wrapped = SandboxedTool::wrap(
            inner,
            store.clone(),
            index,
            SandboxConfig {
                enabled: false,
                ..Default::default()
            },
        );
        let out = wrapped.execute(json!({}), &ctx(tmp.path())).await.unwrap();
        assert_eq!(out, big);
        assert_eq!(store.iter_ids().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn source_is_extracted_from_args() {
        let (tmp, store, index) = setup();
        let big = "D".repeat(8 * 1024);
        let inner = std::sync::Arc::new(FakeTool::new("read_file", big));
        let wrapped = SandboxedTool::wrap(
            inner,
            store.clone(),
            index.clone(),
            SandboxConfig::default(),
        );
        let _ = wrapped
            .execute(json!({"path": "/tmp/big.txt"}), &ctx(tmp.path()))
            .await
            .unwrap();
        let id = store.iter_ids().unwrap().pop().unwrap();
        let (_, meta) = store.read(&id).unwrap();
        assert_eq!(meta.source.as_deref(), Some("/tmp/big.txt"));
        let hits = index.search("/tmp/big.txt", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn multimodal_text_is_stashed() {
        let (tmp, store, index) = setup();
        let big = "E".repeat(8 * 1024);
        let inner = std::sync::Arc::new(FakeTool::new("bash", big));
        let wrapped = SandboxedTool::wrap(inner, store.clone(), index, SandboxConfig::default());
        let out = wrapped
            .execute_multimodal(json!({}), &ctx(tmp.path()))
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.contains("[stashed: ctx://")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn char_safe_end_handles_multibyte() {
        // 'é' is 2 bytes in UTF-8.
        let s = "ée";
        // target 1 lands inside the 'é' multi-byte sequence —
        // must back off to 0.
        assert_eq!(char_safe_end(s, 1), 0);
    }

    #[test]
    fn format_summary_includes_truncation_marker() {
        let raw = "x".repeat(2000);
        let s = format_summary("ctx://abcd1234", &raw, 100);
        assert!(s.contains("ctx://abcd1234"));
        assert!(s.contains("more bytes"));
        assert!(s.contains("metis ctx show abcd1234"));
    }

    #[test]
    fn format_summary_no_marker_when_preview_covers_all() {
        let raw = "short";
        let s = format_summary("ctx://abcd1234", raw, 800);
        assert!(s.contains("short"));
        assert!(!s.contains("more bytes"));
    }
}
