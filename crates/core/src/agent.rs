//! Multi-turn agent loop with tool execution.
//!
//! The loop is the standard OpenAI tool-calling cycle:
//!
//! 1. Send the conversation (plus the registered tool specs) to the
//!    provider.
//! 2. Take the assistant message back and append it to the conversation.
//! 3. If the assistant emitted `tool_calls`, run each one through the
//!    [`ToolRegistry`], append the results as `tool` messages, and go
//!    back to step 1.
//! 4. If the assistant emitted plain content with no tool calls, return
//!    it as the final answer.
//!
//! Design notes:
//!
//! * **Hard turn cap.** Models can loop on bad tool output forever; the
//!   agent enforces [`AgentConfig::max_turns`] and surfaces a clear error
//!   instead of burning tokens. The default of 100 fits real multi-file
//!   refactors with iterative edit + lint + commit cycles while still
//!   bounding runaway costs. The repeat detector and consecutive-error
//!   escalation kick in long before this cap matters in practice.
//! * **Parallel tool execution.** When the model emits multiple tool
//!   calls in a single turn they are executed concurrently via
//!   `tokio::task::JoinSet` (tool execution is async). Results are
//!   collected back in input order so the transcript remains deterministic.
//! * **Transient error retry.** Provider calls are retried up to 3 times
//!   on rate-limit (429) and server errors (5xx) with exponential backoff.
//! * **Tool errors are conversational.** When a tool fails we still send
//!   a `tool` message back to the model — with the error as the content
//!   — so it can self-correct. Hard infrastructure failures (HTTP, JSON
//!   decode) bubble up as [`AgentError`] and abort the run.
//! * **Usage is summed across all turns.** Each provider response gets
//!   added into a running [`UsageSnapshot`] so the cost footer reflects
//!   the entire prompt, not just the final turn.
//! * **Permission prompting is out of scope here.** The loop runs every
//!   tool call unconditionally; Session 3 will introduce a `Permission`
//!   trait that wraps the registry without changing this loop.

use std::sync::Arc;

use aegis_api::{
    autotune::autotune, ApiError, ChatMessage, ChatProvider, ChatRequest, ChatResponse,
    ContentBlock, DeepSeekClient, OpenAICompatClient, StreamEvent,
};
use thiserror::Error;

use crate::compaction::{
    llm_summarizer, maybe_compact_with, maybe_micro_compact, CompactionConfig,
};
use crate::cost::{ModelPricing, UsageSnapshot};
use crate::hooks::{self, HookEvent};
use crate::permission::{Permission, PermissionDecision};
use crate::session::SessionStore;
use crate::tools::{ToolContext, ToolRegistry};

/// User input for a single agent turn. Plain text or multimodal (text + images).
#[derive(Debug, Clone)]
pub enum UserInput {
    /// Plain text prompt.
    Text(String),
    /// Multimodal prompt with text and/or images/documents.
    Multimodal(Vec<ContentBlock>),
}

impl UserInput {
    /// Convenience: build a multimodal input from text + image paths.
    /// Each path is base64-encoded. Unsupported extensions are skipped.
    pub fn with_images(
        text: impl Into<String>,
        paths: &[std::path::PathBuf],
    ) -> std::io::Result<Self> {
        use base64::Engine;
        let mut blocks = vec![ContentBlock::Text { text: text.into() }];
        for path in paths {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let media_type = match ext.to_lowercase().as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                _ => continue,
            };
            let data = std::fs::read(path)?;
            if data.len() > 20 * 1024 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: exceeds 20 MB limit", path.display()),
                ));
            }
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            blocks.push(ContentBlock::Image {
                media_type: media_type.to_string(),
                data: b64,
            });
        }
        Ok(Self::Multimodal(blocks))
    }

    /// Extract the text portion for hooks, logging, etc.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(s) => s,
            Self::Multimodal(blocks) => blocks
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        }
    }

    /// Convert into a ChatMessage.
    fn into_message(self) -> ChatMessage {
        match self {
            Self::Text(s) => ChatMessage::user(s),
            Self::Multimodal(blocks) => ChatMessage::user_multimodal(blocks),
        }
    }
}

impl<S: Into<String>> From<S> for UserInput {
    fn from(s: S) -> Self {
        Self::Text(s.into())
    }
}

/// Configuration for a single agent run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Maximum number of provider round-trips before the loop aborts.
    /// Each round-trip is one assistant message; tool execution between
    /// turns does not count.
    pub max_turns: usize,
    /// Context-window / compaction policy. Defaults to a generous
    /// 64k-token window with a 70% trigger ratio — enough headroom for a
    /// deepseek-chat session without tripping the model's hard limit.
    pub compaction: CompactionConfig,
    /// When true, compaction uses an LLM call to summarize dropped
    /// messages instead of a static placeholder. Costs extra tokens
    /// but preserves much more context. Default: false.
    pub smart_compaction: bool,
    /// Enable extended thinking / "overthink" mode. The model shows
    /// its chain-of-thought before the final answer.
    pub thinking: bool,
    /// Token budget for thinking. Default 10000.
    pub thinking_budget: u32,
    /// Auto-commit edits to git after each edit_file/write_file/multi_edit.
    pub auto_commit: bool,
    /// Lint command to run after file edits. If set, lint output on failure
    /// is appended to the tool result so the model can auto-fix.
    pub lint_command: Option<String>,
    /// Maximum number of lint-fail → fix cycles before giving up and letting
    /// the model continue without lint enforcement. Default: 3.
    pub lint_max_retries: u8,
    /// When true and `temperature` is None, auto-classify each user message
    /// and apply optimal sampling parameters (temperature + top_p).
    pub autotune: bool,
    /// Hard cost ceiling in USD. The agent aborts with [`AgentError::BudgetExceeded`]
    /// once accumulated spend exceeds this value. `None` means no ceiling.
    pub max_cost_usd: Option<f64>,
    /// Auto-run LLM-based memory extraction on session exit. Default: true.
    pub auto_memory: bool,
    /// Minimum user turns before auto-memory runs. Default: 3.
    pub auto_memory_min_turns: usize,
    /// Inject a context-hint system message before the first user turn
    /// identifying files likely relevant to the task (TF-IDF + import graph).
    /// Only fires when a workspace root is set and the task has ≥3 words.
    /// Default: true.
    pub context_priming: bool,
    /// Inject a mandatory task-tracking reminder before every new user turn
    /// when task tools (`create_task` / `update_task`) are registered.
    /// Forces every model — regardless of quality — to plan with tasks before
    /// acting. Mirrors Claude Code's built-in task behaviour.
    /// Default: true.
    pub task_nudge: bool,
    /// Per-turn skill injection — prepended to system prompt for one turn only.
    /// Set by the auto-skill classifier; cleared after each turn.
    pub extra_system: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "deepseek-chat".to_string(),
            system_prompt: None,
            temperature: None,
            max_tokens: None,
            max_turns: 100,
            compaction: CompactionConfig::default(),
            smart_compaction: false,
            thinking: false,
            thinking_budget: 10_000,
            auto_commit: false,
            lint_command: None,
            lint_max_retries: 3,
            autotune: false,
            max_cost_usd: None,
            auto_memory: true,
            auto_memory_min_turns: 3,
            context_priming: true,
            task_nudge: false,
            extra_system: None,
        }
    }
}

/// Final output of an agent run.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub final_text: String,
    pub usage: UsageSnapshot,
    pub turns: usize,
    /// Full conversation including system, user, assistant, and tool
    /// messages — useful for session persistence and debugging.
    pub transcript: Vec<ChatMessage>,
}

/// Errors that abort an agent run. Per-tool failures are *not* in here;
/// those are handed back to the model as tool messages.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider call failed: {0}")]
    Api(#[from] ApiError),
    #[error("provider returned no choices")]
    NoChoices,
    #[error("agent exceeded max turns ({0})")]
    MaxTurns(usize),
    #[error("model emitted invalid tool call arguments: {0}")]
    BadToolArgs(String),
    #[error("session store error: {0}")]
    Session(#[from] crate::session::SessionError),
    /// Configuration was invalid before the loop ever started. Added
    /// in Session 22 so misconfigurations like `max_turns = 0` don't
    /// have to share an error variant with the legitimate "loop hit
    /// its turn cap" outcome.
    #[error("invalid agent config: {0}")]
    Config(String),
    #[error("cost ceiling exceeded: spent ${spent:.4} (limit ${limit:.4})")]
    BudgetExceeded { spent: f64, limit: f64 },
    /// The model issued the same tool call 3 turns in a row without any
    /// change. Happens when a tool returns the same (usually empty or
    /// error) result and the model keeps retrying — classic stuck loop.
    /// Bail out so the user can intervene instead of burning turns.
    /// Added after a real incident where Metis ran a broken BeautifulSoup
    /// scraper that produced 0 output and repeated the same cycle ~50
    /// times before the user killed it.
    #[error("agent stuck in tool-call loop (turn {turn}): {signature}")]
    LoopDetected { turn: usize, signature: String },
    /// The model emitted near-identical assistant text 3 turns in a row.
    /// Happens with reasoning models (DeepSeek V4, Qwen, Kimi-thinking)
    /// when they can't make progress on a task: they fall back to a
    /// placeholder paragraph ("we need to try a different approach...")
    /// and re-emit it every turn because their own prior output looks
    /// like a valid opening line. The tool-call signature guard misses
    /// this because the bash/web_search calls underneath each turn
    /// have different arguments — only the *narration* repeats. Bail
    /// so the user can swap models or rephrase instead of burning the
    /// turn cap on the same paragraph.
    /// Real incident: GoFile-bypass task on a NIM-routed Qwen run, the
    /// same "GoFile bypass için farklı bir yöntem deneyelim" paragraph
    /// emitted 7+ times across a 211s 10-bash-call streak.
    #[error("agent stuck in text-repetition loop (turn {turn}): {preview}")]
    TextLoopDetected { turn: usize, preview: String },
    /// The model produced text matching a banlist pattern. The message
    /// is dropped (not rendered, not persisted to the session jsonl) and
    /// the turn ends. The matched pattern is included so the user knows
    /// which rule fired and can adjust the banlist if it was a false
    /// positive. Real motivator: TCK 299 ("insulting the President")
    /// exposure when reasoning models hallucinate political insults
    /// the user never asked for.
    #[error("guardrail blocked assistant text (pattern `{pattern}`)")]
    GuardrailBlocked { pattern: String },
}

/// The driver. Owns the client, the tool registry, and the per-call
/// context; the conversation lives on the stack of [`Agent::run`].
pub struct Agent<'a> {
    client: &'a dyn ChatProvider,
    registry: &'a ToolRegistry,
    ctx: ToolContext,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    session: Option<SessionStore>,
    /// Optional sink for incremental [`StreamEvent`]s produced by
    /// `chat_stream`. The CLI installs a callback here that prints text
    /// deltas as they arrive; tests that do not stream leave it unset
    /// and the events are silently dropped.
    stream_callback: Option<Box<dyn FnMut(StreamEvent) + Send + Sync + 'a>>,
    /// Optional mid-run interrupt channel. When the user sends a message
    /// while the agent is executing tool calls, the TUI writes to this
    /// slot. The agent checks it between tool batches and injects the
    /// message as a user turn so the model sees the correction immediately
    /// rather than after the full turn completes.
    interrupt: Option<Arc<std::sync::Mutex<Option<String>>>>,
    /// Output guardrail (banlist). Empty by default — model text is not
    /// filtered. When loaded with patterns, every assistant turn's text
    /// is checked before being persisted or returned; a match aborts
    /// the turn with `AgentError::GuardrailBlocked`.
    guardrail: crate::guardrail::Guardrail,
    /// Optional blob store + index handles. When present, the
    /// micro-compaction step swaps oversized tool outputs for
    /// `ctx://<hex>` references that survive in the on-disk store
    /// instead of being head-truncated. See
    /// [`crate::compaction::maybe_micro_compact_with_blobs`].
    #[cfg(feature = "ctx")]
    blob_handles: Option<(
        Arc<crate::blob_store::BlobStore>,
        Arc<crate::blob_index::BlobIndex>,
    )>,
}

impl<'a> Agent<'a> {
    pub fn new(
        client: &'a dyn ChatProvider,
        registry: &'a ToolRegistry,
        ctx: ToolContext,
        config: AgentConfig,
    ) -> Self {
        Self {
            client,
            registry,
            ctx,
            config,
            permission: Arc::new(crate::permission::AllowAll),
            session: None,
            stream_callback: None,
            interrupt: None,
            guardrail: crate::guardrail::Guardrail::empty(),
            #[cfg(feature = "ctx")]
            blob_handles: None,
        }
    }

    /// Attach a [`BlobStore`](crate::blob_store::BlobStore) +
    /// [`BlobIndex`](crate::blob_index::BlobIndex) so micro-compaction
    /// stashes oversized tool outputs into the on-disk store and
    /// replaces them with `ctx://<hex>` references instead of
    /// head-truncating. The handles are normally produced by
    /// [`crate::ToolRegistry::enable_sandbox`] and shared with the
    /// agent here.
    #[cfg(feature = "ctx")]
    pub fn with_blob_handles(
        mut self,
        store: Arc<crate::blob_store::BlobStore>,
        index: Arc<crate::blob_index::BlobIndex>,
    ) -> Self {
        self.blob_handles = Some((store, index));
        self
    }

    /// Installs an output guardrail. When loaded with patterns, every
    /// assistant text turn is matched against the banlist; a hit aborts
    /// the turn with `AgentError::GuardrailBlocked` and the offending
    /// message is *not* persisted to the session jsonl. With the default
    /// `Guardrail::empty()` the agent behaves exactly as before.
    pub fn with_guardrail(mut self, guardrail: crate::guardrail::Guardrail) -> Self {
        self.guardrail = guardrail;
        self
    }

    /// Attaches a mid-run interrupt channel. The TUI writes a correction
    /// message to this slot while the agent is running; the agent injects
    /// it as a user message between tool batches.
    pub fn with_interrupt(mut self, ch: Arc<std::sync::Mutex<Option<String>>>) -> Self {
        self.interrupt = Some(ch);
        self
    }

    /// Installs a stream callback that will be invoked for every
    /// [`StreamEvent`] produced by the underlying provider during
    /// `run`. The CLI uses this to render partial assistant text as it
    /// arrives; non-streaming providers synthesise one `TextDelta` +
    /// `Usage` pair per turn via the default [`ChatProvider::chat_stream`]
    /// implementation, so this hook works uniformly regardless of the
    /// provider in use.
    pub fn with_stream_callback<F>(mut self, callback: F) -> Self
    where
        F: FnMut(StreamEvent) + Send + Sync + 'a,
    {
        self.stream_callback = Some(Box::new(callback));
        self
    }

    /// Installs a [`Permission`] gate. Every tool call is checked
    /// against this policy before the tool runs; a [`PermissionDecision::Deny`]
    /// turns into a tool message so the model can recover.
    pub fn with_permission(mut self, permission: Arc<dyn Permission>) -> Self {
        self.permission = permission;
        self
    }

    /// Read-only handle to the attached session, if any. Exposed so
    /// the REPL can enumerate its messages and fork it without having
    /// to track a parallel `SessionStore` alongside the agent.
    pub fn session(&self) -> Option<&SessionStore> {
        self.session.as_ref()
    }

    /// Returns a reference to the tool context.
    pub fn ctx(&self) -> &ToolContext {
        &self.ctx
    }

    /// Override the model for the next turn (auto-routing).
    pub fn set_model(&mut self, model: String) {
        self.config.model = model;
    }

    /// Current model name.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Returns a copy of all messages in the current session transcript.
    /// Returns an empty vec if no session is active.
    pub fn session_messages(&self) -> Vec<aegis_api::ChatMessage> {
        self.session
            .as_ref()
            .map(|s| s.messages().to_vec())
            .unwrap_or_default()
    }

    /// Force an immediate compaction of the session transcript.
    /// Returns the number of messages removed, or 0 if nothing was compacted.
    pub fn force_compact(&mut self) -> usize {
        let store = match self.session.as_mut() {
            Some(s) => s,
            None => return 0,
        };
        let mut transcript = store.messages().to_vec();
        let before = transcript.len();
        if self.config.smart_compaction {
            let summarizer_fn = llm_summarizer(self.client, &self.config.model);
            // Force compaction by setting a very high token count
            maybe_compact_with(
                &mut transcript,
                u32::MAX,
                &self.config.compaction,
                Some(&summarizer_fn),
            );
        } else {
            maybe_compact_with(&mut transcript, u32::MAX, &self.config.compaction, None);
        }
        let removed = before.saturating_sub(transcript.len());
        if removed > 0 {
            // Rebuild the session with the compacted transcript. If the
            // disk rewrite fails (permission denied, disk full), log
            // and return 0 — pretending compaction succeeded would
            // leave the user with a phantom free context window and
            // on next --resume the full uncompacted transcript would
            // re-inflate, which is a worse UX than a loud failure.
            if let Err(e) = store.replace_messages(transcript) {
                eprintln!("\x1b[1;33m[aegis] compaction wrote to memory but disk rewrite failed: {e}\x1b[0m");
                return 0;
            }
        }
        removed
    }

    /// Appends a "by the way" note to the session transcript without
    /// invoking the model. Used by the REPL's `/btw` command: the user
    /// wants to give the agent extra context that should be visible on
    /// the next real turn, but not trigger a reply right now (and not
    /// interrupt a currently-running turn when typed mid-stream).
    ///
    /// The note is persisted as a `user`-role message wrapped with a
    /// visible marker so the model can tell it apart from a regular
    /// prompt: it is not a question, just context to remember.
    pub fn append_note(&mut self, text: &str) -> Result<(), AgentError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let wrapped = format!("[btw — context note from the user, no reply needed]\n{trimmed}");
        let msg = ChatMessage::user(wrapped);
        self.persist(&msg)?;
        Ok(())
    }

    /// Takes the session out of the agent, leaving `None` behind.
    /// Used by the REPL to transfer a session to a rebuilt agent
    /// (e.g. on `/overthink` toggle) without losing history.
    pub fn take_session(&mut self) -> Option<SessionStore> {
        self.session.take()
    }

    /// Puts a session back after it was taken with [`take_session`].
    pub fn restore_session(&mut self, session: SessionStore) {
        self.session = Some(session);
    }

    /// Attaches a [`SessionStore`] that will receive every appended
    /// message (system, user, assistant, tool) in order. If the store
    /// already contains messages they are replayed as the starting
    /// transcript — this is how `--resume` works.
    pub fn with_session(mut self, session: SessionStore) -> Self {
        self.session = Some(session);
        self
    }

    /// Runs the loop until the model returns a plain message or the turn
    /// cap trips.
    ///
    /// When a session store is attached and already has messages, the
    /// loop treats those as the starting transcript and appends the new
    /// user prompt on top of them. That is how `--resume` works without
    /// the agent having to know anything about files.
    pub async fn run(
        &mut self,
        user_prompt: impl Into<UserInput>,
    ) -> Result<AgentOutput, AgentError> {
        // Fix (Session 17, BUG #2): reject `max_turns = 0` BEFORE
        // persisting anything to the session, otherwise the user
        // message lands on disk with no assistant reply and a later
        // `--resume` replays a dangling prompt. Session 22: rerouted
        // through the dedicated `Config` variant for clearer caller
        // diagnostics.
        if self.config.max_turns == 0 {
            return Err(AgentError::Config("max_turns must be >= 1".to_string()));
        }

        let mut transcript: Vec<ChatMessage> = Vec::new();

        // Seed from an existing session if there is one and it has data,
        // otherwise plant the system prompt ourselves.
        let resumed = match &self.session {
            Some(s) if !s.messages().is_empty() => {
                // A session file can be left in an inconsistent state if
                // the previous run was killed between persisting an
                // assistant's tool_calls and the matching tool results,
                // or vice versa. Every OpenAI-compat provider rejects
                // orphaned tool messages with a 400, so strip them before
                // the transcript ever reaches a request builder.
                let raw: Vec<ChatMessage> = s.messages().to_vec();
                transcript.extend(OpenAICompatClient::sanitize_tool_calls(&raw));
                true
            }
            _ => false,
        };
        if !resumed {
            // Base system prompt
            let mut sys_text = self.config.system_prompt.clone().unwrap_or_default();
            // Per-turn skill injection (auto-skill feature)
            if let Some(ref extra) = self.config.extra_system {
                if !extra.is_empty() {
                    if !sys_text.is_empty() {
                        sys_text.push_str("\n\n");
                    }
                    sys_text.push_str(extra);
                }
            }
            if !sys_text.is_empty() {
                let m = ChatMessage::system(sys_text);
                self.persist(&m)?;
                transcript.push(m);
            }
        }

        // --- SessionStart hook ---
        if !resumed {
            if let Some(outcome) = self.fire_hooks(HookEvent::SessionStart, &[]) {
                if !outcome.output.is_empty() {
                    let tag = hooks::format_hook_output(HookEvent::SessionStart, &outcome.output);
                    let hook_msg = ChatMessage::system(tag);
                    self.persist(&hook_msg)?;
                    transcript.push(hook_msg);
                }
            }
        }

        // --- UserPromptSubmit hook ---
        let user_input: UserInput = user_prompt.into();
        let user_prompt_str = user_input.text().to_string();
        if let Some(outcome) = self.fire_hooks(
            HookEvent::UserPromptSubmit,
            &[("METIS_USER_PROMPT", &user_prompt_str)],
        ) {
            if outcome.blocked {
                let reason = outcome
                    .block_reason
                    .unwrap_or_else(|| "hook blocked".into());
                return Err(AgentError::Config(format!(
                    "user-prompt-submit hook blocked: {reason}"
                )));
            }
            if !outcome.output.is_empty() {
                let tag = hooks::format_hook_output(HookEvent::UserPromptSubmit, &outcome.output);
                let hook_msg = ChatMessage::system(tag);
                self.persist(&hook_msg)?;
                transcript.push(hook_msg);
            }
        }

        // --- Task nudge: inject mandatory task-tracking reminder ---
        // Fires on every new turn (not just first) when task tools are
        // registered. This makes the behaviour model-independent: even a
        // model that has drifted away from the system-prompt instruction
        // sees the reminder right before the user message and can't miss it.
        if self.config.task_nudge && self.registry.specs().iter().any(|s| s.function.name == "create_task") {
            let nudge = "\
<task-tracking-required>\n\
For any work that has more than one step:\n\
1. Call `create_task` for EACH planned step BEFORE doing any work.\n\
2. Call `update_task` (status=in_progress) when you START a step.\n\
3. Call `update_task` (status=done) immediately when a step is COMPLETE.\n\
Single-step answers (one tool call or one short reply) are exempt.\n\
</task-tracking-required>";
            let nudge_msg = ChatMessage::system(nudge.to_string());
            self.persist(&nudge_msg)?;
            transcript.push(nudge_msg);
        }

        // --- Context priming (first turn only, not resumed sessions) ---
        if !resumed && self.config.context_priming {
            let hint = crate::context_primer::prime_context(
                &user_prompt_str,
                &self.ctx.workspace_root,
                12,
            );
            if !hint.is_empty() {
                let text = crate::context_primer::format_hint(&hint);
                let hint_msg = ChatMessage::system(text);
                self.persist(&hint_msg)?;
                transcript.push(hint_msg);
            }
        }

        let user_msg = user_input.into_message();
        self.persist(&user_msg)?;
        transcript.push(user_msg);

        // --- Auto-task: silently create a task for the user's request ---
        // Fires when task_nudge is enabled and create_task tool exists.
        // Runs with AllowAll permission (no user prompt) — the task is
        // an internal bookkeeping action, not a mutating file operation.
        // This makes task creation model-independent: even weak models
        // (GLM, MiniMax) that ignore the nudge message will have a task
        // entry in tasks.json for the panel to display.
        if self.config.task_nudge
            && self.registry.specs().iter().any(|s| s.function.name == "create_task")
        {
            let desc: String = user_prompt_str.split_whitespace().take(12).collect::<Vec<_>>().join(" ");
            let desc = if user_prompt_str.split_whitespace().count() > 12 {
                format!("{desc}…")
            } else {
                desc
            };
            let args = format!(r#"{{"description":{}}}"#, serde_json::json!(desc));
            let allow_all = crate::permission::AllowAll;
            run_tool(self.registry, &allow_all, &self.ctx, "create_task", &args).await;
        }

        let tools = if self.registry.is_empty() {
            None
        } else {
            // Hide phantom tools — those whose runtime dependency is
            // not wired up — from the advertised spec. Otherwise the
            // model sees them, calls them, gets a hard error like
            // "agent spawner not configured", retries, and burns a
            // turn (or trips the loop detector). Today only the
            // subagent tools have this concern.
            let agent_spawner_present = self.ctx.agent_spawner.is_some();
            let mut specs = self.registry.specs();
            if !agent_spawner_present {
                specs.retain(|t| {
                    let n = t.function.name.as_str();
                    n != "agent" && n != "spawn_agent" && n != "parallel_agents"
                });
            }
            Some(specs)
        };

        let mut total = UsageSnapshot::default();
        // Fix (Session 17, BUG #1): on resume we have no real
        // `prompt_tokens` reading yet but the preloaded transcript may
        // already exceed the context window. Seed with `u32::MAX` so
        // the very first `maybe_compact` call evaluates the size guard
        // and compacts if the transcript is long enough. From turn 2
        // onwards real provider usage replaces this sentinel.
        let mut last_prompt_tokens: u32 = if resumed { u32::MAX } else { 0 };
        let mut consecutive_errors: u32 = 0;
        let mut lint_fail_count: u8 = 0;
        // Tool-call loop detector — Claude Code style: only count
        // consecutive identical batches as a "loop" when the prior
        // batch was *all errors*. Legitimate iteration (rebuild after
        // a fix, polling, retry-with-different-state) repeats the same
        // command but its results change, so we let it through.
        // Threshold raised from 3 → 5 because reasoning models often
        // re-emit the same call once or twice while thinking.
        let mut last_tool_sig: Option<String> = None;
        let mut tool_sig_streak: u32 = 0;
        let mut prior_batch_all_errors: bool = false;
        const TOOL_LOOP_THRESHOLD: u32 = 5;
        // Parallel guard for the text channel. Stores the last few
        // normalized fingerprints of assistant.content; 3 ≈-equal in a
        // row → TextLoopDetected. Runs alongside tool_call_signatures
        // because reasoning models often vary their tool args while
        // re-emitting the same narration paragraph.
        let mut text_signatures: Vec<u64> = Vec::new();

        for turn in 1..=self.config.max_turns {
            // Check cancel flag before each LLM round-trip. Set by the
            // TUI/REPL when the user interrupts the current turn.
            if self
                .ctx
                .cancel_flag
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }

            // Before every provider round-trip, give the compactor a
            // chance to shrink the transcript if we're near the
            // context window limit. The summarizer is built fresh each
            // iteration so the borrow of `self.client` doesn't span
            // the mutable borrows below.
            let pre_compact_len = transcript.len();
            // Level 1: microcompact at 30% — trim oversized tool outputs in-place.
            // Fires before full compaction to reclaim tokens from large tool results
            // without removing any messages. With ctx blob handles attached,
            // oversized content is stashed (ctx://<hex>) instead of being
            // head-truncated, so the bytes survive in the on-disk store.
            #[cfg(feature = "ctx")]
            {
                if let Some((store, index)) = &self.blob_handles {
                    crate::compaction::maybe_micro_compact_with_blobs(
                        &mut transcript,
                        last_prompt_tokens,
                        0.30,
                        self.config.compaction.context_window,
                        2048,
                        store,
                        index,
                    );
                } else {
                    maybe_micro_compact(
                        &mut transcript,
                        last_prompt_tokens,
                        0.30,
                        self.config.compaction.context_window,
                        2048,
                    );
                }
            }
            #[cfg(not(feature = "ctx"))]
            {
                maybe_micro_compact(
                    &mut transcript,
                    last_prompt_tokens,
                    0.30,
                    self.config.compaction.context_window,
                    2048,
                );
            }
            // Level 2: full compaction at trigger_ratio (default 55%).
            if self.config.smart_compaction {
                let summarizer_fn = llm_summarizer(self.client, &self.config.model);
                maybe_compact_with(
                    &mut transcript,
                    last_prompt_tokens,
                    &self.config.compaction,
                    Some(&summarizer_fn),
                );
            } else {
                maybe_compact_with(
                    &mut transcript,
                    last_prompt_tokens,
                    &self.config.compaction,
                    None,
                );
            }
            // --- compact hook ---
            if transcript.len() != pre_compact_len {
                if let Some(outcome) = self.fire_hooks(HookEvent::Compact, &[]) {
                    if !outcome.output.is_empty() {
                        let tag = hooks::format_hook_output(HookEvent::Compact, &outcome.output);
                        let hook_msg = ChatMessage::system(tag);
                        self.persist(&hook_msg)?;
                        transcript.push(hook_msg);
                    }
                }
            }

            let effective_temperature = self.config.temperature.or_else(|| {
                if self.config.autotune {
                    let last_text = transcript
                        .iter()
                        .rev()
                        .find(|m| m.role == aegis_api::Role::User)
                        .and_then(|m| m.content.as_deref())
                        .unwrap_or("");
                    Some(autotune(last_text).temperature)
                } else {
                    None
                }
            });
            // In-session sanitize: strip orphan tool_calls/tool_results
            // before every request, not just on resume. An assistant
            // turn can land on disk before its tool results if the
            // process is killed mid-batch (Ctrl-C, OOM, panic), and
            // every OpenAI-compat provider 400s on orphans. Without
            // this guard the next request kills the session via
            // AgentError::Api → user sees "session boşa gitti".
            // Cheap when transcript is well-formed (single linear pass).
            let sanitized_messages = OpenAICompatClient::sanitize_tool_calls(&transcript);
            let request = ChatRequest {
                model: self.config.model.clone(),
                messages: sanitized_messages,
                tools: tools.clone(),
                temperature: effective_temperature,
                max_tokens: self.config.max_tokens,
                thinking: self.config.thinking,
                thinking_budget: self.config.thinking_budget,
            };
            // Move the callback out of `self` for the duration of the
            // provider call. Borrowing `self.client` and `self.stream_callback`
            // at the same time would trip the borrow checker, so we
            // take/put-back via `Option`. Any panic inside `chat_stream`
            // drops the callback with the agent — no ordering risk.
            let mut cb_slot = self.stream_callback.take();
            let response: ChatResponse = {
                // `committed` flips to true the moment the user has seen
                // any visible output from the current attempt. Once that
                // happens, we can no longer retry safely: a second
                // attempt would stream a fresh response that visually
                // concatenates with the partial one on the terminal
                // (there is no portable way to un-print multi-line
                // streamed text from here). So retries are only allowed
                // BEFORE the first textual event escapes this closure.
                let committed = std::sync::atomic::AtomicBool::new(false);
                let mut dispatch = |event: StreamEvent| {
                    match &event {
                        StreamEvent::TextDelta(_)
                        | StreamEvent::ThinkingDelta(_)
                        | StreamEvent::ToolCall { .. }
                        | StreamEvent::ToolResult { .. } => {
                            committed.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    if let Some(cb) = cb_slot.as_mut() {
                        cb(event);
                    }
                };
                // Mutable copy of the request: a tool-payload 400 lets
                // us trim the transcript and retry instead of killing
                // the session outright (Claude Code style — drop bad
                // turn, keep talking).
                let mut request = request;
                let mut attempt = 0u32;
                let mut tool_400_recoveries = 0u32;
                loop {
                    // Wrap the provider call in a per-attempt timeout
                    // so a stalled SSE connection (TCP open, no bytes
                    // for minutes) doesn't freeze the whole turn. The
                    // existing retry classifier treats Timeout as
                    // transient, so up to 3 hung attempts auto-recover
                    // before propagating to the caller.
                    let timeout = std::time::Duration::from_secs(PROVIDER_CALL_TIMEOUT_SECS);
                    let call = self.client.chat_stream(&request, &mut dispatch);
                    let result = match tokio::time::timeout(timeout, call).await {
                        Ok(inner) => inner,
                        Err(_) => Err(ApiError::Timeout {
                            seconds: PROVIDER_CALL_TIMEOUT_SECS,
                        }),
                    };
                    match result {
                        Ok(v) => break v,
                        Err(e)
                            if attempt < 3
                                && is_transient(&e)
                                && !committed.load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            // Safe to retry: nothing has been shown to
                            // the user yet, so a fresh attempt cannot
                            // visually double-up with the failed one.
                            attempt += 1;
                            let delay = std::time::Duration::from_secs(1 << (attempt - 1));
                            tokio::time::sleep(delay).await;
                        }
                        Err(e)
                            if tool_400_recoveries < 1
                                && is_tool_payload_400(&e)
                                && !committed.load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            // Provider rejected the tool_call payload
                            // (orphan id, malformed args, missing pair).
                            // Drop trailing assistant turns whose tool
                            // results never landed, re-sanitize, and
                            // retry once. If it fails again, propagate.
                            tool_400_recoveries += 1;
                            request.messages = strip_trailing_orphan_assistants(
                                &OpenAICompatClient::sanitize_tool_calls(&request.messages),
                            );
                        }
                        Err(e) => return Err(AgentError::Api(e)),
                    }
                }
            };
            self.stream_callback = cb_slot;

            if let Some(usage) = response.usage {
                total.input_tokens = total.input_tokens.saturating_add(usage.prompt_tokens);
                total.output_tokens = total.output_tokens.saturating_add(usage.completion_tokens);
                total.cache_read_tokens = total
                    .cache_read_tokens
                    .saturating_add(usage.cache_read_tokens);
                total.cache_write_tokens = total
                    .cache_write_tokens
                    .saturating_add(usage.cache_write_tokens);
                // The compactor looks at the size of the most recent
                // request as an approximation of the current context
                // depth. Cache reads count toward that depth because
                // the model still processes those tokens even though
                // they're billed at a discount.
                last_prompt_tokens = usage
                    .prompt_tokens
                    .saturating_add(usage.cache_read_tokens)
                    .saturating_add(usage.cache_write_tokens);
            }

            // Hard cost ceiling — abort before the model does anything
            // else so we don't accidentally spend more on tool execution.
            if let Some(limit) = self.config.max_cost_usd {
                let spent = ModelPricing::resolve(&self.config.model)
                    .estimate(&total)
                    .total_usd();
                if spent >= limit {
                    return Err(AgentError::BudgetExceeded { spent, limit });
                }
            }

            let choice = response
                .choices
                .into_iter()
                .next()
                .ok_or(AgentError::NoChoices)?;
            let assistant = choice.message;

            // Output guardrail — checked BEFORE persist/transcript so a
            // banlist hit never lands on disk and never gets fed back
            // into the next turn's context. Empty guardrail (default)
            // is a single hash compare → near-zero overhead. Tool-call
            // turns with no narration are checked anyway: a model can
            // emit banned text alongside a tool call.
            if let Some(text) = assistant.content.as_deref() {
                if !text.is_empty() {
                    if let crate::guardrail::Verdict::Block(pattern) = self.guardrail.check(text) {
                        return Err(AgentError::GuardrailBlocked { pattern });
                    }
                }
            }

            // Always append the assistant turn before doing anything
            // with it — this is what the next request will need to see.
            // Skip ghost assistant messages that have no content, no
            // content_blocks, and no tool_calls. Providers reject these
            // with a 400, and sanitize_tool_calls strips them anyway.
            // Reasoning models (DeepSeek V4, Qwen-thinking) sometimes
            // produce only reasoning_content with no visible text.
            if assistant.content.is_some()
                || !assistant.content_blocks.is_empty()
                || !assistant.tool_calls.is_empty()
            {
                self.persist(&assistant)?;
                transcript.push(assistant.clone());
            }

            // Silent-loop detection: if the model issues the same set of
            // tool calls 3 turns in a row, something is stuck. Bail out
            // with a clear error so the user knows to intervene rather
            // than burn turns.
            if !assistant.tool_calls.is_empty() {
                let sig = tool_call_signature(&assistant.tool_calls);
                if last_tool_sig.as_deref() == Some(&sig) && prior_batch_all_errors {
                    tool_sig_streak += 1;
                } else if last_tool_sig.as_deref() == Some(&sig) {
                    // Same signature but prior batch had progress (some
                    // tool returned non-error). Let it through — looks
                    // like polling or rebuild-after-fix, not a stuck
                    // loop.
                    tool_sig_streak = 1;
                } else {
                    tool_sig_streak = 1;
                }
                last_tool_sig = Some(sig.clone());
                if tool_sig_streak >= TOOL_LOOP_THRESHOLD {
                    return Err(AgentError::LoopDetected {
                        turn,
                        signature: truncate_for_error(&sig, 240),
                    });
                }
            } else {
                // Plain turn (no tool calls) breaks any prior identical-call
                // streak so the counter doesn't carry over across unrelated
                // exchanges.
                last_tool_sig = None;
                tool_sig_streak = 0;
            }

            // Text-repetition guard. Runs in parallel with the tool-call
            // signature check because reasoning models often vary tool
            // args while re-emitting the same narration paragraph.
            // Skipped when the model produces no text (pure tool turn) —
            // empty content would otherwise hash to a constant and
            // trip the guard on long tool-only chains.
            let text_for_check = assistant.content.as_deref().unwrap_or("");
            if let Some(fp) = normalized_text_fingerprint(text_for_check) {
                text_signatures.push(fp);
                let n = text_signatures.len();
                if n >= 3
                    && text_signatures[n - 1] == text_signatures[n - 2]
                    && text_signatures[n - 2] == text_signatures[n - 3]
                {
                    return Err(AgentError::TextLoopDetected {
                        turn,
                        preview: truncate_for_error(text_for_check, 240),
                    });
                }
            } else {
                text_signatures.clear();
            }

            if assistant.tool_calls.is_empty() {
                // Plain text reply; we're done.
                let final_text = assistant.content.unwrap_or_default();
                return Ok(AgentOutput {
                    final_text,
                    usage: total,
                    turns: turn,
                    transcript,
                });
            }

            // Phase 1 (sequential): emit ToolCall stream events and
            // fire pre-hooks. Blocked calls get a result immediately;
            // non-blocked calls are collected for parallel execution.
            let mut blocked_results: Vec<Option<ChatMessage>> = Vec::new();
            let mut runnable_indices: Vec<usize> = Vec::new();

            for (i, call) in assistant.tool_calls.iter().enumerate() {
                if let Some(cb) = self.stream_callback.as_mut() {
                    cb(StreamEvent::ToolCall {
                        name: call.function.name.clone(),
                        arguments_preview: preview_arguments(&call.function.arguments),
                    });
                }
                let tool_name = &call.function.name;
                let mut was_blocked = false;
                if let Some(outcome) = self.fire_hooks(
                    HookEvent::PreToolUse,
                    &[
                        ("METIS_TOOL_NAME", tool_name),
                        ("METIS_TOOL_ARGS", &call.function.arguments),
                    ],
                ) {
                    if outcome.blocked {
                        let reason = outcome
                            .block_reason
                            .unwrap_or_else(|| "hook blocked".into());
                        let tool_msg = ChatMessage::tool_result(
                            call.id.clone(),
                            tool_name.clone(),
                            format!("error: pre-tool-use hook blocked — {reason}"),
                        );
                        blocked_results.push(Some(tool_msg));
                        was_blocked = true;
                    }
                }
                if !was_blocked {
                    blocked_results.push(None);
                    runnable_indices.push(i);
                }
            }

            // Phase 2: execute non-blocked calls concurrently via
            // futures::join_all. This polls all futures on the current
            // task without spawning, so no 'static bound is needed.
            type RunResult = ToolRunOutcome;
            let executed: Vec<(usize, RunResult)> = if runnable_indices.len() > 1 {
                let registry = self.registry;
                let permission: &dyn Permission = &*self.permission;
                let ctx = &self.ctx;
                let futs: Vec<_> = runnable_indices
                    .iter()
                    .map(|&i| {
                        let call = &assistant.tool_calls[i];
                        let name = call.function.name.clone();
                        let arguments = call.function.arguments.clone();
                        async move {
                            (
                                i,
                                run_tool(registry, permission, ctx, &name, &arguments).await,
                            )
                        }
                    })
                    .collect();
                futures::future::join_all(futs).await
            } else {
                let mut results = Vec::new();
                for &i in &runnable_indices {
                    let call = &assistant.tool_calls[i];
                    let result = self
                        .execute_call(&call.function.name, &call.function.arguments)
                        .await;
                    results.push((i, result));
                }
                results
            };

            // Build a map from index → result for easy lookup.
            let mut result_map: std::collections::HashMap<usize, RunResult> =
                executed.into_iter().collect();

            // Track whether any tool in this batch was hard-denied by
            // the user. If so, we end the turn after persisting all
            // results — no follow-up model call, so the model cannot
            // pivot around the explicit refusal.
            let mut any_hard_denied = false;
            // For the loop detector: was every result in this batch an
            // error? If yes, identical signature in the next turn is a
            // real loop; if no (some progress), reset the streak.
            let mut batch_had_success = false;

            // Phase 3 (sequential): process all results in original
            // order — stream events, post-hooks, persist. This keeps
            // transcript ordering deterministic.
            for (i, call) in assistant.tool_calls.iter().enumerate() {
                if let Some(blocked_msg) = blocked_results[i].take() {
                    self.persist(&blocked_msg)?;
                    transcript.push(blocked_msg);
                    continue;
                }

                let outcome = result_map
                    .remove(&i)
                    .expect("runnable index must have a result");
                if outcome.hard_denied {
                    any_hard_denied = true;
                }
                let result = outcome.output;
                let tool_name = &call.function.name;
                let result_text = result.as_text().to_string();

                if let Some(cb) = self.stream_callback.as_mut() {
                    let is_error = result_text.starts_with("error:");
                    let preview = format_tool_preview(tool_name, &result_text);
                    cb(StreamEvent::ToolResult {
                        name: tool_name.to_string(),
                        preview,
                        is_error,
                    });
                }

                if let Some(outcome) = self.fire_hooks(
                    HookEvent::PostToolUse,
                    &[
                        ("METIS_TOOL_NAME", tool_name),
                        ("METIS_TOOL_RESULT", &result_text),
                    ],
                ) {
                    if !outcome.output.is_empty() {
                        let tag =
                            hooks::format_hook_output(HookEvent::PostToolUse, &outcome.output);
                        let hook_msg = ChatMessage::system(tag);
                        self.persist(&hook_msg)?;
                        transcript.push(hook_msg);
                    }
                }

                let tool_msg = match result {
                    crate::tools::ToolOutput::Text(text) => {
                        let text = if text.starts_with("error:") {
                            consecutive_errors += 1;
                            enrich_error_hint(tool_name, &text, consecutive_errors)
                        } else {
                            consecutive_errors = 0;
                            batch_had_success = true;
                            text
                        };
                        // Wrap in boundary markers so file/web content cannot
                        // be interpreted as model instructions (prompt injection).
                        let text = format!("[TOOL_RESULT]\n{text}\n[/TOOL_RESULT]");
                        ChatMessage::tool_result(call.id.clone(), tool_name.clone(), text)
                    }
                    crate::tools::ToolOutput::Multimodal { blocks, .. } => {
                        consecutive_errors = 0;
                        batch_had_success = true;
                        ChatMessage::tool_result_multimodal(
                            call.id.clone(),
                            tool_name.clone(),
                            blocks,
                        )
                    }
                };
                self.persist(&tool_msg)?;
                transcript.push(tool_msg);

                let is_edit_tool = tool_name == "edit_file"
                    || tool_name == "write_file"
                    || tool_name == "multi_edit";

                // Auto-lint: run lint command after successful edits and
                // append any errors as a system message so the model can fix them.
                // Tracks lint_fail_count; once lint_max_retries is exceeded,
                // lint enforcement is suspended for this run so the model isn't
                // stuck in an infinite fix loop on genuinely broken toolchains.
                if is_edit_tool && consecutive_errors == 0 {
                    if let Some(ref lint_cmd) = self.config.lint_command {
                        if lint_fail_count < self.config.lint_max_retries {
                            match run_lint(lint_cmd, &self.ctx.workspace_root) {
                                Some(lint_err) => {
                                    lint_fail_count += 1;
                                    let remaining = self.config.lint_max_retries - lint_fail_count;
                                    let hint = if remaining == 0 {
                                        format!(
                                            "[auto-lint] `{lint_cmd}` still failing after \
                                             {} attempts. Lint enforcement suspended — \
                                             continue and inform the user of outstanding \
                                             lint errors at the end.\n\nLast output:\n{lint_err}",
                                            self.config.lint_max_retries
                                        )
                                    } else {
                                        format!(
                                            "[auto-lint] `{lint_cmd}` failed \
                                             ({lint_fail_count}/{}):\n{lint_err}\n\
                                             Fix the lint errors in the files you just edited. \
                                             ({remaining} attempt(s) remaining before \
                                             lint enforcement is suspended)",
                                            self.config.lint_max_retries
                                        )
                                    };
                                    let lint_msg = ChatMessage::system(hint);
                                    self.persist(&lint_msg)?;
                                    transcript.push(lint_msg);
                                }
                                None => {
                                    // Lint passed — reset failure counter so a fresh
                                    // edit session gets a full retry budget again.
                                    lint_fail_count = 0;
                                }
                            }
                        }
                    }
                }

                // Auto-commit: if enabled and tool was a file-mutating tool
                // that succeeded, auto-commit the changes to git.
                if self.config.auto_commit && consecutive_errors == 0 && is_edit_tool {
                    auto_commit_changes(&self.ctx.workspace_root, tool_name);
                }
            }

            // ── Plan reassessment nudge ───────────────────────────
            // When the model hits a streak of consecutive errors, inject
            // a system message that forces it to stop and rethink rather
            // than blindly retrying. The threshold messages are additive:
            // at 3 errors the model gets a "reassess" nudge, at 5 a
            // stronger "completely different approach" nudge. These are
            // system messages (not part of the tool result) so they feel
            // like an external intervention.
            if consecutive_errors == 5 {
                let nudge = ChatMessage::system(
                    "[plan-reassessment] You have failed 5 times in a row. Your current \
                     approach is not working. Step back, re-read the user's original request, \
                     and try a completely different approach. Do not retry the same strategy."
                        .to_string(),
                );
                self.persist(&nudge)?;
                transcript.push(nudge);
            } else if consecutive_errors == 3 {
                let nudge = ChatMessage::system(
                    "[plan-reassessment] 3 consecutive tool errors detected. Stop and \
                     reassess your current approach. Consider what is going wrong and why. \
                     Try a fundamentally different strategy instead of retrying the same \
                     thing. If unsure, use ask_user to request guidance."
                        .to_string(),
                );
                self.persist(&nudge)?;
                transcript.push(nudge);
            }

            // Check for a mid-run user interrupt. If the TUI pushed a
            // correction while we were executing tools, inject it as a
            // user message NOW so the model sees it on the very next
            // LLM call rather than after the full turn completes.
            let interrupt_msg: Option<String> = self
                .interrupt
                .as_ref()
                .and_then(|ch| ch.lock().ok())
                .and_then(|mut g| g.take());
            if let Some(msg) = interrupt_msg {
                let user_msg = ChatMessage::user(format!(
                    "[user correction mid-run — adjust your approach accordingly]\n{msg}"
                ));
                self.persist(&user_msg)?;
                transcript.push(user_msg);
            }

            // If the user explicitly hard-denied any tool in this
            // batch, end the turn now. The deny result is already in
            // the transcript so the next user message gives the model
            // the context it needs to acknowledge the refusal. Looping
            // back here would let the model pivot around the deny —
            // exactly what the user is telling us not to do.
            if any_hard_denied {
                let final_text = assistant
                    .content
                    .clone()
                    .unwrap_or_else(|| "(stopped — user denied tool call)".to_string());
                return Ok(AgentOutput {
                    final_text,
                    usage: total,
                    turns: turn,
                    transcript,
                });
            }

            // Update the loop-detector view of the just-finished batch.
            // Consumed by the next turn's signature check (see top of
            // assistant-turn handler).
            prior_batch_all_errors = !batch_had_success;
        }

        Err(AgentError::MaxTurns(self.config.max_turns))
    }

    /// Run hooks for a given event and return the outcome. Returns
    /// `None` if no hooks are configured for the event.
    fn fire_hooks(
        &self,
        event: HookEvent,
        extra_env: &[(&str, &str)],
    ) -> Option<hooks::HookOutcome> {
        if self.ctx.hooks.hooks_for(event).is_empty() {
            return None;
        }
        let mut env = std::collections::HashMap::new();
        env.insert(
            "METIS_WORKSPACE".to_string(),
            self.ctx.workspace_root.display().to_string(),
        );
        for (k, v) in extra_env {
            env.insert(k.to_string(), v.to_string());
        }
        Some(hooks::run_hooks(
            &self.ctx.hooks,
            event,
            &env,
            &self.ctx.workspace_root,
        ))
    }

    /// Writes `msg` to the attached session store, if any. Also keeps
    /// the in-memory mirror in the store in sync so a later resume in
    /// the same process sees a consistent view.
    fn persist(&mut self, msg: &ChatMessage) -> Result<(), AgentError> {
        if let Some(store) = self.session.as_mut() {
            store.append(msg)?;
        }
        Ok(())
    }

    /// Delegates to the free function [`run_tool`] which is `Send`-safe.
    async fn execute_call(&self, name: &str, arguments: &str) -> ToolRunOutcome {
        run_tool(self.registry, &*self.permission, &self.ctx, name, arguments).await
    }
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        // Fire session_end hooks on agent drop.
        // This runs regardless of how the session ends (normal exit, error,
        // or panic unwind), giving users a reliable hook point for cleanup,
        // logging, or notification.
        if !self.ctx.hooks.hooks_for(HookEvent::SessionEnd).is_empty() {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "METIS_WORKSPACE".to_string(),
                self.ctx.workspace_root.display().to_string(),
            );
            let _ = hooks::run_hooks(
                &self.ctx.hooks,
                HookEvent::SessionEnd,
                &env,
                &self.ctx.workspace_root,
            );
        }
    }
}

/// Result of running one tool call: the output plus a flag for whether
/// the user explicitly hard-denied at the interactive prompt. The flag
/// lets the agent loop break out of the current turn entirely instead
/// of looping back to the model — "stop and wait for the user"
/// behaviour rather than pivoting to a different tool.
pub(crate) struct ToolRunOutcome {
    pub output: crate::tools::ToolOutput,
    pub hard_denied: bool,
}

/// Maximum length of a single tool result before it's truncated, in
/// characters. Anything past this is replaced with a `[truncated]`
/// marker so a runaway grep / read_file / bash output cannot blow the
/// context window in one shot. Sized so even a dozen big results in a
/// row stays well under the 128k token budget.
const MAX_TOOL_RESULT_CHARS: usize = 30_000;

fn truncate_tool_output(text: String, tool: &str) -> String {
    if text.chars().count() <= MAX_TOOL_RESULT_CHARS {
        return text;
    }
    let mut head: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    let dropped = text.chars().count() - MAX_TOOL_RESULT_CHARS;
    head.push_str(&format!(
        "\n\n[truncated: `{tool}` produced {dropped} more characters that were elided to keep the context window safe. \
         Re-run with a narrower scope (e.g. head_limit, smaller path, more specific pattern) if you need the rest.]"
    ));
    head
}

/// Pure tool execution extracted from [`Agent`] so it can be called from
/// spawned tasks without capturing the non-`Send` stream callback.
/// All parameters are `Send + Sync`, making this safe for concurrent use.
///
/// A soft `Deny` is returned as an ordinary tool error so the model can
/// pivot. A `HardDeny` (user pressed "Deny" at the interactive prompt)
/// sets `hard_denied = true` on the outcome — the agent loop watches
/// for that flag and ends the current turn without making another model
/// call, so the model cannot work around the user's explicit refusal.
async fn run_tool(
    registry: &ToolRegistry,
    permission: &dyn Permission,
    ctx: &ToolContext,
    name: &str,
    arguments: &str,
) -> ToolRunOutcome {
    let tool = match registry.get(name) {
        Some(t) => t,
        None => {
            return ToolRunOutcome {
                output: crate::tools::ToolOutput::Text(format!("error: unknown tool `{name}`")),
                hard_denied: false,
            };
        }
    };
    let args = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) => v,
        Err(e) => {
            return ToolRunOutcome {
                output: crate::tools::ToolOutput::Text(format!(
                    "error: arguments are not valid JSON: {e}"
                )),
                hard_denied: false,
            };
        }
    };
    match permission.check(name, &args) {
        PermissionDecision::Allow => {}
        PermissionDecision::Deny(reason) => {
            // Soft deny (allowlist / policy rule). Feed the reason
            // back to the model as a tool error so it can try another
            // approach on the next turn.
            return ToolRunOutcome {
                output: crate::tools::ToolOutput::Text(format!(
                    "error: permission denied — {reason}"
                )),
                hard_denied: false,
            };
        }
        PermissionDecision::HardDeny(reason) => {
            // User pressed "Deny" at the interactive prompt. The
            // outcome carries `hard_denied = true` so the agent loop
            // stops the current turn — no follow-up model call, no
            // pivot to a different tool. The user gets the prompt
            // back and can type a new instruction.
            return ToolRunOutcome {
                output: crate::tools::ToolOutput::Text(format!(
                    "error: permission denied — {reason}. \
                     Stopping this turn; waiting for the user."
                )),
                hard_denied: true,
            };
        }
    }
    {
        let state = ctx.plan_state.lock().unwrap();
        if state.is_read_only() && !crate::tools::PLAN_MODE_ALLOWED.contains(&name) {
            return ToolRunOutcome {
                output: crate::tools::ToolOutput::Text(format!(
                    "error: plan mode is active (state: {state}) — `{name}` is not allowed. \
                     Use `exit_plan_mode` first."
                )),
                hard_denied: false,
            };
        }
    }
    match tool.execute_multimodal(args, ctx).await {
        Ok(crate::tools::ToolOutput::Text(text)) => ToolRunOutcome {
            output: crate::tools::ToolOutput::Text(truncate_tool_output(text, name)),
            hard_denied: false,
        },
        Ok(out) => ToolRunOutcome {
            output: out,
            hard_denied: false,
        },
        Err(err) => ToolRunOutcome {
            output: crate::tools::ToolOutput::Text(format!("error: {err}")),
            hard_denied: false,
        },
    }
}

/// Shortens a tool-call argument JSON string to a single-line preview
/// suitable for a terminal. Strips interior newlines so streamed JSON
/// with embedded whitespace still fits on one line, and truncates with
/// an ellipsis past 120 chars so a giant `edit_file` payload doesn't
/// take over the terminal.
fn preview_arguments(raw: &str) -> String {
    const MAX: usize = 120;
    let flat: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= MAX {
        return flat;
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("{head}…")
}

/// Retry an async provider call up to 3 times on transient errors
/// (rate-limit, server errors, network failures). Uses exponential
/// backoff: 1s, 2s, 4s via `tokio::time::sleep`.
#[cfg(test)]
async fn retry_transient_async<F, Fut, T>(mut f: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    const MAX_RETRIES: u32 = 3;
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES && is_transient(&e) => {
                attempt += 1;
                let delay = std::time::Duration::from_secs(1 << (attempt - 1));
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Determine if an API error is transient and worth retrying.
fn is_transient(err: &ApiError) -> bool {
    match err {
        ApiError::Http(_) => true, // network errors
        ApiError::Status { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 504),
        ApiError::Timeout { .. } => true, // upstream stalled — retry with fresh connection
        ApiError::Decode(_) | ApiError::MissingKey(_) => false,
    }
}

/// Per-attempt budget for a single `chat_stream` call. Long enough for
/// reasoning models (DeepSeek V4 thinking can take 60-120s on hard
/// prompts) but short enough that a hung connection self-recovers
/// within one retry cycle instead of the user staring at a frozen
/// terminal. Hard-coded at the call site to keep the surgical fix
/// surgical; promote to AgentConfig if a user ever needs to tune it.
const PROVIDER_CALL_TIMEOUT_SECS: u64 = 300;

/// True when a 400 looks like a tool-payload mismatch — orphan
/// `tool_call_id`, missing tool result, or malformed function block.
/// Body matching is heuristic but covers OpenAI, DeepSeek, and Anthropic
/// wording observed in the wild.
fn is_tool_payload_400(err: &ApiError) -> bool {
    match err {
        ApiError::Status { status: 400, body } => {
            let lower = body.to_ascii_lowercase();
            lower.contains("tool_call")
                || lower.contains("tool_use")
                || lower.contains("tool result")
                || lower.contains("tool message")
                || lower.contains("tool_use_id")
        }
        _ => false,
    }
}

/// Trims the tail of `messages` so it never ends with an assistant turn
/// whose `tool_calls` are unanswered. Sanitize already drops orphan
/// `tool` messages and assistant tool-call turns that have no following
/// results, but a transcript may still end on an assistant *text* turn
/// that was followed by a partial tool batch we just stripped — that's
/// fine, the model can simply continue. The point of this pass is to
/// guarantee the next request opens with a clean turn boundary.
fn strip_trailing_orphan_assistants(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = messages.to_vec();
    while let Some(last) = out.last() {
        if last.role == aegis_api::Role::Assistant && !last.tool_calls.is_empty() {
            // Sanitize already removed orphans; if one slipped through
            // (e.g. result with mismatched id), drop the assistant turn
            // so the model never sees its own dangling call.
            out.pop();
        } else {
            break;
        }
    }
    out
}

/// Convenience: most callers want a default-configured agent that uses
/// the model name straight from the CLI flag.
pub async fn run_simple(
    client: &DeepSeekClient,
    registry: &ToolRegistry,
    workspace_root: impl Into<std::path::PathBuf>,
    model: impl Into<String>,
    system_prompt: Option<String>,
    user_prompt: impl Into<UserInput>,
) -> Result<AgentOutput, AgentError> {
    let config = AgentConfig {
        model: model.into(),
        system_prompt,
        ..AgentConfig::default()
    };
    let ctx = ToolContext::new(workspace_root);
    let mut agent = Agent::new(client as &dyn ChatProvider, registry, ctx, config);
    agent.run(user_prompt).await
}

/// Run a lint command and return its stderr/stdout if it fails.
/// Returns `None` on success (exit 0) or if the command can't be spawned.
fn run_lint(command: &str, workspace: &std::path::Path) -> Option<String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", command]).current_dir(workspace);
    // Scrub secrets so lint commands (which may emit their env in debug
    // output) cannot expose API keys to the model via lint output.
    let to_strip: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| crate::tools::is_secret_env_var(k))
        .collect();
    for k in to_strip {
        cmd.env_remove(k);
    }
    let output = cmd.output().ok()?;
    if output.status.success() {
        None
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{stdout}{stderr}");
        // Truncate to ~2000 chars to avoid flooding the context
        let truncated = if combined.len() > 2000 {
            format!("{}…(truncated)", &combined[..2000])
        } else {
            combined
        };
        Some(truncated)
    }
}

/// Auto-commit changed files to git after a successful edit.
/// Best-effort: failures are silently ignored (the user may not be in
/// a git repo, or the file may be .gitignored).
fn auto_commit_changes(workspace: &std::path::Path, tool_name: &str) {
    // Stage all changes in the workspace
    let add = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(workspace)
        .output();
    if add.is_err() || !add.as_ref().unwrap().status.success() {
        return;
    }
    // Check if there's anything to commit
    let status = std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(workspace)
        .status();
    if status.map(|s| s.success()).unwrap_or(true) {
        return; // nothing staged
    }
    let _ = std::process::Command::new("git")
        .args(["commit", "-m", &format!("metis: {tool_name}")])
        .current_dir(workspace)
        .output();
}

/// Enrich tool error messages with recovery hints so the model can
/// self-correct without burning turns on the same mistake.
///
/// Also warns when consecutive errors pile up — if the model is stuck
/// in a loop, the hint tells it to stop and ask the user.
fn enrich_error_hint(tool_name: &str, error: &str, consecutive: u32) -> String {
    let mut enriched = error.to_string();
    let err_lower = error.to_lowercase();

    // ── Cross-tool generic patterns ──────────────────────────────────
    // These fire regardless of which tool produced the error, so the
    // model always gets a useful nudge for the most common failure modes.

    let mut matched_generic = false;

    if err_lower.contains("permission denied") || err_lower.contains("access denied") {
        enriched.push_str(
            "\n[hint] Permission denied. Check file/directory permissions with \
             `ls -la`, ensure the path is inside the workspace, or try \
             `chmod`/`sudo` if appropriate.",
        );
        matched_generic = true;
    }

    if !matched_generic
        && (err_lower.contains("no such file")
            || err_lower.contains("does not exist")
            || err_lower.contains("not found")
                && (err_lower.contains("file")
                    || err_lower.contains("path")
                    || err_lower.contains("directory")))
    {
        // Avoid firing on edit_file's "not found in file" (substring match)
        // — that has its own more specific hint below.
        let is_edit_substring = tool_name == "edit_file"
            && (err_lower.contains("not found in file") || err_lower.contains("editnotfound"));
        if !is_edit_substring {
            enriched.push_str(
                "\n[hint] File/path not found. Use `glob` or `ls` to list the directory \
                 and verify the correct path. Watch for typos or wrong extensions.",
            );
            matched_generic = true;
        }
    }

    if err_lower.contains("json")
        && (err_lower.contains("parse")
            || err_lower.contains("unexpected")
            || err_lower.contains("invalid")
            || err_lower.contains("syntax")
            || err_lower.contains("deserialize"))
    {
        enriched.push_str(
            "\n[hint] JSON parse error. Check for trailing commas, unquoted keys, \
             mismatched brackets, or invalid escape sequences in the input.",
        );
        matched_generic = true;
    }

    if err_lower.contains("timed out") || err_lower.contains("timeout") {
        enriched.push_str(
            "\n[hint] Operation timed out. Break it into smaller steps, increase \
             the timeout parameter, or use run_in_background: true for \
             long-running commands.",
        );
        matched_generic = true;
    }

    if err_lower.contains("command not found") || err_lower.contains("not recognized as") {
        enriched.push_str(
            "\n[hint] Command not found. Verify the tool/binary is installed and \
             on PATH. Consider using an alternative command or installing it first.",
        );
        matched_generic = true;
    }

    if err_lower.contains("connection refused")
        || err_lower.contains("connection reset")
        || err_lower.contains("network")
            && (err_lower.contains("error") || err_lower.contains("unreachable"))
        || err_lower.contains("dns") && (err_lower.contains("fail") || err_lower.contains("resolv"))
        || err_lower.contains("could not resolve")
    {
        enriched.push_str(
            "\n[hint] Network/connection error. This may be transient — wait a \
             moment and retry. If it persists, check network connectivity \
             or whether the target host is correct.",
        );
        matched_generic = true;
    }

    // ── Tool-specific recovery hints ─────────────────────────────────
    // These add precision on top of (or instead of) the generic patterns.

    match tool_name {
        "edit_file" => {
            if err_lower.contains("not found in file") || err_lower.contains("editnotfound") {
                enriched.push_str(
                    "\n[hint] The old_string was not found. Use read_file first to see \
                     the current file content, then retry with the exact text.",
                );
            } else if err_lower.contains("not unique") || err_lower.contains("editnotunique") {
                enriched.push_str(
                    "\n[hint] The old_string matches multiple locations. Include more \
                     surrounding context to make the match unique, or use replace_all: true.",
                );
            }
        }
        // Generic patterns above already cover file-not-found and
        // permission-denied; only add tool-specific extras here.
        "read_file" | "write_file" if err_lower.contains("is a directory") => {
            enriched.push_str(
                "\n[hint] The path points to a directory, not a file. Use glob \
                 or ls to list its contents and pick a specific file.",
            );
        }
        "bash"
            if (err_lower.contains("exit code") || err_lower.contains("exit status"))
                && !matched_generic =>
        {
            enriched.push_str(
                "\n[hint] The command exited with a non-zero status. Check \
                 the error output above for details on what went wrong.",
            );
        }
        "grep" | "glob" if err_lower.contains("no matches") || err_lower.contains("0 matches") => {
            enriched.push_str(
                "\n[hint] No matches found. Try a broader pattern, check for typos, \
                 or search in a different directory.",
            );
        }
        _ => {}
    }

    // ── Consecutive error escalation ─────────────────────────────────
    if consecutive >= 3 {
        enriched.push_str(&format!(
            "\n[warning] {} consecutive tool errors. STOP retrying the same approach. \
             Step back and reconsider: try a different strategy, re-read the \
             relevant files, or use ask_user to get guidance.",
            consecutive
        ));
    }

    enriched
}

/// Canonical "what did the model just try to do" fingerprint of a turn's
/// tool calls, used by loop detection to notice the model is repeating
/// itself verbatim. Joins name + argument JSON for each call with a
/// separator unlikely to collide with argument content. The arguments
/// string comes from the provider as-is, so this is byte-identical
/// across turns when the model truly repeats — which is exactly what
/// we want to catch.
fn tool_call_signature(calls: &[aegis_api::ToolCall]) -> String {
    calls
        .iter()
        .map(|c| format!("{}\u{1f}{}", c.function.name, c.function.arguments))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

/// Normalized fingerprint for assistant text used by the text-loop
/// guard. Returns `None` when the text is empty or shorter than a
/// threshold — short messages ("ok", "done") legitimately repeat and
/// shouldn't trip the guard.
///
/// Normalization strips case and whitespace runs so cosmetic edits
/// ("Trying again." vs "trying  again") still hash the same. Hash is
/// FxHash-style via `std::hash::DefaultHasher` — process-local, no
/// cryptographic property needed.
fn normalized_text_fingerprint(text: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let normalized: String = text
        .trim()
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace() || *c == ' ')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Below ~40 chars, repetition is not a "stuck loop" signal — short
    // confirmations ("ok", "done", "hazır") naturally repeat.
    if normalized.len() < 40 {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    Some(hasher.finish())
}

/// Clamp a potentially long signature down for inclusion in an error
/// message without losing the leading discriminator. Uses character
/// boundaries so it never panics on UTF-8 multibyte input.
fn truncate_for_error(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [{} chars total]", s.chars().count())
}

/// Format a tool result preview for the stream callback.
///
/// Most tools get a single-line truncated preview. `edit_file` and
/// `write_file` get a coloured diff preview showing changed lines
/// (red for removals, green for additions).
fn format_tool_preview(tool_name: &str, result: &str) -> String {
    if tool_name == "edit_file" || tool_name == "write_file" {
        let mut out = String::new();
        let mut diff_lines = 0;
        for line in result.lines() {
            if line.starts_with("edited ") || line.starts_with("wrote ") {
                out.push_str(line);
                out.push('\n');
            } else if line.starts_with('-') && !line.starts_with("---") {
                out.push_str(&format!("\x1b[31m{line}\x1b[0m\n"));
                diff_lines += 1;
            } else if line.starts_with('+') && !line.starts_with("+++") {
                out.push_str(&format!("\x1b[32m{line}\x1b[0m\n"));
                diff_lines += 1;
            } else if line.starts_with("@@") {
                out.push_str(&format!("\x1b[36m{line}\x1b[0m\n"));
            }
            if diff_lines > 20 {
                out.push_str(&format!(
                    "\x1b[2m  … ({} more lines)\x1b[0m\n",
                    result.lines().count().saturating_sub(20)
                ));
                break;
            }
        }
        if out.is_empty() {
            let first = result.lines().next().unwrap_or("");
            if first.len() > 120 {
                format!("{}…", &first[..120])
            } else {
                first.to_string()
            }
        } else {
            out.trim_end().to_string()
        }
    } else {
        let first = result.lines().next().unwrap_or("").to_string();
        if first.len() > 120 {
            format!("{}…", &first[..120])
        } else {
            first
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_default_has_sensible_turn_cap() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_turns, 100);
        assert_eq!(cfg.model, "deepseek-chat");
        assert!(cfg.system_prompt.is_none());
    }

    fn fake_call(name: &str, args: &str) -> aegis_api::ToolCall {
        aegis_api::ToolCall {
            id: format!("call_{name}"),
            kind: "function".to_string(),
            function: aegis_api::ToolCallFunction {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    #[test]
    fn signature_is_stable_across_equal_calls() {
        let a = tool_call_signature(&[fake_call("bash", r#"{"cmd":"ls"}"#)]);
        let b = tool_call_signature(&[fake_call("bash", r#"{"cmd":"ls"}"#)]);
        assert_eq!(a, b);
    }

    #[test]
    fn signature_differs_when_any_field_differs() {
        let base = tool_call_signature(&[fake_call("bash", r#"{"cmd":"ls"}"#)]);
        assert_ne!(
            base,
            tool_call_signature(&[fake_call("bash", r#"{"cmd":"pwd"}"#)])
        );
        assert_ne!(
            base,
            tool_call_signature(&[fake_call("read", r#"{"cmd":"ls"}"#)])
        );
    }

    #[test]
    fn signature_order_matters() {
        let a = tool_call_signature(&[fake_call("bash", "a"), fake_call("bash", "b")]);
        let b = tool_call_signature(&[fake_call("bash", "b"), fake_call("bash", "a")]);
        assert_ne!(a, b, "different order of identical tools must not collide");
    }

    #[test]
    fn text_fingerprint_short_text_returns_none() {
        // "ok", "done", and other short confirmations legitimately
        // repeat — the guard must skip them.
        assert_eq!(normalized_text_fingerprint(""), None);
        assert_eq!(normalized_text_fingerprint("ok"), None);
        assert_eq!(normalized_text_fingerprint("hazır"), None);
        assert_eq!(
            normalized_text_fingerprint("şimdilik bu kadar yeter sanırım"),
            None,
            "39 chars must still be below threshold"
        );
    }

    #[test]
    fn text_fingerprint_normalizes_whitespace_and_case() {
        // The model varies trailing whitespace and capitalization between
        // turns even when the paragraph is otherwise identical.
        // Normalization makes both versions hash the same so the
        // text-loop guard catches the repetition.
        let a =
            "GoFile bypass için farklı bir yöntem deneyelim. Şimdi başka bir yaklaşım deneyelim.";
        let b = "  gofile bypass için farklı bir yöntem deneyelim.\n  şimdi  başka bir yaklaşım deneyelim.  ";
        let fa = normalized_text_fingerprint(a).expect("long enough");
        let fb = normalized_text_fingerprint(b).expect("long enough");
        assert_eq!(fa, fb, "case + whitespace variants must collapse");
    }

    #[test]
    fn text_fingerprint_distinguishes_genuinely_different_paragraphs() {
        let a = "Reading the source file to understand the existing pattern.";
        let b = "Patching the source file to apply the requested change.";
        let fa = normalized_text_fingerprint(a).expect("long enough");
        let fb = normalized_text_fingerprint(b).expect("long enough");
        assert_ne!(fa, fb);
    }

    #[test]
    fn truncate_for_error_respects_char_boundaries() {
        // Mix ASCII + multi-byte so a naive byte slice would panic.
        let s = format!("{}🔥{}", "a".repeat(250), "b".repeat(10));
        let out = truncate_for_error(&s, 240);
        assert!(out.contains("chars total"));
        // Round-trips through char count without crashing on multibyte.
        assert!(out.chars().count() > 240);
    }

    #[test]
    fn preview_arguments_passes_short_input_through() {
        assert_eq!(
            preview_arguments(r#"{"path":"a.rs"}"#),
            r#"{"path":"a.rs"}"#
        );
    }

    #[test]
    fn preview_arguments_flattens_newlines() {
        let raw = "{\n  \"path\": \"a.rs\"\n}";
        let out = preview_arguments(raw);
        assert!(!out.contains('\n'));
        assert!(out.contains("\"path\""));
    }

    #[test]
    fn preview_arguments_truncates_past_120_chars() {
        let long = format!("{{\"blob\":\"{}\"}}", "x".repeat(500));
        let out = preview_arguments(&long);
        // 120 head chars plus the ellipsis char.
        assert_eq!(out.chars().count(), 121);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn agent_error_max_turns_carries_count() {
        let err = AgentError::MaxTurns(7);
        assert!(err.to_string().contains("7"));
    }

    #[test]
    fn is_transient_classifies_correctly() {
        assert!(is_transient(&ApiError::Status {
            status: 429,
            body: "rate limit".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 500,
            body: "".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 502,
            body: "".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 503,
            body: "".into()
        }));
        assert!(is_transient(&ApiError::Status {
            status: 504,
            body: "".into()
        }));
        assert!(!is_transient(&ApiError::Status {
            status: 400,
            body: "".into()
        }));
        assert!(!is_transient(&ApiError::Status {
            status: 401,
            body: "".into()
        }));
        assert!(!is_transient(&ApiError::Decode("bad json".into())));
        assert!(!is_transient(&ApiError::MissingKey("TEST_KEY")));
    }

    #[tokio::test]
    async fn retry_transient_succeeds_on_first_try() {
        let result = retry_transient_async(|| async { Ok::<_, ApiError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retry_transient_gives_up_on_non_transient() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result = retry_transient_async(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                Err::<i32, _>(ApiError::Status {
                    status: 400,
                    body: "bad request".into(),
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "non-transient errors should not be retried"
        );
    }

    #[tokio::test]
    async fn retry_transient_recovers_after_transient_failure() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result = retry_transient_async(|| {
            let c = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            async move {
                if c < 2 {
                    Err::<i32, _>(ApiError::Status {
                        status: 429,
                        body: "rate limit".into(),
                    })
                } else {
                    Ok(99)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn error_hint_edit_not_found() {
        let msg = super::enrich_error_hint("edit_file", "error: not found in file", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("read_file"));
    }

    #[test]
    fn error_hint_edit_not_unique() {
        let msg = super::enrich_error_hint("edit_file", "error: not unique (3 matches)", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("replace_all"));
    }

    #[test]
    fn error_hint_bash_timeout() {
        let msg = super::enrich_error_hint("bash", "error: timed out after 120s", 1);
        assert!(msg.contains("run_in_background"));
    }

    #[test]
    fn error_hint_consecutive_escalation() {
        let msg = super::enrich_error_hint("bash", "error: something", 3);
        assert!(msg.contains("[warning]"));
        assert!(msg.contains("3 consecutive"));
        assert!(msg.contains("ask_user"));
    }

    #[test]
    fn error_hint_no_escalation_below_3() {
        let msg = super::enrich_error_hint("bash", "error: something", 2);
        assert!(!msg.contains("[warning]"));
    }

    #[test]
    fn error_hint_permission_denied_generic() {
        let msg = super::enrich_error_hint("bash", "error: Permission denied", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("permissions"));
    }

    #[test]
    fn error_hint_permission_denied_on_any_tool() {
        let msg = super::enrich_error_hint("write_file", "Permission denied: /etc/passwd", 1);
        assert!(msg.contains("permissions"));
    }

    #[test]
    fn error_hint_file_not_found_generic() {
        let msg = super::enrich_error_hint("bash", "error: No such file or directory", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("glob"));
    }

    #[test]
    fn error_hint_json_parse() {
        let msg = super::enrich_error_hint("bash", "JSON parse error: unexpected token", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("trailing commas"));
    }

    #[test]
    fn error_hint_command_not_found_generic() {
        let msg = super::enrich_error_hint("bash", "zsh: command not found: foobar", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("installed"));
    }

    #[test]
    fn error_hint_connection_refused() {
        let msg = super::enrich_error_hint("bash", "error: connection refused", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("retry"));
    }

    #[test]
    fn error_hint_network_error() {
        let msg = super::enrich_error_hint("bash", "network error: host unreachable", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("connectivity"));
    }

    #[test]
    fn error_hint_dns_resolution() {
        let msg = super::enrich_error_hint("bash", "could not resolve host: example.com", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("retry"));
    }

    #[test]
    fn error_hint_is_a_directory() {
        let msg = super::enrich_error_hint("read_file", "error: Is a directory: /src", 1);
        assert!(msg.contains("[hint]"));
        assert!(msg.contains("directory"));
    }

    #[test]
    fn error_hint_consecutive_5_says_step_back() {
        let msg = super::enrich_error_hint("edit_file", "error: not found in file", 5);
        assert!(msg.contains("[warning]"));
        assert!(msg.contains("5 consecutive"));
        assert!(msg.contains("Step back"));
    }

    #[test]
    fn error_hint_edit_not_found_no_generic_file_hint() {
        // edit_file "not found in file" should NOT trigger the generic
        // file-not-found hint about glob/ls — only the edit-specific one.
        let msg = super::enrich_error_hint("edit_file", "error: not found in file", 1);
        assert!(msg.contains("read_file"));
        assert!(!msg.contains("Use `glob` or `ls` to list"));
    }

    #[test]
    fn preview_edit_shows_coloured_diff() {
        let result = "edited src/main.rs (1 replacement)\n\
            --- src/main.rs\n\
            +++ src/main.rs\n\
            @@ -1,3 +1,3 @@\n\
            -old line\n\
            +new line\n\
             context\n";
        let preview = super::format_tool_preview("edit_file", result);
        assert!(preview.contains("edited src/main.rs"));
        assert!(preview.contains("\x1b[31m-old line\x1b[0m"));
        assert!(preview.contains("\x1b[32m+new line\x1b[0m"));
        assert!(preview.contains("\x1b[36m@@"));
    }

    #[test]
    fn preview_other_tool_truncates() {
        let result = "found 5 files matching pattern\nextra detail";
        let preview = super::format_tool_preview("grep", result);
        assert_eq!(preview, "found 5 files matching pattern");
    }

    #[test]
    fn preview_long_line_truncates_at_120() {
        let long = "x".repeat(200);
        let preview = super::format_tool_preview("bash", &long);
        assert!(preview.starts_with("xxxx"));
        assert!(preview.ends_with('…'));
        // 120 ASCII chars + "…" (3 bytes UTF-8)
        assert_eq!(preview.chars().count(), 121);
    }

    #[test]
    fn user_input_from_string() {
        let input: UserInput = "hello".into();
        assert_eq!(input.text(), "hello");
        match input {
            UserInput::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn user_input_from_owned_string() {
        let input: UserInput = String::from("world").into();
        assert_eq!(input.text(), "world");
    }

    #[test]
    fn user_input_text_into_message() {
        let input = UserInput::Text("test".to_string());
        let msg = input.into_message();
        assert_eq!(msg.content.as_deref(), Some("test"));
        assert!(msg.content_blocks.is_empty());
    }

    #[test]
    fn user_input_multimodal_text_extraction() {
        let blocks = vec![
            ContentBlock::Text {
                text: "describe this".to_string(),
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "abc".to_string(),
            },
        ];
        let input = UserInput::Multimodal(blocks);
        assert_eq!(input.text(), "describe this");
    }

    #[test]
    fn user_input_multimodal_into_message() {
        let blocks = vec![
            ContentBlock::Text {
                text: "prompt".to_string(),
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "abc".to_string(),
            },
        ];
        let input = UserInput::Multimodal(blocks);
        let msg = input.into_message();
        assert!(msg.content.is_none());
        assert_eq!(msg.content_blocks.len(), 2);
    }

    #[test]
    fn user_input_with_images_reads_png() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("test.png");
        // Minimal valid PNG (1x1 pixel, red)
        let png_data = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        ];
        std::fs::write(&img_path, png_data).unwrap();
        let input = UserInput::with_images("describe", &[img_path]).unwrap();
        match input {
            UserInput::Multimodal(blocks) => {
                assert_eq!(blocks.len(), 2); // text + image
                match &blocks[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "describe"),
                    _ => panic!("expected text block"),
                }
                match &blocks[1] {
                    ContentBlock::Image { media_type, .. } => assert_eq!(media_type, "image/png"),
                    _ => panic!("expected image block"),
                }
            }
            _ => panic!("expected Multimodal"),
        }
    }

    #[test]
    fn user_input_with_images_skips_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let txt_path = dir.path().join("notes.txt");
        std::fs::write(&txt_path, "not an image").unwrap();
        let input = UserInput::with_images("prompt", &[txt_path]).unwrap();
        match input {
            UserInput::Multimodal(blocks) => {
                // Only text block — .txt was skipped
                assert_eq!(blocks.len(), 1);
            }
            _ => panic!("expected Multimodal"),
        }
    }

    #[test]
    fn user_input_multimodal_no_text_returns_empty() {
        let blocks = vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "abc".to_string(),
        }];
        let input = UserInput::Multimodal(blocks);
        assert_eq!(input.text(), "");
    }
}
