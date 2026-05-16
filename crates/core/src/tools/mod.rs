//! Tool framework and the v0.1 built-in tool set.
//!
//! A `Tool` is anything the model can invoke through the OpenAI tool-call
//! protocol. The trait pins three things: a JSON-Schema description (so it
//! can be advertised to the provider), an executor (called when the model
//! emits a `tool_calls` entry), and a stable name used as the routing key.
//!
//! Design choices:
//!
//! * **One trait, one box, one registry.** We keep dispatch dynamic
//!   (`Box<dyn Tool>`) so the agent loop can iterate a heterogeneous tool
//!   set without macro tricks. The registry is a flat `Vec` lookup; with
//!   five tools a HashMap would be premature.
//! * **Workspace-rooted file ops.** `read_file`, `grep`, `glob`, and
//!   `edit_file` all clamp paths to the workspace root via
//!   [`ToolContext::resolve_path`]. The model cannot escape the working
//!   directory by passing `..` or an absolute path elsewhere on disk.
//! * **`bash` has process-level safeguards, not OS isolation.** v0.2 adds
//!   a wall-clock timeout, an output cap, and secret-env scrubbing so a
//!   runaway or curious model can't hang the agent, blow up the context,
//!   or exfiltrate API keys through `env`. Real OS-level isolation
//!   (sandbox-exec / bubblewrap) is opt-in via the host shell and
//!   documented in the README — we deliberately don't pretend to provide
//!   it from inside the tool.
//! * **String-only return type.** Tool results travel back to the model as
//!   the `content` of a tool message, which is a string. Doing the
//!   stringification inside the tool keeps the agent loop trivial.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use aegis_api::{FunctionSpec, ToolKind, ToolSpec};
use serde_json::Value;
use thiserror::Error;

// Per-domain tool submodules. Each module owns a group of tool impls
// and its own small helpers; shared types (ToolContext, ToolError,
// Tool trait, ToolRegistry, etc.) live in this file.
mod agent;
mod bash;
mod cluster;
mod code;
mod cron;
mod fs;
mod mcp;
mod memory;
mod notebook;
mod plan;
mod prompt_mod;
#[cfg(feature = "wasm")]
mod python_wasi;
mod semantic_memory_search;
mod system;
mod task;
#[cfg(feature = "wasm")]
mod wasm;
mod web;
mod monitor;
mod worktree;
pub use agent::{AgentTool, ParallelAgentsTool};
pub use bash::Bash;
pub use cluster::{CheckHallucination, ScanInput};
pub use code::{Lsp, RepoMap, SemanticSearch};
pub use cron::{read_crons, CronCreate, CronDelete, CronEntry, CronList};
pub use fs::{unified_diff, EditFile, GlobTool, Grep, MultiEdit, ReadFile, WriteFile};
pub use mcp::{
    register_mcp_server, spawn_mcp_server, spawn_mcp_server_with_cache, McpAuthenticate, McpSpec,
    McpTool, SpawnedMcpServer,
};
pub use memory::{DeleteMemory, ListMemories, ReadMemory, SaveMemory};
pub use monitor::{read_wakeup_hint, Monitor, ScheduleWakeup};
pub use notebook::NotebookEdit;
pub use plan::{EnterPlanMode, ExitPlanMode};
pub use prompt_mod::{ModifyPrompt, RollbackPrompt, ShowPromptChanges};
#[cfg(feature = "wasm")]
pub use python_wasi::PythonWasi;
pub use semantic_memory_search::SemanticMemorySearch;
pub use system::{AskUser, AskUserQuestion, RemoteTrigger, Screenshot, ToolSearch};
pub use task::{CreateTask, ListTasks, TaskEntry, UpdateTask};
#[cfg(feature = "wasm")]
pub use wasm::WasmRun;
pub use web::{WebFetch, WebSearch};
pub use worktree::{EnterWorktree, ExitWorktree};

/// Errors a tool can surface. The agent loop converts these into a tool
/// message so the model can see what went wrong and retry.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("path `{0}` escapes the workspace root")]
    PathEscape(String),
    #[error("io error on `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`old_string` not found in {0}")]
    EditNotFound(String),
    #[error("`old_string` is not unique in {path} ({count} matches)")]
    EditNotUnique { path: String, count: usize },
    #[error("regex compile failed: {0}")]
    BadRegex(#[from] regex::Error),
    #[error("glob pattern invalid: {0}")]
    BadGlob(#[from] glob::PatternError),
    #[error("command failed to spawn: {0}")]
    Spawn(String),
    #[error("command exceeded {0:?} wall-clock budget and was killed")]
    Timeout(Duration),
    #[error("MCP tool `{name}` failed: {message}")]
    McpFailed { name: String, message: String },
}

/// Callback for tools that need interactive user input (e.g.
/// `ask_user_question`). Takes a question string and a list of options;
/// returns the user's choice or freeform answer, or `None` if the tool
/// should fall back to a default.
pub type UserInputFn = Arc<dyn Fn(&str, &[String]) -> Option<String> + Send + Sync>;

/// Request to spawn a subagent from the `agent` tool.
#[derive(Debug, Clone)]
pub struct AgentSpawnRequest {
    pub description: String,
    pub prompt: String,
    pub subagent_type: Option<String>,
    pub model: Option<String>,
    pub run_in_background: bool,
    pub isolation: Option<String>,
}

/// Callback for the `agent` tool to spawn a subagent. Set by the agent
/// loop so it captures the provider, registry and config. Returns
/// the subagent's final text on success.
pub type AgentSpawnerFn = Arc<dyn Fn(AgentSpawnRequest) -> Result<String, String> + Send + Sync>;

/// A completed background agent result.
#[derive(Debug, Clone)]
pub struct BackgroundResult {
    pub description: String,
    pub result: Result<String, String>,
}

/// Thread-safe store for background agent results. The spawner pushes
/// completed results here; the REPL drains them between turns.
#[derive(Debug, Clone, Default)]
pub struct BackgroundAgents {
    completed: Arc<Mutex<Vec<BackgroundResult>>>,
    pending_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl BackgroundAgents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a background agent was started.
    pub fn inc_pending(&self) {
        self.pending_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Push a completed result and decrement pending count.
    pub fn push_completed(&self, result: BackgroundResult) {
        self.completed.lock().unwrap().push(result);
        self.pending_count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drain all completed results. Returns empty vec if none ready.
    pub fn drain_completed(&self) -> Vec<BackgroundResult> {
        let mut guard = self.completed.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    /// How many agents are still running.
    pub fn pending(&self) -> usize {
        self.pending_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Per-call context handed to every tool execution. Carries the workspace
/// root, bash safeguards, and an optional user-input callback for
/// interactive tools.
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub bash: BashConfig,
    /// Optional interactive input. Set by the REPL; absent in one-shot
    /// or non-interactive modes.
    pub user_input: Option<UserInputFn>,
    /// Plan-mode state machine. In `Drafting` state, only read-only
    /// tools may execute. Transitions: Normal → Drafting (enter) →
    /// Executing (exit/approve) → Normal (plan done).
    pub plan_state: Arc<Mutex<PlanState>>,
    /// Hook configuration loaded from `.metis/hooks.toml` and
    /// `~/.metis/hooks.toml`.
    pub hooks: crate::hooks::HookConfig,
    /// Bash command history for `[rerun: bN]` aliases.
    pub bash_history: Arc<Mutex<Vec<String>>>,
    /// Sticky cwd for the bash tool — each `bash` call captures the
    /// final `pwd` and stores it here so a subsequent call starts
    /// from the SAME directory the model left off in. Contract:
    /// working directory persists between commands, but shell state
    /// does not. `None` falls back to `effective_root()`.
    pub bash_cwd: Arc<Mutex<Option<PathBuf>>>,
    /// Active worktree state: (original_root, worktree_path, branch_name).
    /// Set by `enter_worktree`, cleared by `exit_worktree`.
    pub worktree: Arc<Mutex<Option<WorktreeState>>>,
    /// Subagent spawner callback. Set by the agent loop so the `agent`
    /// tool can spawn child agents without direct access to the provider.
    pub agent_spawner: Option<AgentSpawnerFn>,
    /// Shared store for background agent results.
    pub background_agents: BackgroundAgents,
    /// Tracks the last-read state (mtime) of files so edit_file can warn
    /// about external modifications since the last read_file call.
    pub file_read_times: Arc<Mutex<std::collections::HashMap<PathBuf, std::time::SystemTime>>>,
    /// Set to `true` by the TUI/REPL when the user interrupts the current
    /// turn. The bash tool and the agent loop check this to abort early.
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Root directory of the aegis source code. Used by `modify_prompt` /
    /// `rollback_prompt` tools to find and edit system_prompt.md.
    pub aegis_root: Option<PathBuf>,
}

/// Plan-mode state machine.
///
/// Three states:
/// - `Normal` — all tools allowed, no plan context.
/// - `Drafting` — plan mode active, only read-only tools allowed.
///   The model is expected to research and draft a plan.
/// - `Executing` — plan approved, all tools allowed. The plan text
///   from the drafting phase is preserved as execution context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanState {
    Normal,
    Drafting,
    Executing,
}

impl PlanState {
    /// Returns true if mutating tools should be blocked.
    pub fn is_read_only(&self) -> bool {
        matches!(self, PlanState::Drafting)
    }
}

impl std::fmt::Display for PlanState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanState::Normal => write!(f, "normal"),
            PlanState::Drafting => write!(f, "drafting"),
            PlanState::Executing => write!(f, "executing"),
        }
    }
}

/// State of an active git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeState {
    /// The original workspace root before entering the worktree.
    pub original_root: PathBuf,
    /// Path to the worktree directory.
    pub worktree_path: PathBuf,
    /// Branch name created for the worktree.
    pub branch_name: String,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .field("bash", &self.bash)
            .field("user_input", &self.user_input.as_ref().map(|_| "..."))
            .field("plan_state", &*self.plan_state.lock().unwrap())
            .field(
                "hooks",
                &format!(
                    "{} events configured",
                    if self.hooks.is_empty() { 0 } else { 5 }
                ),
            )
            .field("bash_history_len", &self.bash_history.lock().unwrap().len())
            .field("worktree", &self.worktree.lock().unwrap().is_some())
            .field("agent_spawner", &self.agent_spawner.as_ref().map(|_| "..."))
            .field("background_pending", &self.background_agents.pending())
            .field(
                "file_read_count",
                &self.file_read_times.lock().unwrap().len(),
            )
            .finish()
    }
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            workspace_root: self.workspace_root.clone(),
            bash: self.bash.clone(),
            user_input: self.user_input.clone(),
            plan_state: Arc::clone(&self.plan_state),
            hooks: self.hooks.clone(),
            bash_history: Arc::clone(&self.bash_history),
            bash_cwd: Arc::clone(&self.bash_cwd),
            worktree: Arc::clone(&self.worktree),
            agent_spawner: self.agent_spawner.clone(),
            background_agents: self.background_agents.clone(),
            file_read_times: Arc::clone(&self.file_read_times),
            cancel_flag: Arc::clone(&self.cancel_flag),
            aegis_root: self.aegis_root.clone(),
        }
    }
}

/// Tunables for the `bash` tool's process-level safeguards. Defaults are
/// chosen to be invisible to well-behaved commands (echo, ls, cargo) while
/// still catching the three failure modes that hurt the agent loop: hangs,
/// runaway output, and silently leaked API keys.
#[derive(Debug, Clone)]
pub struct BashConfig {
    /// Wall-clock budget. The child is `kill`'d if it overruns.
    pub timeout: Duration,
    /// Hard cap on combined stdout+stderr returned to the model. Anything
    /// past this is dropped and replaced with a `[truncated]` marker.
    pub max_output_bytes: usize,
    /// When true, environment variables whose names look like secrets
    /// (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`) or that belong
    /// to known LLM providers (`DEEPSEEK_*`, `OPENAI_*`, `ANTHROPIC_*`,
    /// `METIS_*`) are stripped from the child's environment.
    pub scrub_secret_env: bool,
    /// Sandboxing mode for bash commands. On macOS uses `sandbox-exec`,
    /// on Linux uses bubblewrap (`bwrap`). The sandbox restricts the
    /// child process to the workspace directory for writes while allowing
    /// reads from system paths.
    pub sandbox: SandboxMode,
}

/// Sandboxing mode for the bash tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// No OS-level sandbox — the child inherits the full authority of
    /// the user running metis. Process-level safeguards (timeout, output
    /// cap, env scrubbing) still apply.
    #[default]
    None,
    /// macOS `sandbox-exec` with a SBPL profile that allows reads
    /// everywhere but restricts writes to the workspace directory.
    /// Falls back to `None` if `sandbox-exec` is not available.
    SandboxExec,
    /// Linux `bwrap` (bubblewrap) sandbox. Mounts the workspace
    /// read-write and the rest of the filesystem read-only.
    /// Falls back to `None` if `bwrap` is not available.
    Bubblewrap,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_output_bytes: 64 * 1024,
            scrub_secret_env: true,
            sandbox: SandboxMode::None,
        }
    }
}

impl BashConfig {
    fn should_scrub(&self, name: &str) -> bool {
        self.scrub_secret_env && is_secret_env_var(name)
    }
}

/// Returns true if an env var name looks like a secret.
/// Used by the bash tool AND by hooks/lint to ensure keys don't leak
/// to child processes that the model may be able to observe.
pub fn is_secret_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const PROVIDER_PREFIXES: &[&str] = &[
        "DEEPSEEK_",
        "OPENAI_",
        "ANTHROPIC_",
        "METIS_",
        "NVIDIA_",
        "MINIMAX_",
        "GEMINI_",
        "GOOGLE_",
        "GROQ_",
        "TOGETHER_",
        "OPENROUTER_",
        "TAVILY_",
    ];
    const SECRET_SUFFIXES: &[&str] =
        &["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_PRIVATE_KEY"];
    PROVIDER_PREFIXES.iter().any(|p| upper.starts_with(p))
        || SECRET_SUFFIXES.iter().any(|s| upper.ends_with(s))
}

impl ToolContext {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            bash: BashConfig::default(),
            user_input: None,
            plan_state: Arc::new(Mutex::new(PlanState::Normal)),
            hooks: crate::hooks::HookConfig::default(),
            bash_history: Arc::new(Mutex::new(Vec::new())),
            bash_cwd: Arc::new(Mutex::new(None)),
            worktree: Arc::new(Mutex::new(None)),
            agent_spawner: None,
            background_agents: BackgroundAgents::new(),
            file_read_times: Arc::new(Mutex::new(std::collections::HashMap::new())),
            cancel_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            aegis_root: None,
        }
    }

    /// Attach an externally-owned cancel flag. The TUI/REPL sets this
    /// to `true` on user interrupt; the bash tool and agent loop poll it.
    pub fn with_cancel(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel_flag = flag;
        self
    }

    /// Sets the plan-state. The caller can keep a clone of the `Arc`
    /// to change state from the outside (e.g. a REPL `/plan` command)
    /// without rebuilding the agent.
    pub fn with_plan_state(mut self, state: Arc<Mutex<PlanState>>) -> Self {
        self.plan_state = state;
        self
    }

    /// Sets the hook configuration (from TOML files).
    pub fn with_hooks(mut self, hooks: crate::hooks::HookConfig) -> Self {
        self.hooks = hooks;
        self
    }

    /// Sets the interactive input callback. Used by the REPL to allow
    /// tools like `ask_user_question` to prompt the user mid-turn.
    pub fn with_user_input(mut self, f: UserInputFn) -> Self {
        self.user_input = Some(f);
        self
    }

    /// Sets the subagent spawner callback for the `agent` tool.
    pub fn with_agent_spawner(mut self, f: AgentSpawnerFn) -> Self {
        self.agent_spawner = Some(f);
        self
    }

    /// Builder-style override for the bash safeguards. Useful for tests
    /// (tight timeouts, tiny output caps) and for callers that want to
    /// loosen or tighten the defaults at startup.
    pub fn with_bash_config(mut self, bash: BashConfig) -> Self {
        self.bash = bash;
        self
    }

    /// Joins `requested` onto the workspace root, then verifies the
    /// canonical result is still inside the root. Used by every file tool
    /// to keep the model from poking at files outside the project.
    /// Returns the effective root: worktree path if active, otherwise workspace_root.
    pub fn effective_root(&self) -> PathBuf {
        if let Ok(wt) = self.worktree.lock() {
            if let Some(ref state) = *wt {
                return state.worktree_path.clone();
            }
        }
        self.workspace_root.clone()
    }

    pub fn resolve_path(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let root = self.effective_root();
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            root.join(requested)
        };
        let resolved = normalize_lexically(&candidate);
        // Block reads of known-sensitive paths regardless of workspace.
        // Cross-workspace reads are otherwise allowed (the model needs them
        // for cross-repo work), but config files with plaintext API keys and
        // SSH keys must never be handed to the model.
        if is_sensitive_path(&resolved) {
            return Err(ToolError::PathEscape(format!(
                "{} — sensitive path is protected",
                resolved.display()
            )));
        }
        Ok(resolved)
    }
}

/// Lexical `..` resolution that does not touch the filesystem. Good
/// enough for the workspace-escape check; we never need to follow
/// symlinks for this guard because the model only sees paths it asked
/// for.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        use std::path::Component::*;
        match component {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Returns true for paths the model must never be able to read, regardless
/// of workspace location. Covers plaintext key stores, SSH keys, and GPG.
fn is_sensitive_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    // Metis config — contains plaintext API keys
    if s.contains("/.metis/config") {
        return true;
    }
    // SSH and GPG private material
    if s.contains("/.ssh/") || s.contains("/.gnupg/") {
        return true;
    }
    // AWS credential files
    if s.contains("/.aws/credentials") || s.contains("/.aws/config") {
        return true;
    }
    // macOS Keychain DB
    if s.contains("/Library/Keychains/") {
        return true;
    }
    false
}

/// Result from a tool execution. Most tools return plain text; multimodal
/// tools (e.g. `read_file` on images) can return content blocks.
#[derive(Debug, Clone)]
pub enum ToolOutput {
    Text(String),
    Multimodal {
        /// Text summary for providers that don't support vision.
        fallback_text: String,
        /// Content blocks for providers that support vision/documents.
        blocks: Vec<aegis_api::ContentBlock>,
    },
}

impl ToolOutput {
    /// Extract plain text, discarding any content blocks.
    pub fn as_text(&self) -> &str {
        match self {
            ToolOutput::Text(t) => t,
            ToolOutput::Multimodal { fallback_text, .. } => fallback_text,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(s: String) -> Self {
        ToolOutput::Text(s)
    }
}

/// The interface every tool implements. The lifetime on `name` and
/// `description` is the borrow of `&self`, not `'static`, so tools whose
/// identity is only known at runtime (e.g. MCP-bridged tools) can store
/// their own owned strings and hand back a borrow.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;

    /// Multimodal execution. Default delegates to `execute` and wraps in Text.
    /// Override for tools that produce image/document content blocks.
    async fn execute_multimodal(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.execute(args, ctx).await.map(ToolOutput::Text)
    }

    /// Builds the OpenAI `ToolSpec` envelope from the trait methods.
    /// Tools should not normally override this.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            kind: ToolKind::Function,
            function: FunctionSpec {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: self.parameters_schema(),
            },
        }
    }
}

/// Registry of tools available to the agent loop. Owns its tools so the
/// loop only needs a single borrow.
pub struct ToolRegistry {
    tools: std::sync::Arc<std::sync::RwLock<Vec<std::sync::Arc<dyn Tool>>>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ToolRegistry({} tools)", self.tools.read().unwrap().len())
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }

    /// Returns a registry containing the v0.1 built-in tool set:
    /// `read_file`, `grep`, `glob`, `edit_file`, `bash`.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Box::new(ReadFile));
        r.register(Box::new(Grep));
        r.register(Box::new(GlobTool));
        r.register(Box::new(WriteFile));
        r.register(Box::new(EditFile));
        r.register(Box::new(MultiEdit));
        r.register(Box::new(Bash));
        r.register(Box::new(SaveMemory));
        r.register(Box::new(ListMemories));
        r.register(Box::new(ReadMemory));
        r.register(Box::new(DeleteMemory));
        r.register(Box::new(SemanticMemorySearch));
        r.register(Box::new(WebFetch));
        r.register(Box::new(AskUserQuestion));
        r.register(Box::new(AskUser));
        // Atakan: create_task/update_task agent'a register edilmedi —
        // model her user mesajını / system reminder echo'sunu task'a
        // çeviriyordu, reject_chatlike heuristik filtresi de yetmedi
        // (uzun cümleler, ASCII dump'lar, "Hiçbir tool çağırma..." gibi
        // sistem-context echo'ları geçiyordu). ListTasks read-only,
        // kalıyor. Manuel ekleme için TUI `/task add` slash command
        // kullanılır. Geri açmak için flag istenirse eklenebilir.
        // r.register(Box::new(CreateTask));
        // r.register(Box::new(UpdateTask));
        r.register(Box::new(ListTasks));
        r.register(Box::new(EnterPlanMode));
        r.register(Box::new(ExitPlanMode));
        r.register(Box::new(ModifyPrompt));
        r.register(Box::new(RollbackPrompt));
        r.register(Box::new(ShowPromptChanges));
        r.register(Box::new(ToolSearch));
        r.register(Box::new(NotebookEdit));
        r.register(Box::new(WebSearch));
        r.register(Box::new(EnterWorktree));
        r.register(Box::new(ExitWorktree));
        r.register(Box::new(CronCreate));
        r.register(Box::new(CronList));
        r.register(Box::new(CronDelete));
        r.register(Box::new(Monitor));
        r.register(Box::new(ScheduleWakeup));
        r.register(Box::new(Lsp));
        r.register(Box::new(RemoteTrigger));
        r.register(Box::new(McpAuthenticate));
        r.register(Box::new(AgentTool));
        r.register(Box::new(ParallelAgentsTool));
        r.register(Box::new(RepoMap));
        r.register(Box::new(SemanticSearch));
        r.register(Box::new(Screenshot));
        r.register(Box::new(CheckHallucination));
        r.register(Box::new(ScanInput));
        #[cfg(feature = "wasm")]
        r.register(Box::new(WasmRun));
        #[cfg(feature = "wasm")]
        r.register(Box::new(PythonWasi));
        r
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.write().unwrap().push(std::sync::Arc::from(tool));
    }

    /// Register a tool at runtime without exclusive access. Safe to call
    /// while the agent is between turns (no rebuild required).
    pub fn register_late(&self, tool: Box<dyn Tool>) {
        self.tools.write().unwrap().push(std::sync::Arc::from(tool));
    }

    /// Returns a cloned Arc handle to the tool, safe to use after the
    /// lock is released (including across await points).
    pub fn get(&self, name: &str) -> Option<std::sync::Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap()
            .iter()
            .find(|t| t.name() == name)
            .cloned()
    }

    /// Renders all registered tools as the `tools` field of a chat
    /// completion request. Deduplicates by name (first registration wins,
    /// so built-ins take precedence over MCP tools with the same name).
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut seen = std::collections::HashSet::new();
        self.tools
            .read()
            .unwrap()
            .iter()
            .filter(|t| seen.insert(t.name().to_string()))
            .map(|t| t.spec())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.read().unwrap().is_empty()
    }

    /// Wrap every registered tool in a [`crate::sandbox_exec::SandboxedTool`]
    /// so large outputs are auto-stashed into the supplied
    /// [`crate::blob_store::BlobStore`] / [`crate::blob_index::BlobIndex`].
    /// The wrapped tools keep their original name/description/schema.
    #[cfg(feature = "ctx")]
    pub fn wrap_with_sandbox(
        &mut self,
        store: std::sync::Arc<crate::blob_store::BlobStore>,
        index: std::sync::Arc<crate::blob_index::BlobIndex>,
        config: crate::sandbox_exec::SandboxConfig,
    ) {
        let mut guard = self.tools.write().unwrap();
        let old = std::mem::take(&mut *guard);
        *guard = old
            .into_iter()
            .map(|t| {
                std::sync::Arc::from(crate::sandbox_exec::SandboxedTool::wrap(
                    t,
                    store.clone(),
                    index.clone(),
                    config.clone(),
                ))
            })
            .collect();
    }

    /// Open a [`BlobStore`](crate::blob_store::BlobStore) +
    /// [`BlobIndex`](crate::blob_index::BlobIndex) under
    /// `<workspace>/.metis/` and wrap every tool in one call.
    /// Returns the (store, index) pair so the caller can keep handles
    /// for `metis ctx search`/`show`/`prune`.
    #[cfg(feature = "ctx")]
    pub fn enable_sandbox(
        &mut self,
        workspace: &std::path::Path,
        config: crate::sandbox_exec::SandboxConfig,
    ) -> Result<
        (
            std::sync::Arc<crate::blob_store::BlobStore>,
            std::sync::Arc<crate::blob_index::BlobIndex>,
        ),
        anyhow::Error,
    > {
        let store = std::sync::Arc::new(crate::blob_store::BlobStore::open(workspace)?);
        let index = std::sync::Arc::new(crate::blob_index::BlobIndex::open(workspace)?);
        self.wrap_with_sandbox(store.clone(), index.clone(), config);
        Ok((store, index))
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}
// fs tools moved to tools/fs.rs

// bash tool moved to tools/bash.rs

// ask_user_question moved to tools/system.rs

// task + cron tools moved to tools/{task,cron}.rs

// remote_trigger moved to tools/system.rs

// code-intel tools moved to tools/code.rs

// agent tools moved to tools/agent.rs

// plan-mode tools moved to tools/plan.rs

// mcp_authenticate moved to tools/mcp.rs

// tool_search moved to tools/system.rs

// worktree tools moved to tools/worktree.rs

// notebook_edit moved to tools/notebook.rs

/// Names of tools allowed in plan mode (read-only).
pub const PLAN_MODE_ALLOWED: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_tasks",
    "list_memories",
    "read_memory",
    "web_fetch",
    "ask_user_question",
    "enter_plan_mode",
    "exit_plan_mode",
    "tool_search",
    "agent",
];

// MCP bridge (McpTool + register_mcp_server) moved to tools/mcp.rs

// screenshot tool moved to tools/system.rs

// ---------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("metis-tools-{}-{}", std::process::id(), n,));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // canonicalize so the workspace-escape check compares apples-to-apples
        // (macOS temp dir is a symlink under /private).
        fs::canonicalize(&dir).unwrap()
    }

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn registry_with_builtins_has_thirty_tools() {
        let r = ToolRegistry::with_builtins();
        // wasm_run is feature-gated (`wasm`), so the count differs by build.
        // Default: 38 tools after CreateTask/UpdateTask removal — model
        // was abusing them as a chat scratchpad. ListTasks/EnterPlanMode
        // /ExitPlanMode stay. With `wasm`: +wasm_run +python_wasi.
        let expected = if cfg!(feature = "wasm") { 40 } else { 38 };
        assert_eq!(r.len(), expected);
        for name in [
            "read_file",
            "grep",
            "glob",
            "write_file",
            "edit_file",
            "multi_edit",
            "bash",
            "save_memory",
            "list_memories",
            "read_memory",
            "delete_memory",
            "web_fetch",
            "ask_user_question",
            "list_tasks",
            "enter_plan_mode",
            "exit_plan_mode",
            "tool_search",
            "notebook_edit",
            "web_search",
            "enter_worktree",
            "exit_worktree",
            "cron_create",
            "cron_list",
            "cron_delete",
            "lsp",
            "remote_trigger",
            "mcp_authenticate",
            "agent",
            "parallel_agents",
            "repo_map",
            "semantic_search",
            "check_hallucination",
            "scan_input",
        ] {
            assert!(r.get(name).is_some(), "missing {name}");
        }
    }

    #[tokio::test]
    async fn read_file_returns_numbered_lines() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = ReadFile
            .execute(json!({"path": "a.txt"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("1\talpha"));
        assert!(out.contains("3\tgamma"));
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let out = ReadFile
            .execute(
                json!({"path": "a.txt", "offset": 2, "limit": 2}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("2\t2"));
        assert!(out.contains("3\t3"));
        assert!(!out.contains("4\t4"));
    }

    #[tokio::test]
    async fn resolve_path_allows_cross_workspace_reads() {
        // New policy (2026-04-19): absolute paths outside the
        // workspace resolve without rejection so the model can read
        // cross-repo without reaching for `bash cat`. Safety rests on
        // the permission gate for mutating tools, not on a path
        // allowlist for reads.
        let dir = tempdir();
        let ctx = ctx(&dir);
        let outside = std::env::temp_dir().join("metis-cross-read.txt");
        std::fs::write(&outside, "hello from outside").unwrap();
        let resolved = ctx.resolve_path(outside.to_str().unwrap()).unwrap();
        assert_eq!(resolved, outside);
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn grep_finds_matches_and_skips_excluded_dirs() {
        let dir = tempdir();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() { needle(); }\n").unwrap();
        fs::write(dir.join("target/junk.rs"), "needle in build dir\n").unwrap();
        let out = Grep
            .execute(json!({"pattern": "needle"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("src/main.rs"));
        assert!(
            !out.contains("target/"),
            "target should be excluded:\n{out}"
        );
    }

    #[tokio::test]
    async fn grep_files_with_matches_mode() {
        let dir = tempdir();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "fn main() { needle(); }\n").unwrap();
        fs::write(dir.join("src/b.rs"), "no match here\n").unwrap();
        fs::write(dir.join("src/c.rs"), "another needle\n").unwrap();
        let out = Grep
            .execute(
                json!({"pattern": "needle", "output_mode": "files_with_matches"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("src/a.rs"), "a.rs should match: {out}");
        assert!(out.contains("src/c.rs"), "c.rs should match: {out}");
        assert!(!out.contains("src/b.rs"), "b.rs should not match: {out}");
        // Should NOT contain line numbers or content.
        assert!(
            !out.contains("needle"),
            "files mode should not show content: {out}"
        );
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let dir = tempdir();
        fs::write(
            dir.join("multi.txt"),
            "needle one\nno match\nneedle two\nneedle three\n",
        )
        .unwrap();
        let out = Grep
            .execute(
                json!({"pattern": "needle", "output_mode": "count"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("multi.txt:3"), "should show count of 3: {out}");
    }

    #[tokio::test]
    async fn grep_type_filter() {
        let dir = tempdir();
        fs::write(dir.join("code.rs"), "fn needle() {}\n").unwrap();
        fs::write(dir.join("code.py"), "def needle(): pass\n").unwrap();
        let out = Grep
            .execute(json!({"pattern": "needle", "type": "rust"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("code.rs"), "should find .rs: {out}");
        assert!(!out.contains("code.py"), "should skip .py: {out}");
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let dir = tempdir();
        fs::write(
            dir.join("mixed.txt"),
            "Hello World\nhello world\nHELLO WORLD\n",
        )
        .unwrap();
        let out = Grep
            .execute(json!({"pattern": "hello", "-i": true}), &ctx(&dir))
            .await
            .unwrap();
        // All 3 lines should match.
        assert!(out.contains("Hello World"), "case insensitive: {out}");
        assert!(out.contains("HELLO WORLD"), "case insensitive: {out}");
    }

    #[tokio::test]
    async fn grep_context_lines() {
        let dir = tempdir();
        fs::write(dir.join("ctx.txt"), "line1\nline2\nNEEDLE\nline4\nline5\n").unwrap();
        let out = Grep
            .execute(json!({"pattern": "NEEDLE", "-B": 1, "-A": 1}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("line2"), "before context missing: {out}");
        assert!(out.contains("NEEDLE"), "match missing: {out}");
        assert!(out.contains("line4"), "after context missing: {out}");
    }

    #[tokio::test]
    async fn grep_head_limit_and_offset() {
        let dir = tempdir();
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("needle_{i}\n"));
        }
        fs::write(dir.join("many.txt"), &content).unwrap();
        // Skip 3, take 2.
        let out = Grep
            .execute(
                json!({"pattern": "needle", "offset": 3, "head_limit": 2}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        // Should contain exactly 2 matching lines (plus possible truncation notice).
        let match_lines: Vec<&str> = out.lines().filter(|l| l.contains("needle_")).collect();
        assert_eq!(
            match_lines.len(),
            2,
            "should return exactly 2 matches: {out}"
        );
        assert!(out.contains("needle_3"), "first after skip: {out}");
        assert!(out.contains("needle_4"), "second after skip: {out}");
    }

    #[tokio::test]
    async fn grep_multiline_matches_across_lines() {
        let dir = tempdir();
        fs::write(dir.join("ml.txt"), "start\nfoo\nbar\nend\n").unwrap();
        let out = Grep
            .execute(
                json!({"pattern": "foo.*bar", "multiline": true}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(
            out.contains("foo") && out.contains("bar"),
            "multiline match should find cross-line pattern: {out}"
        );
    }

    #[tokio::test]
    async fn glob_lists_matching_paths() {
        let dir = tempdir();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "").unwrap();
        fs::write(dir.join("src/b.rs"), "").unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        let out = GlobTool
            .execute(json!({"pattern": "src/*.rs"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("a.rs"));
        assert!(out.contains("b.rs"));
        assert!(!out.contains("README"));
    }

    #[tokio::test]
    async fn glob_skips_bloat_dirs_and_uses_prefix() {
        // Regression: home-dir workspaces used to walk ~/Library,
        // ~/.rustup, etc. and time out at 10s. Verify the bloat list
        // filters them AND that the walker honors the literal prefix
        // of the pattern (so `src/**/*.rs` doesn't descend siblings).
        let dir = tempdir();
        fs::create_dir_all(dir.join("src/nested")).unwrap();
        fs::create_dir_all(dir.join("Library/Caches")).unwrap();
        fs::create_dir_all(dir.join(".rustup/toolchains")).unwrap();
        fs::create_dir_all(dir.join("__pycache__")).unwrap();
        fs::write(dir.join("src/nested/hit.rs"), "").unwrap();
        fs::write(dir.join("Library/Caches/junk.rs"), "").unwrap();
        fs::write(dir.join(".rustup/toolchains/junk.rs"), "").unwrap();
        fs::write(dir.join("__pycache__/junk.rs"), "").unwrap();

        let out = GlobTool
            .execute(json!({"pattern": "src/**/*.rs"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("src/nested/hit.rs"), "{out}");
        assert!(!out.contains("Library"), "bloat dir leaked: {out}");
        assert!(!out.contains(".rustup"), "bloat dir leaked: {out}");
        assert!(!out.contains("__pycache__"), "bloat dir leaked: {out}");
    }

    #[tokio::test]
    async fn glob_still_finds_hidden_when_pattern_targets_them() {
        // If the user explicitly globs for `.github/**/*.yml` we must
        // honor that and not silently filter out the hidden dir.
        let dir = tempdir();
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/ci.yml"), "").unwrap();
        let out = GlobTool
            .execute(json!({"pattern": ".github/**/*.yml"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("ci.yml"), "{out}");
    }

    #[tokio::test]
    async fn edit_file_unique_replacement() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        EditFile
            .execute(
                json!({"path": "a.txt", "old_string": "world", "new_string": "metis"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello metis\n"
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "x\nx\n").unwrap();
        let err = EditFile
            .execute(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
                &ctx(&dir),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::EditNotUnique { count: 2, .. }));
    }

    #[tokio::test]
    async fn edit_file_replace_all() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "x\nx\n").unwrap();
        EditFile
            .execute(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "y\ny\n");
    }

    // ── MultiEdit tests ────────────────────────────────────────────

    #[tokio::test]
    async fn multi_edit_happy_path_two_files() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "hello world").unwrap();
        fs::write(dir.join("b.txt"), "foo bar baz").unwrap();
        let out = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "a.txt", "old_string": "hello", "new_string": "hi" },
                        { "path": "b.txt", "old_string": "bar", "new_string": "qux" }
                    ]
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "hi world");
        assert_eq!(
            fs::read_to_string(dir.join("b.txt")).unwrap(),
            "foo qux baz"
        );
        assert!(out.contains("2 edit(s) applied atomically"));
        assert!(out.contains("a.txt"));
        assert!(out.contains("b.txt"));
    }

    #[tokio::test]
    async fn multi_edit_rollback_on_second_failure() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "alpha beta").unwrap();
        fs::write(dir.join("b.txt"), "gamma delta").unwrap();
        let result = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "a.txt", "old_string": "alpha", "new_string": "ALPHA" },
                        { "path": "b.txt", "old_string": "MISSING", "new_string": "x" }
                    ]
                }),
                &ctx(&dir),
            )
            .await;
        assert!(result.is_err());
        // a.txt must NOT be modified (validation catches before any write)
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "alpha beta");
        assert_eq!(
            fs::read_to_string(dir.join("b.txt")).unwrap(),
            "gamma delta"
        );
    }

    #[tokio::test]
    async fn multi_edit_not_unique_rejects() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "x x x").unwrap();
        let result = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "a.txt", "old_string": "x", "new_string": "y" }
                    ]
                }),
                &ctx(&dir),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "x x x");
    }

    #[tokio::test]
    async fn multi_edit_replace_all() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "aaa").unwrap();
        fs::write(dir.join("b.txt"), "bbb").unwrap();
        let out = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "a.txt", "old_string": "a", "new_string": "A", "replace_all": true },
                        { "path": "b.txt", "old_string": "b", "new_string": "B", "replace_all": true }
                    ]
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "AAA");
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "BBB");
        assert!(out.contains("3 replacement"));
    }

    #[tokio::test]
    async fn multi_edit_same_file_multiple_edits() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "one two three").unwrap();
        let out = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "a.txt", "old_string": "one", "new_string": "1" },
                        { "path": "a.txt", "old_string": "two", "new_string": "2" }
                    ]
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "1 2 three");
        assert!(out.contains("2 edit(s)"));
    }

    #[tokio::test]
    async fn multi_edit_file_not_found() {
        let dir = tempdir();
        let result = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "nonexistent.txt", "old_string": "a", "new_string": "b" }
                    ]
                }),
                &ctx(&dir),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multi_edit_returns_diffs() {
        let dir = tempdir();
        fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        let out = MultiEdit
            .execute(
                json!({
                    "edits": [
                        { "path": "x.rs", "old_string": "fn main() {}", "new_string": "fn main() {\n    println!(\"hi\");\n}" }
                    ]
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("--- a/x.rs"));
        assert!(out.contains("+++ b/x.rs"));
    }

    #[tokio::test]
    async fn write_file_creates_new_file() {
        let dir = tempdir();
        let out = WriteFile
            .execute(
                json!({"path": "new.txt", "content": "hello world\n"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("wrote"));
        assert_eq!(
            fs::read_to_string(dir.join("new.txt")).unwrap(),
            "hello world\n"
        );
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempdir();
        WriteFile
            .execute(json!({"path": "a/b/c.txt", "content": "deep"}), &ctx(&dir))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a/b/c.txt")).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_overwrites_existing() {
        let dir = tempdir();
        fs::write(dir.join("exist.txt"), "old").unwrap();
        WriteFile
            .execute(json!({"path": "exist.txt", "content": "new"}), &ctx(&dir))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("exist.txt")).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_file_accepts_path_outside_workspace() {
        // Matches the loosened `resolve_path` policy — writes outside
        // the workspace are allowed by the tool layer. The interactive
        // permission gate (not exercised in this test harness) is now
        // the sole guard; `--yes` / `AllowAll` skip it by design.
        let dir = tempdir();
        let outside = std::env::temp_dir().join(format!(
            "metis-cross-write-{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let result = WriteFile
            .execute(
                json!({"path": outside.to_str().unwrap(), "content": "ok"}),
                &ctx(&dir),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn ask_user_question_returns_answer() {
        let dir = tempdir();
        let ctx = ToolContext::new(dir.clone())
            .with_user_input(Arc::new(|_q, _opts| Some("yes please".to_string())));
        let out = AskUserQuestion
            .execute(json!({"question": "proceed?"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("yes please"), "answer: {out}");
    }

    #[tokio::test]
    async fn ask_user_question_with_options_passes_them_to_callback() {
        let dir = tempdir();
        // The REPL callback resolves numbered options; the tool just
        // passes the question and options through to the callback.
        let ctx = ToolContext::new(dir.clone()).with_user_input(Arc::new(|_q, opts| {
            // Simulate REPL resolving option "2" → "beta"
            Some(opts.get(1).cloned().unwrap_or_else(|| "fallback".into()))
        }));
        let out = AskUserQuestion
            .execute(
                json!({"question": "pick one", "options": ["alpha", "beta", "gamma"]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("beta"), "callback should see options: {out}");
    }

    #[tokio::test]
    async fn ask_user_question_fails_without_callback() {
        let dir = tempdir();
        let result = AskUserQuestion
            .execute(json!({"question": "hello?"}), &ctx(&dir))
            .await;
        assert!(result.is_err(), "should fail in non-interactive mode");
    }

    #[tokio::test]
    async fn task_crud_lifecycle() {
        let dir = tempdir();
        let c = ctx(&dir);
        // Create two tasks. Descriptions intentionally >3 words to clear
        // the create_task chat-like guard.
        let out = CreateTask
            .execute(json!({"description": "fix bug in parser"}), &c)
            .await
            .unwrap();
        assert!(out.contains("#1"), "first task id: {out}");
        let out = CreateTask
            .execute(json!({"description": "add feature flag toggle"}), &c)
            .await
            .unwrap();
        assert!(out.contains("#2"), "second task id: {out}");
        // List shows both pending.
        let out = ListTasks.execute(json!({}), &c).await.unwrap();
        assert!(out.contains("fix bug"), "list: {out}");
        assert!(out.contains("add feature"), "list: {out}");
        assert!(out.contains("pending"), "list: {out}");
        // Update task 1 to in_progress, then completed.
        let out = UpdateTask
            .execute(json!({"id": 1, "status": "in_progress"}), &c)
            .await
            .unwrap();
        assert!(out.contains("pending → in_progress"), "update: {out}");
        let out = UpdateTask
            .execute(json!({"id": 1, "status": "completed"}), &c)
            .await
            .unwrap();
        assert!(out.contains("in_progress → completed"), "update: {out}");
        // List shows task 1 completed.
        let out = ListTasks.execute(json!({}), &c).await.unwrap();
        assert!(out.contains("✓ #1"), "completed marker: {out}");
        assert!(
            out.contains("→ #2") || out.contains("· #2"),
            "task 2 still pending: {out}"
        );
    }

    #[tokio::test]
    async fn task_update_nonexistent_fails() {
        let dir = tempdir();
        let result = UpdateTask
            .execute(json!({"id": 999, "status": "completed"}), &ctx(&dir))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_update_invalid_status_fails() {
        let dir = tempdir();
        let c = ctx(&dir);
        CreateTask
            .execute(json!({"description": "test the update path"}), &c)
            .await
            .unwrap();
        let result = UpdateTask
            .execute(json!({"id": 1, "status": "invalid"}), &c)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_list_empty_workspace() {
        let dir = tempdir();
        let out = ListTasks.execute(json!({}), &ctx(&dir)).await.unwrap();
        assert!(out.contains("no tasks"), "empty: {out}");
    }

    #[tokio::test]
    async fn bash_runs_command_and_reports_exit() {
        let dir = tempdir();
        let out = Bash
            .execute(json!({"command": "echo hi && false"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains("[exit] 1"));
        // Sentinel must not leak to the model — we strip the trailing
        // `__METIS_CWD__=…` line before returning output.
        assert!(!out.contains("__METIS_CWD__"), "sentinel leaked: {out}");
    }

    #[tokio::test]
    async fn bash_cwd_persists_across_calls() {
        // Two-call contract: a `cd` in call 1 moves us into the
        // subdirectory for call 2. Matches the "working directory
        // persists between commands" promise in the tool description.
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/marker.txt"), "here").unwrap();
        let ctx = ctx(&dir);

        let _ = Bash
            .execute(json!({"command": "cd sub"}), &ctx)
            .await
            .unwrap();

        let out = Bash
            .execute(json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("here"), "expected cwd to persist: {out}");
    }

    #[tokio::test]
    async fn bash_cwd_resets_to_workspace_root_when_not_set() {
        // First call on a fresh ctx must execute from workspace root
        // even if a later call elsewhere would persist a different
        // cwd. Guards against leaked state between sessions.
        let dir = tempdir();
        std::fs::write(dir.join("root.txt"), "root").unwrap();
        let ctx = ctx(&dir);
        let out = Bash
            .execute(json!({"command": "cat root.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("root"));
    }

    #[tokio::test]
    async fn bash_timeout_kills_runaway_command() {
        let dir = tempdir();
        let tight = ToolContext::new(dir.clone()).with_bash_config(BashConfig {
            timeout: Duration::from_millis(200),
            ..BashConfig::default()
        });
        let start = std::time::Instant::now();
        let out = Bash
            .execute(json!({"command": "sleep 5"}), &tight)
            .await
            .unwrap();
        // The sleep would run for 5s if the kill never fired. We pick a 4s
        // ceiling because slow CI runners (cold cargo cache, fresh process,
        // SIGKILL reap latency) have been measured well above 2s while
        // still being far from a missed-kill case. The kill marker below
        // is the real correctness check; this assertion only guards
        // against the "we just waited for sleep to finish" regression.
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "timeout did not fire fast enough: {:?}",
            start.elapsed()
        );
        assert!(out.contains("[killed:"), "missing kill marker:\n{out}");
    }

    #[tokio::test]
    async fn bash_truncates_output_past_cap() {
        let dir = tempdir();
        let small = ToolContext::new(dir.clone()).with_bash_config(BashConfig {
            max_output_bytes: 64,
            ..BashConfig::default()
        });
        // Print 2 KiB of 'a' so we comfortably exceed the 64-byte cap
        // without depending on coreutils flags that differ across OSes.
        let out = Bash
            .execute(json!({"command": "printf 'a%.0s' $(seq 1 2048)"}), &small)
            .await
            .unwrap();
        assert!(
            out.contains("[truncated:"),
            "missing truncation marker:\n{out}"
        );
        assert!(out.contains("[exit] 0"));
    }

    #[tokio::test]
    async fn bash_background_returns_id() {
        let dir = tempdir();
        let out = Bash
            .execute(
                json!({"command": "echo bg-test", "run_in_background": true}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("[background]"));
        assert!(out.contains("id="));
        assert!(out.contains("metis-bg-"));
    }

    #[tokio::test]
    async fn bash_custom_timeout() {
        let dir = tempdir();
        let out = Bash
            .execute(json!({"command": "sleep 10", "timeout": 500}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("[killed:"));
    }

    #[tokio::test]
    async fn bash_scrubs_secret_env_vars() {
        let dir = tempdir();
        // Use a uniquely-named var so parallel tests can't race us. The
        // suffix `_API_KEY` is what triggers the scrubber.
        let key = "METIS_TEST_FAKE_API_KEY";
        // SAFETY: tests run in-process; we set then unset around the call.
        // The scrubber inspects std::env at execute() time.
        std::env::set_var(key, "supersecret");
        let out = Bash
            .execute(
                json!({"command": "env | grep METIS_TEST_FAKE_API_KEY || echo MISSING"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        std::env::remove_var(key);
        assert!(
            out.contains("MISSING"),
            "secret env var leaked into child:\n{out}"
        );
    }

    #[tokio::test]
    async fn bash_config_should_scrub_patterns() {
        let cfg = BashConfig::default();
        assert!(cfg.should_scrub("DEEPSEEK_API_KEY"));
        assert!(cfg.should_scrub("OPENAI_API_KEY"));
        assert!(cfg.should_scrub("ANTHROPIC_AUTH_TOKEN"));
        assert!(cfg.should_scrub("GITHUB_TOKEN"));
        assert!(cfg.should_scrub("DB_PASSWORD"));
        assert!(cfg.should_scrub("METIS_SESSION_ID"));
        assert!(!cfg.should_scrub("PATH"));
        assert!(!cfg.should_scrub("HOME"));
        assert!(!cfg.should_scrub("LANG"));

        let off = BashConfig {
            scrub_secret_env: false,
            ..BashConfig::default()
        };
        assert!(!off.should_scrub("OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn mcp_tool_advertises_runtime_name_and_description() {
        let info = aegis_mcp::McpToolInfo {
            name: "weather".to_string(),
            description: Some("get the forecast".to_string()),
            input_schema: Some(json!({"type": "object"})),
        };
        let tool = McpTool::new(
            info,
            std::sync::Arc::new(tokio::sync::Mutex::new(None::<aegis_mcp::McpServer>)),
        );
        // The trait now returns `&str`, so an MCP tool can hand back a
        // borrow into its own owned String — this is the whole reason
        // we relaxed the lifetime on `Tool::name`.
        assert_eq!(tool.name(), "weather");
        assert_eq!(tool.description(), "get the forecast");
        let spec = tool.spec();
        assert_eq!(spec.function.name, "weather");
    }

    #[tokio::test]
    async fn tool_spec_round_trips_through_registry() {
        let r = ToolRegistry::with_builtins();
        let specs = r.specs();
        // Default: 38 tools after CreateTask/UpdateTask removal — model
        // was abusing them as a chat scratchpad. ListTasks/EnterPlanMode
        // /ExitPlanMode stay. With `wasm`: +wasm_run +python_wasi.
        let expected = if cfg!(feature = "wasm") { 40 } else { 38 };
        assert_eq!(specs.len(), expected);
        let names: Vec<_> = specs.iter().map(|s| s.function.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"multi_edit"));
        assert!(names.contains(&"repo_map"));
        assert!(names.contains(&"parallel_agents"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"save_memory"));
        assert!(names.contains(&"semantic_memory_search"));
        assert!(names.contains(&"web_fetch"));
    }

    #[tokio::test]
    async fn strip_html_removes_tags_and_scripts() {
        let html = r#"<html><head><script>var x=1;</script><style>body{}</style></head>
        <body><h1>Title</h1><p>Hello <b>world</b></p></body></html>"#;
        let text = super::web::strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world"));
        assert!(!text.contains("<"));
        assert!(!text.contains("var x"));
        assert!(!text.contains("body{}"));
    }

    #[tokio::test]
    async fn strip_html_decodes_entities() {
        let html = "5 &lt; 10 &amp; 3 &gt; 1 &quot;ok&quot;";
        let text = super::web::strip_html(html);
        assert_eq!(text, r#"5 < 10 & 3 > 1 "ok""#);
    }

    #[tokio::test]
    async fn strip_html_plain_text_passthrough() {
        let text = "Just plain text, no HTML.";
        assert_eq!(super::web::strip_html(text), text);
    }

    #[tokio::test]
    async fn enter_plan_mode_sets_flag() {
        let dir = tempdir();
        let c = ctx(&dir);
        assert_eq!(*c.plan_state.lock().unwrap(), PlanState::Normal);
        EnterPlanMode.execute(json!({}), &c).await.unwrap();
        assert_eq!(*c.plan_state.lock().unwrap(), PlanState::Drafting);
    }

    #[tokio::test]
    async fn exit_plan_mode_clears_flag() {
        let dir = tempdir();
        let c = ctx(&dir);
        *c.plan_state.lock().unwrap() = PlanState::Drafting;
        ExitPlanMode.execute(json!({}), &c).await.unwrap();
        assert_eq!(*c.plan_state.lock().unwrap(), PlanState::Executing);
    }

    #[tokio::test]
    async fn exit_plan_without_enter_fails() {
        let dir = tempdir();
        let c = ctx(&dir);
        assert!(ExitPlanMode.execute(json!({}), &c).await.is_err());
    }

    #[tokio::test]
    async fn double_enter_is_noop() {
        let dir = tempdir();
        let c = ctx(&dir);
        EnterPlanMode.execute(json!({}), &c).await.unwrap();
        let msg = EnterPlanMode.execute(json!({}), &c).await.unwrap();
        assert!(msg.contains("Already"));
        assert_eq!(*c.plan_state.lock().unwrap(), PlanState::Drafting);
    }

    #[tokio::test]
    async fn plan_mode_allowed_list_is_read_only() {
        // Mutating tools must NOT be in the allowed list.
        for name in [
            "edit_file",
            "write_file",
            "bash",
            "save_memory",
            "delete_memory",
            "create_task",
            "update_task",
        ] {
            assert!(
                !PLAN_MODE_ALLOWED.contains(&name),
                "{name} should not be allowed in plan mode"
            );
        }
        // Read-only tools must be present.
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_tasks",
            "list_memories",
            "read_memory",
            "web_fetch",
            "ask_user_question",
            "enter_plan_mode",
            "exit_plan_mode",
            "tool_search",
        ] {
            assert!(
                PLAN_MODE_ALLOWED.contains(&name),
                "{name} should be allowed in plan mode"
            );
        }
    }

    #[tokio::test]
    async fn tool_search_keyword() {
        let dir = tempdir();
        let out = ToolSearch
            .execute(json!({"query": "file read"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("read_file"));
    }

    #[tokio::test]
    async fn tool_search_select() {
        let dir = tempdir();
        let out = ToolSearch
            .execute(json!({"query": "select:bash,grep"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("bash"));
        assert!(out.contains("grep"));
        assert!(!out.contains("edit_file"));
    }

    #[tokio::test]
    async fn tool_search_no_match() {
        let dir = tempdir();
        let out = ToolSearch
            .execute(json!({"query": "xyznonexistent"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("No tools matched"));
    }

    #[tokio::test]
    async fn bash_rerun_alias_works() {
        let dir = tempdir();
        let c = ctx(&dir);
        // Run a command to populate history
        let out1 = Bash
            .execute(json!({"command": "echo hello"}), &c)
            .await
            .unwrap();
        assert!(out1.contains("[rerun: b1]"));

        // Rerun it
        let out2 = Bash.execute(json!({"rerun": "b1"}), &c).await.unwrap();
        assert!(out2.contains("hello"));
        assert!(out2.contains("[rerun: b2]"));
    }

    #[tokio::test]
    async fn bash_rerun_invalid_alias_fails() {
        let dir = tempdir();
        let err = Bash
            .execute(json!({"rerun": "b99"}), &ctx(&dir))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn read_file_notebook_renders_cells() {
        let dir = tempdir();
        let nb = serde_json::json!({
            "cells": [
                {
                    "cell_type": "code",
                    "source": ["import os\n", "print('hello')"],
                    "outputs": [
                        {
                            "output_type": "stream",
                            "name": "stdout",
                            "text": ["hello\n"]
                        }
                    ]
                },
                {
                    "cell_type": "markdown",
                    "source": "# Title\nSome text"
                }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 2
        });
        fs::write(dir.join("test.ipynb"), nb.to_string()).unwrap();
        let out = ReadFile
            .execute(json!({"path": "test.ipynb"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("[Notebook:"));
        assert!(out.contains("2 cells"));
        assert!(out.contains("Cell 1 (code)"));
        assert!(out.contains("import os"));
        assert!(out.contains("[output: stream]"));
        assert!(out.contains("hello"));
        assert!(out.contains("Cell 2 (markdown)"));
        assert!(out.contains("# Title"));
    }

    #[tokio::test]
    async fn read_file_image_returns_metadata() {
        let dir = tempdir();
        fs::write(dir.join("photo.png"), b"fake png data").unwrap();
        let out = ReadFile
            .execute(json!({"path": "photo.png"}), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.contains("[Image:"));
        assert!(out.contains("png"));
        assert!(out.contains("bytes"));
    }

    #[tokio::test]
    async fn parse_page_range_single() {
        let (s, e) = super::fs::parse_page_range("3", 10).unwrap();
        assert_eq!((s, e), (3, 3));
    }

    #[tokio::test]
    async fn parse_page_range_span() {
        let (s, e) = super::fs::parse_page_range("2-5", 10).unwrap();
        assert_eq!((s, e), (2, 5));
    }

    #[tokio::test]
    async fn parse_page_range_clamps_to_total() {
        let (s, e) = super::fs::parse_page_range("8-20", 10).unwrap();
        assert_eq!((s, e), (8, 10));
    }

    #[tokio::test]
    async fn parse_page_range_too_many_fails() {
        let err = super::fs::parse_page_range("1-25", 30);
        assert!(err.is_err());
    }

    // ---------------------------------------------------------------
    // NotebookEdit
    // ---------------------------------------------------------------

    fn sample_notebook() -> serde_json::Value {
        json!({
            "cells": [
                {
                    "cell_type": "code",
                    "metadata": {},
                    "source": ["import os\n", "print('hello')"],
                    "outputs": [],
                    "execution_count": null
                },
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": ["# Title"]
                }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 2
        })
    }

    #[tokio::test]
    async fn notebook_edit_cell_replaces_source() {
        let dir = tempdir();
        fs::write(dir.join("nb.ipynb"), sample_notebook().to_string()).unwrap();
        let out = NotebookEdit
            .execute(
                json!({
                    "path": "nb.ipynb",
                    "command": "edit_cell",
                    "cell_index": 1,
                    "new_source": "import sys\nprint('bye')"
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("Edited cell 1"));
        // Verify written content
        let nb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("nb.ipynb")).unwrap()).unwrap();
        let src = super::fs::cell_source(&nb["cells"][0]);
        assert!(src.contains("import sys"));
        assert!(src.contains("print('bye')"));
    }

    #[tokio::test]
    async fn notebook_insert_cell() {
        let dir = tempdir();
        fs::write(dir.join("nb.ipynb"), sample_notebook().to_string()).unwrap();
        let out = NotebookEdit
            .execute(
                json!({
                    "path": "nb.ipynb",
                    "command": "insert_cell",
                    "cell_index": 2,
                    "new_source": "x = 42",
                    "cell_type": "code"
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("Inserted code cell at position 2"));
        let nb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("nb.ipynb")).unwrap()).unwrap();
        let cells = nb["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1]["cell_type"], "code");
        let src = super::fs::cell_source(&cells[1]);
        assert!(src.contains("x = 42"));
    }

    #[tokio::test]
    async fn notebook_delete_cell() {
        let dir = tempdir();
        fs::write(dir.join("nb.ipynb"), sample_notebook().to_string()).unwrap();
        let out = NotebookEdit
            .execute(
                json!({
                    "path": "nb.ipynb",
                    "command": "delete_cell",
                    "cell_index": 2
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("Deleted markdown cell 2"));
        let nb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("nb.ipynb")).unwrap()).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn notebook_edit_out_of_range() {
        let dir = tempdir();
        fs::write(dir.join("nb.ipynb"), sample_notebook().to_string()).unwrap();
        let err = NotebookEdit
            .execute(
                json!({
                    "path": "nb.ipynb",
                    "command": "edit_cell",
                    "cell_index": 99,
                    "new_source": "nope"
                }),
                &ctx(&dir),
            )
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn notebook_edit_rejects_non_ipynb() {
        let dir = tempdir();
        fs::write(dir.join("test.py"), "x = 1").unwrap();
        let err = NotebookEdit
            .execute(
                json!({
                    "path": "test.py",
                    "command": "edit_cell",
                    "cell_index": 1,
                    "new_source": "y = 2"
                }),
                &ctx(&dir),
            )
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn notebook_insert_at_end() {
        let dir = tempdir();
        fs::write(dir.join("nb.ipynb"), sample_notebook().to_string()).unwrap();
        // Insert at position 3 = after all existing cells (2 cells → valid range 1..=3)
        let out = NotebookEdit
            .execute(
                json!({
                    "path": "nb.ipynb",
                    "command": "insert_cell",
                    "cell_index": 3,
                    "new_source": "# Footer",
                    "cell_type": "markdown"
                }),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("Inserted markdown cell at position 3"));
        let nb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("nb.ipynb")).unwrap()).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn notebook_source_to_lines_format() {
        let lines = super::notebook::source_to_lines("line1\nline2\nline3");
        let arr = lines.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], "line1\n");
        assert_eq!(arr[1], "line2\n");
        assert_eq!(arr[2], "line3");
    }

    // ---------------------------------------------------------------
    // WebSearch (DDG HTML parsing — no network)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn parse_ddg_results_extracts_entries() {
        // Minimal DDG-like HTML structure
        let html = r#"
        <div class="result results_links result--more">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc">
            Example <b>Page</b>
          </a>
          <a class="result__snippet" href="...">
            This is a snippet about the page.
          </a>
        </div>
        <div class="result results_links result--more">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fother.org&rut=def">
            Other Site
          </a>
          <a class="result__snippet" href="...">
            Another snippet here.
          </a>
        </div>
        "#;
        let results = super::web::parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Example Page");
        assert_eq!(results[0].url, "https://example.com/page");
        assert!(results[0].snippet.contains("snippet about the page"));
        assert_eq!(results[1].title, "Other Site");
        assert_eq!(results[1].url, "https://other.org");
    }

    #[tokio::test]
    async fn parse_ddg_results_respects_max() {
        let html = r#"
        <div class="result results_links x">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.com&rut=1">A</a>
          <a class="result__snippet" href="">snip a</a>
        </div>
        <div class="result results_links y">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fb.com&rut=2">B</a>
          <a class="result__snippet" href="">snip b</a>
        </div>
        <div class="result results_links z">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fc.com&rut=3">C</a>
          <a class="result__snippet" href="">snip c</a>
        </div>
        "#;
        let results = super::web::parse_ddg_results(html, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "A");
        assert_eq!(results[1].title, "B");
    }

    #[tokio::test]
    async fn parse_ddg_empty_html_returns_empty() {
        let results = super::web::parse_ddg_results("<html><body>nothing</body></html>", 10);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn decode_ddg_url_extracts_target() {
        let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2Flearn&rut=xxx";
        assert_eq!(
            super::web::decode_ddg_url(raw),
            "https://rust-lang.org/learn"
        );
    }

    #[tokio::test]
    async fn decode_ddg_url_passthrough_for_direct() {
        assert_eq!(
            super::web::decode_ddg_url("https://example.com"),
            "https://example.com"
        );
    }

    #[tokio::test]
    async fn strip_inline_html_removes_tags() {
        assert_eq!(
            super::web::strip_inline_html("Hello <b>world</b> &amp; friends"),
            "Hello world & friends"
        );
    }

    // ---------------------------------------------------------------
    // EnterWorktree / ExitWorktree
    // ---------------------------------------------------------------

    /// Helper: create a temp dir with a git repo initialized.
    fn git_tempdir() -> PathBuf {
        let dir = tempdir();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        // Configure user for CI environments where no global git config exists.
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn enter_worktree_creates_worktree() {
        let dir = git_tempdir();
        let c = ctx(&dir);
        let out = EnterWorktree
            .execute(json!({"branch": "test-wt"}), &c)
            .await
            .unwrap();
        assert!(out.contains("test-wt"));
        assert!(out.contains("Worktree created"));
        // Verify worktree state is set
        assert!(c.worktree.lock().unwrap().is_some());
        // Verify effective_root changed
        assert_ne!(c.effective_root(), dir);
        // Cleanup
        ExitWorktree.execute(json!({}), &c).await.unwrap();
    }

    #[tokio::test]
    async fn exit_worktree_without_changes_removes_it() {
        let dir = git_tempdir();
        let c = ctx(&dir);
        EnterWorktree
            .execute(json!({"branch": "clean-wt"}), &c)
            .await
            .unwrap();
        let wt_path = c
            .worktree
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .worktree_path
            .clone();
        let out = ExitWorktree.execute(json!({}), &c).await.unwrap();
        assert!(out.contains("removed"));
        assert!(c.worktree.lock().unwrap().is_none());
        // Worktree dir should be gone
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn exit_worktree_with_changes_preserves_it() {
        let dir = git_tempdir();
        let c = ctx(&dir);
        EnterWorktree
            .execute(json!({"branch": "dirty-wt"}), &c)
            .await
            .unwrap();
        let wt_path = c
            .worktree
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .worktree_path
            .clone();
        // Create a file in the worktree
        fs::write(wt_path.join("new.txt"), "hello").unwrap();
        let out = ExitWorktree.execute(json!({}), &c).await.unwrap();
        assert!(out.contains("preserved"));
        assert!(wt_path.exists());
        // Manual cleanup
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", wt_path.to_str().unwrap()])
            .current_dir(&dir)
            .output();
    }

    #[tokio::test]
    async fn double_enter_worktree_fails() {
        let dir = git_tempdir();
        let c = ctx(&dir);
        EnterWorktree.execute(json!({}), &c).await.unwrap();
        let err = EnterWorktree.execute(json!({}), &c).await;
        assert!(err.is_err());
        // Cleanup
        ExitWorktree.execute(json!({}), &c).await.unwrap();
    }

    #[tokio::test]
    async fn exit_without_enter_fails() {
        let dir = tempdir();
        let c = ctx(&dir);
        let err = ExitWorktree.execute(json!({}), &c).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn effective_root_returns_worktree_when_active() {
        let dir = git_tempdir();
        let c = ctx(&dir);
        assert_eq!(c.effective_root(), dir);
        EnterWorktree.execute(json!({}), &c).await.unwrap();
        let eff = c.effective_root();
        assert_ne!(eff, dir);
        assert!(eff.exists());
        ExitWorktree.execute(json!({}), &c).await.unwrap();
        assert_eq!(c.effective_root(), dir);
    }

    // ---------------------------------------------------------------
    // Cron tools
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn cron_create_and_list() {
        let dir = tempdir();
        let c = ctx(&dir);
        let out = CronCreate
            .execute(
                json!({"schedule": "0 9 * * 1-5", "command": "cargo test", "description": "daily tests"}),
                &c,
            )
            .await.unwrap();
        assert!(out.contains("Created cron #1"));
        let list = CronList.execute(json!({}), &c).await.unwrap();
        assert!(list.contains("0 9 * * 1-5"));
        assert!(list.contains("cargo test"));
        assert!(list.contains("daily tests"));
    }

    #[tokio::test]
    async fn cron_delete() {
        let dir = tempdir();
        let c = ctx(&dir);
        CronCreate
            .execute(json!({"schedule": "0 * * * *", "command": "echo hi"}), &c)
            .await
            .unwrap();
        let out = CronDelete.execute(json!({"id": 1}), &c).await.unwrap();
        assert!(out.contains("Deleted cron #1"));
        let list = CronList.execute(json!({}), &c).await.unwrap();
        assert!(list.contains("no cron"));
    }

    #[tokio::test]
    async fn cron_delete_nonexistent_fails() {
        let dir = tempdir();
        let c = ctx(&dir);
        let err = CronDelete.execute(json!({"id": 99}), &c).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn cron_create_validates_schedule() {
        let dir = tempdir();
        let c = ctx(&dir);
        // Only 3 fields — should fail
        let err = CronCreate
            .execute(json!({"schedule": "0 9 *", "command": "echo"}), &c)
            .await;
        assert!(err.is_err());
    }

    // ---------------------------------------------------------------
    // Unified diff
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn unified_diff_shows_changes() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nmodified\nline3\n";
        let diff = super::fs::unified_diff(old, new, "test.txt");
        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+modified"));
    }

    #[tokio::test]
    async fn unified_diff_identical_returns_empty() {
        let text = "same\ntext\n";
        let diff = super::fs::unified_diff(text, text, "f.txt");
        assert!(diff.is_empty());
    }

    #[tokio::test]
    async fn unified_diff_addition() {
        let old = "a\nb\n";
        let new = "a\nx\nb\n";
        let diff = super::fs::unified_diff(old, new, "f.txt");
        assert!(diff.contains("+x"));
    }

    #[tokio::test]
    async fn unified_diff_deletion() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let diff = super::fs::unified_diff(old, new, "f.txt");
        assert!(diff.contains("-b"));
    }

    #[tokio::test]
    async fn edit_file_returns_diff() {
        let dir = tempdir();
        fs::write(dir.join("f.txt"), "hello world\n").unwrap();
        let out = EditFile
            .execute(
                json!({"path": "f.txt", "old_string": "hello", "new_string": "goodbye"}),
                &ctx(&dir),
            )
            .await
            .unwrap();
        assert!(out.contains("edited f.txt"));
        assert!(out.contains("-hello world"));
        assert!(out.contains("+goodbye world"));
    }

    #[tokio::test]
    async fn lcs_lines_basic() {
        let old = vec!["a", "b", "c"];
        let new = vec!["a", "x", "c"];
        let matches = super::fs::lcs_lines(&old, &new);
        assert_eq!(matches, vec![(0, 0), (2, 2)]);
    }

    // ---------------------------------------------------------------
    // LSP (formatting tests — no actual server needed)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn lsp_server_for_known_languages() {
        assert!(super::code::lsp_server_for("rust").is_some());
        assert!(super::code::lsp_server_for("python").is_some());
        assert!(super::code::lsp_server_for("typescript").is_some());
        assert!(super::code::lsp_server_for("go").is_some());
        assert!(super::code::lsp_server_for("unknown_lang").is_none());
    }

    #[tokio::test]
    async fn format_location_response_array() {
        let root = PathBuf::from("/project");
        let resp = json!({
            "result": [
                {
                    "uri": "file:///project/src/main.rs",
                    "range": {
                        "start": {"line": 9, "character": 4},
                        "end": {"line": 9, "character": 10}
                    }
                }
            ]
        });
        let out = super::code::format_location_response(&resp, &root);
        assert!(out.contains("src/main.rs:10:5"));
    }

    #[tokio::test]
    async fn format_location_response_null() {
        let root = PathBuf::from("/project");
        let resp = json!({"result": null});
        let out = super::code::format_location_response(&resp, &root);
        assert!(out.contains("No results"));
    }

    #[tokio::test]
    async fn format_hover_response_string() {
        let resp = json!({"result": {"contents": "fn main()"}});
        let out = super::code::format_hover_response(&resp);
        assert!(out.contains("fn main()"));
    }

    #[tokio::test]
    async fn format_hover_response_markup() {
        let resp = json!({"result": {"contents": {"kind": "markdown", "value": "```rust\nfn foo()\n```"}}});
        let out = super::code::format_hover_response(&resp);
        assert!(out.contains("fn foo()"));
    }

    #[tokio::test]
    async fn format_diagnostics_output() {
        let root = PathBuf::from("/project");
        let params = json!({
            "uri": "file:///project/src/lib.rs",
            "diagnostics": [
                {
                    "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 10}},
                    "severity": 1,
                    "message": "expected `;`"
                }
            ]
        });
        let out = super::code::format_diagnostics(&params, &root);
        assert!(out.contains("src/lib.rs:5"));
        assert!(out.contains("error"));
        assert!(out.contains("expected `;`"));
    }

    #[tokio::test]
    async fn lsp_rejects_unknown_language_without_server() {
        let dir = tempdir();
        fs::write(dir.join("test.xyz"), "content").unwrap();
        let err = Lsp.execute(
            json!({"command": "hover", "path": "test.xyz", "line": 1, "column": 1, "language": "xyz"}),
            &ctx(&dir),
        ).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn bash_sandbox_exec_allows_reads_blocks_outside_writes() {
        let dir = tempdir();
        fs::write(dir.join("hello.txt"), "hi").unwrap();
        let mut c = ctx(&dir);
        c.bash.sandbox = SandboxMode::SandboxExec;

        // Reading inside workspace should work.
        let out = Bash
            .execute(json!({"command": "cat hello.txt"}), &c)
            .await
            .unwrap();
        assert!(out.contains("hi"), "sandbox should allow reads: {out}");

        // Writing inside workspace should work.
        let out = Bash
            .execute(
                json!({"command": "echo ok > inside.txt && cat inside.txt"}),
                &c,
            )
            .await
            .unwrap();
        assert!(
            out.contains("ok"),
            "sandbox should allow workspace writes: {out}"
        );

        // Writing outside workspace should fail.
        // sandbox-exec allows /tmp writes (by SBPL profile), so test a truly
        // outside path instead.
        let out2 = Bash
            .execute(
                json!({"command": "touch /usr/local/metis_sandbox_probe 2>&1 || echo DENIED"}),
                &c,
            )
            .await
            .unwrap();
        assert!(
            out2.contains("DENIED") || out2.contains("denied") || out2.contains("not permitted"),
            "sandbox should block writes outside workspace: {out2}"
        );
    }

    #[tokio::test]
    async fn sandbox_mode_enum_default_is_none() {
        let c = ctx(&tempdir());
        assert!(matches!(c.bash.sandbox, SandboxMode::None));
    }

    #[tokio::test]
    async fn screenshot_tool_has_correct_spec() {
        let tool = Screenshot;
        assert_eq!(tool.name(), "screenshot");
        assert!(tool.description().contains("screenshot"));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["window_title"].is_object());
        assert!(schema["properties"]["region"].is_object());
        assert!(schema["properties"]["delay"].is_object());
    }

    #[tokio::test]
    async fn screenshot_registered_in_builtins() {
        let reg = ToolRegistry::with_builtins();
        assert!(
            reg.get("screenshot").is_some(),
            "screenshot should be in builtins"
        );
    }

    // ---- parallel_agents tool tests ----

    #[tokio::test]
    async fn parallel_agents_spec_has_correct_schema() {
        let tool = ParallelAgentsTool;
        assert_eq!(tool.name(), "parallel_agents");
        assert!(tool.description().contains("concurrently"));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["agents"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
        assert_eq!(schema["required"], json!(["agents"]));
        // agents items must have description and prompt required
        let items = &schema["properties"]["agents"]["items"];
        assert_eq!(items["required"], json!(["description", "prompt"]));
    }

    #[tokio::test]
    async fn parallel_agents_registered_in_builtins() {
        let reg = ToolRegistry::with_builtins();
        assert!(
            reg.get("parallel_agents").is_some(),
            "parallel_agents should be in builtins"
        );
    }

    #[tokio::test]
    async fn parallel_agents_no_spawner_returns_error() {
        let dir = tempdir();
        let ctx = ctx(&dir);
        let result = ParallelAgentsTool
            .execute(
                json!({
                    "agents": [
                        {"description": "test", "prompt": "hello"}
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("agent spawner not configured"));
    }

    #[tokio::test]
    async fn parallel_agents_empty_agents_returns_error() {
        let dir = tempdir();
        let ctx = ctx(&dir);
        let result = ParallelAgentsTool
            .execute(json!({"agents": []}), &ctx)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must not be empty"));
    }

    #[tokio::test]
    async fn parallel_agents_runs_concurrently_and_preserves_order() {
        let dir = tempdir();
        let mut ctx = ctx(&dir);
        // Spawner that echoes the description back as the result
        ctx.agent_spawner = Some(Arc::new(|req: AgentSpawnRequest| {
            Ok(format!("result for: {}", req.description))
        }));
        let result = ParallelAgentsTool
            .execute(
                json!({
                    "agents": [
                        {"description": "task-A", "prompt": "do A"},
                        {"description": "task-B", "prompt": "do B"},
                        {"description": "task-C", "prompt": "do C"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        // Results must be in submission order
        let pos_a = result.find("task-A").unwrap();
        let pos_b = result.find("task-B").unwrap();
        let pos_c = result.find("task-C").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
        assert!(result.contains("result for: task-A"));
        assert!(result.contains("result for: task-B"));
        assert!(result.contains("result for: task-C"));
    }

    #[tokio::test]
    async fn parallel_agents_handles_per_agent_errors() {
        let dir = tempdir();
        let mut ctx = ctx(&dir);
        // Spawner that fails for task-B
        ctx.agent_spawner = Some(Arc::new(|req: AgentSpawnRequest| {
            if req.description == "task-B" {
                Err("deliberate failure".to_string())
            } else {
                Ok(format!("ok: {}", req.description))
            }
        }));
        let result = ParallelAgentsTool
            .execute(
                json!({
                    "agents": [
                        {"description": "task-A", "prompt": "do A"},
                        {"description": "task-B", "prompt": "do B"},
                        {"description": "task-C", "prompt": "do C"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        // task-A and task-C should succeed
        assert!(result.contains("ok: task-A"));
        assert!(result.contains("ok: task-C"));
        // task-B should show an error
        assert!(result.contains("**Error:**"));
        assert!(result.contains("deliberate failure"));
    }

    // ---------------------------------------------------------------
    // M8: subagent CC-style brief injection (commit 339869b)
    // ---------------------------------------------------------------
    // The agent + parallel_agents tools must wrap the user prompt with a
    // standard brief header before handing it to the spawner. We assert this
    // at the spawn boundary by intercepting AgentSpawnRequest.

    #[tokio::test]
    async fn agent_tool_injects_subagent_brief_into_prompt() {
        let dir = tempdir();
        let mut ctx = ctx(&dir);
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_w = captured.clone();
        ctx.agent_spawner = Some(Arc::new(move |req: AgentSpawnRequest| {
            *captured_w.lock().unwrap() = Some(req.prompt.clone());
            Ok("ok".to_string())
        }));
        let _ = AgentTool
            .execute(
                json!({"description": "fix the bug", "prompt": "do the work"}),
                &ctx,
            )
            .await
            .unwrap();
        let got = captured.lock().unwrap().clone().expect("spawner called");
        assert!(got.starts_with("[Subagent brief]\n"), "brief header missing: {got:?}");
        assert!(got.contains("Task: fix the bug"), "description not in brief");
        assert!(got.contains("Do NOT ask clarifying questions"), "no-questions clause missing");
        assert!(got.contains("[End brief]"), "brief terminator missing");
        assert!(got.contains("\n\ndo the work"), "original prompt not appended after brief");
    }

    #[tokio::test]
    async fn parallel_agents_tool_injects_brief_into_each_prompt() {
        let dir = tempdir();
        let mut ctx = ctx(&dir);
        let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let prompts_w = prompts.clone();
        ctx.agent_spawner = Some(Arc::new(move |req: AgentSpawnRequest| {
            prompts_w.lock().unwrap().push(req.prompt.clone());
            Ok(format!("ok: {}", req.description))
        }));
        let _ = ParallelAgentsTool
            .execute(
                json!({
                    "agents": [
                        {"description": "task-A", "prompt": "do A"},
                        {"description": "task-B", "prompt": "do B"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        let got = prompts.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "both agents must have spawned");
        for p in &got {
            assert!(p.starts_with("[Subagent brief]\n"), "brief missing in: {p:?}");
            assert!(p.contains("[End brief]"), "terminator missing");
        }
        assert!(got[0].contains("Task: task-A"));
        assert!(got[0].contains("\n\ndo A"));
        assert!(got[1].contains("Task: task-B"));
        assert!(got[1].contains("\n\ndo B"));
    }

    // ---------------------------------------------------------------
    // ctx-feature: ToolRegistry sandbox integration
    // ---------------------------------------------------------------

    #[cfg(feature = "ctx")]
    #[tokio::test]
    async fn enable_sandbox_preserves_tool_count_and_names() {
        let dir = tempdir();
        let mut reg = ToolRegistry::with_builtins();
        let original_len = reg.len();
        let original_names: Vec<String> = reg.tools.read().unwrap().iter().map(|t| t.name().to_string()).collect();

        let _handles = reg
            .enable_sandbox(&dir, crate::sandbox_exec::SandboxConfig::default())
            .unwrap();

        assert_eq!(reg.len(), original_len, "tool count must be preserved");
        let new_names: Vec<String> = reg.tools.read().unwrap().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(new_names, original_names, "tool names must be preserved");
        // Specs must still resolve — agent loop relies on this.
        let specs = reg.specs();
        assert_eq!(specs.len(), original_len);
    }

    #[cfg(feature = "ctx")]
    #[tokio::test]
    async fn enable_sandbox_stashes_large_bash_output() {
        let dir = tempdir();
        // Plant a 6 KB file we'll cat through bash to produce >4 KB output.
        let big_path = dir.join("big.txt");
        fs::write(&big_path, "x".repeat(6 * 1024)).unwrap();

        let mut reg = ToolRegistry::with_builtins();
        let (store, index) = reg
            .enable_sandbox(&dir, crate::sandbox_exec::SandboxConfig::default())
            .unwrap();

        let bash = reg.get("bash").expect("bash registered");
        let out = bash
            .execute(
                json!({"command": format!("cat {}", big_path.display())}),
                &ctx(&dir),
            )
            .await
            .unwrap();

        assert!(
            out.contains("[stashed: ctx://"),
            "bash output should be stashed; got: {}",
            &out[..out.len().min(200)]
        );
        assert_eq!(store.iter_ids().unwrap().len(), 1);
        let hits = index.search("cat", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tool, "bash");
    }
}
