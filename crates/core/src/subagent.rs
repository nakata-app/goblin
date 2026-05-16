//! Subagent infrastructure — Session 30.
//!
//! A subagent is a child [`Agent`] spawned with its own fresh transcript
//! and isolated session. The parent's conversation, session store and
//! usage counters are untouched; only a [`SubagentReport`] crosses the
//! boundary back to the caller.
//!
//! This is the spine that later sessions (S31 — general-purpose type,
//! S32 — parallel execution, S36 — Explore/Plan derived types) build on.
//! Keep it minimal and explicit: no shared mutable state, no implicit
//! parent context bleed.
//!
//! Session 32 added [`Subagent::spawn_parallel`], which fans a list of
//! briefs out onto scoped OS threads and collects `Result`s back in the
//! original input order. Each thread builds an independent child
//! [`Agent`] so there is still no shared mutable state across workers —
//! parallelism here is about wall-clock latency, not shared context.

use std::sync::Arc;

use aegis_api::ChatProvider;
use serde_json::Value;

use crate::agent::{Agent, AgentConfig, AgentError};
use crate::cost::UsageSnapshot;
use crate::permission::{AllowAll, Permission, PermissionDecision};
use crate::tools::{ToolContext, ToolRegistry};

/// A named subagent flavour. Each type bundles a default system
/// prompt and an optional tool allowlist so the parent can pick a
/// pre-configured personality instead of wiring everything by hand.
///
/// S31 ships exactly one type — [`SubagentType::general_purpose`] —
/// but the surface is built so S36 can layer on `Explore` and `Plan`
/// without breaking callers.
#[derive(Debug, Clone)]
pub struct SubagentType {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// Tool allowlist. `None` means "inherit the parent's full
    /// registry"; `Some(vec)` means "only these tool names are
    /// callable, every other tool is denied at the permission gate".
    pub allowed_tools: Option<Vec<String>>,
}

impl SubagentType {
    /// The default flavour: full tool access, generic system prompt.
    /// Used when the parent has no specialisation in mind and just
    /// wants a fresh agent with an isolated transcript.
    /// The default flavour: full tool access, generic system prompt.
    pub fn general_purpose() -> Self {
        Self {
            name: "general-purpose".to_string(),
            description:
                "General-purpose agent for researching complex questions, searching for code, \
                 and executing multi-step tasks. Has access to the full tool set."
                    .to_string(),
            system_prompt:
                "You are a focused subagent. You have no memory of any prior conversation — \
                 the brief you receive is your only context. Read it carefully, do the work, \
                 and return a single self-contained answer. Be concise."
                    .to_string(),
            allowed_tools: None,
        }
    }

    /// Read-only codebase explorer. Can search files and read code but
    /// cannot modify anything.
    pub fn explore() -> Self {
        Self {
            name: "explore".to_string(),
            description:
                "Fast agent for exploring codebases. Searches files, reads code, \
                 and answers questions about the codebase. Read-only — cannot edit or run commands."
                    .to_string(),
            system_prompt:
                "You are an Explore subagent. Your job is to search and read the codebase \
                 to answer questions or find information. You cannot modify files or run \
                 shell commands. Read the brief carefully, search thoroughly, and return \
                 a single self-contained answer with file paths and line numbers."
                    .to_string(),
            allowed_tools: Some(vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "web_fetch".to_string(),
                "web_search".to_string(),
                "lsp".to_string(),
                "tool_search".to_string(),
            ]),
        }
    }

    /// Read-only architecture planner. Can research but cannot modify.
    pub fn plan() -> Self {
        Self {
            name: "plan".to_string(),
            description: "Software architect agent for designing implementation plans. Returns \
                 step-by-step plans with file paths and trade-offs. Read-only."
                .to_string(),
            system_prompt:
                "You are a Plan subagent — a software architect. Your job is to research \
                 the codebase and design an implementation plan. You cannot modify files \
                 or run shell commands. Return a structured plan with: critical files, \
                 step-by-step approach, dependencies, and trade-offs."
                    .to_string(),
            allowed_tools: Some(vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "web_fetch".to_string(),
                "web_search".to_string(),
                "lsp".to_string(),
                "tool_search".to_string(),
            ]),
        }
    }

    /// Look up a type by name. Returns `None` for unknown names.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "general-purpose" => Some(Self::general_purpose()),
            "explore" => Some(Self::explore()),
            "plan" => Some(Self::plan()),
            _ => None,
        }
    }
}

/// Permission wrapper enforcing a tool name allowlist. Any tool not
/// in the list is denied with a clear reason; the inner permission
/// is consulted for everything else.
struct AllowlistPermission {
    inner: Arc<dyn Permission>,
    allowed: Vec<String>,
}

impl Permission for AllowlistPermission {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        if !self.allowed.iter().any(|t| t == tool) {
            return PermissionDecision::Deny(format!(
                "tool `{tool}` is not in this subagent's allowlist"
            ));
        }
        self.inner.check(tool, args)
    }
}

/// Format a brief into the child's first user message. The shape is
/// deliberately stable and verifiable from tests so the briefing
/// protocol is part of the contract, not a free-form decision.
pub fn format_briefing(brief: &SubagentBrief) -> String {
    format!("# Task: {}\n\n{}", brief.description, brief.prompt)
}

/// Briefing handed to a subagent at spawn time. Mirrors the way the
/// Agent tool is invoked from a parent agent: a short human-readable
/// description plus the actual prompt the subagent should answer.
#[derive(Debug, Clone)]
pub struct SubagentBrief {
    /// Short label identifying the task — surfaced back in the report
    /// so the parent can tell which spawn produced which output when
    /// many run in parallel (S32).
    pub description: String,
    /// The actual user-style prompt the subagent will run.
    pub prompt: String,
    /// Optional system prompt override. If `None` the parent's
    /// `AgentConfig::system_prompt` is reused unchanged.
    pub system_prompt: Option<String>,
}

/// What a subagent returns to its parent. Crucially this is a *value*,
/// not a handle — once the subagent returns, its transcript is dropped
/// and no further interaction is possible. The parent only sees the
/// final text and the aggregate usage.
#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub description: String,
    pub final_text: String,
    pub usage: UsageSnapshot,
    pub turns: usize,
}

/// Spawner for child agents. Holds borrows to the shared provider,
/// tool registry and a base config; each `spawn` builds a fresh
/// [`Agent`] from these and runs it to completion.
pub struct Subagent<'a> {
    client: &'a dyn ChatProvider,
    registry: &'a ToolRegistry,
    ctx: ToolContext,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
}

impl<'a> Subagent<'a> {
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
            permission: Arc::new(AllowAll),
        }
    }

    /// Installs a [`Permission`] gate that every spawned subagent will
    /// inherit. Useful for sandboxing: a parent running with full
    /// permissions can still spawn children locked behind a stricter
    /// policy.
    pub fn with_permission(mut self, permission: Arc<dyn Permission>) -> Self {
        self.permission = permission;
        self
    }

    /// Spawn a fresh child agent, run it on the given brief, and return
    /// the report. The child has:
    ///
    /// * its own transcript starting from an empty `Vec`
    /// * **no** session store attached (parent's session is never
    ///   touched, and the child's transcript is discarded on return)
    /// * an independent [`UsageSnapshot`] reported back to the caller
    pub async fn spawn(&self, brief: SubagentBrief) -> Result<SubagentReport, AgentError> {
        let mut config = self.config.clone();
        if let Some(sp) = brief.system_prompt {
            config.system_prompt = Some(sp);
        }
        let mut agent = Agent::new(self.client, self.registry, self.ctx.clone(), config)
            .with_permission(self.permission.clone());
        let output = agent.run(brief.prompt).await?;
        Ok(SubagentReport {
            description: brief.description,
            final_text: output.final_text,
            usage: output.usage,
            turns: output.turns,
        })
    }

    /// Fan a batch of briefs out onto scoped OS threads and collect the
    /// results back in input order.
    ///
    /// Each brief runs in its own thread, each thread builds its own
    /// fresh [`Agent`] from this spawner's shared provider, registry and
    /// context, and each thread's result lands at the same index its
    /// brief had in the input vector. An error in one thread does not
    /// cancel the others — every brief either produces an `Ok(report)`
    /// or an `Err(AgentError)`, and the parent's state is untouched
    /// exactly as for sequential [`Self::spawn`].
    ///
    /// Ordering, isolation and parent-state invariants are pinned by
    /// `failure_driven.rs` Session 32; any change to the thread fan-out
    /// must keep those tests green.
    ///
    /// # Panics and cancellation
    ///
    /// A panic inside a worker thread is caught at join time and
    /// surfaced as `Err(AgentError::Config(...))` — it does not unwind
    /// into the caller. An empty `briefs` input returns an empty vector
    /// without spawning any threads.
    pub async fn spawn_parallel(
        &self,
        briefs: Vec<SubagentBrief>,
    ) -> Vec<Result<SubagentReport, AgentError>> {
        if briefs.is_empty() {
            return Vec::new();
        }
        // Run all briefs concurrently via join_all. Each future builds
        // an independent child Agent so there is no shared mutable state
        // across workers. join_all polls all futures on the current task,
        // which is ideal for network-bound provider calls.
        let futs: Vec<_> = briefs.into_iter().map(|b| self.spawn(b)).collect();
        futures::future::join_all(futs).await
    }

    /// Spawn a child using a [`SubagentType`]'s defaults:
    ///
    /// * the type's `system_prompt` becomes the child's system prompt
    ///   (a non-`None` `brief.system_prompt` still wins)
    /// * the type's `allowed_tools`, if any, wraps the parent
    ///   permission with an allowlist gate
    /// * the brief is formatted via [`format_briefing`] before being
    ///   handed to the child as its first user message
    pub async fn spawn_typed(
        &self,
        ty: &SubagentType,
        brief: SubagentBrief,
    ) -> Result<SubagentReport, AgentError> {
        let mut config = self.config.clone();
        config.system_prompt = Some(
            brief
                .system_prompt
                .clone()
                .unwrap_or_else(|| ty.system_prompt.clone()),
        );

        let permission: Arc<dyn Permission> = match &ty.allowed_tools {
            Some(list) => Arc::new(AllowlistPermission {
                inner: self.permission.clone(),
                allowed: list.clone(),
            }),
            None => self.permission.clone(),
        };

        let mut agent = Agent::new(self.client, self.registry, self.ctx.clone(), config)
            .with_permission(permission);
        let formatted = format_briefing(&brief);
        let output = agent.run(formatted).await?;
        Ok(SubagentReport {
            description: brief.description,
            final_text: output.final_text,
            usage: output.usage,
            turns: output.turns,
        })
    }
}
