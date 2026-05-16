//! Core building blocks for the `aegis` agent CLI: tools, agent loop,
//! session persistence, and the cost meter.
//!
//! In v0.1 this crate is intentionally tiny — Session 1 only ships the
//! cost meter and the public re-export surface. Tool execution and the
//! agent loop arrive in Session 2.

pub mod agent;
pub mod autonomous_security;
pub mod bash_safety;
pub mod context_primer;
#[cfg(feature = "ctx")]
pub mod blob_index;
#[cfg(feature = "ctx")]
pub mod blob_store;
pub mod compaction;
pub mod cost;
pub mod cron;
pub mod display;
pub mod execution;
pub mod feature_permission;
pub mod features;
pub mod guardrail;
pub mod halluguard;
pub mod hooks;
pub mod lang_hints;
pub mod learning;
pub mod mcp_cache;
pub mod memory;
pub mod permission;
pub mod prompt_mod;
#[cfg(feature = "policy")]
pub mod policy;
pub mod repomap;
#[cfg(feature = "ctx")]
pub mod sandbox_exec;
pub mod search;
pub mod session;
pub mod skills;
pub mod subagent;
pub mod telemetry;
pub mod tools;
pub mod update;
#[cfg(feature = "wasm")]
pub mod wasi_sandbox;

pub use agent::{run_simple, Agent, AgentConfig, AgentError, AgentOutput, UserInput};
pub use autonomous_security::{
    AutonomousSecurityConfig, AutonomousSecurityLayer, KillSwitchState, SecurityStatsSnapshot,
};
#[cfg(feature = "ctx")]
pub use blob_index::{BlobIndex, IndexError, SearchHit};
#[cfg(feature = "ctx")]
pub use blob_store::{BlobError, BlobId, BlobMeta, BlobStats, BlobStore, ID_PREFIX_LEN};
pub use compaction::{llm_summarizer, CompactionConfig, Summarizer};
pub use cost::{
    format_cost_breakdown, format_cost_delta, format_cost_footer, ModelPricing, Pricing, TokenCost,
    UsageSnapshot,
};
pub use feature_permission::{FeatureConfig, FeaturePermission};
pub use hooks::{
    format_hook_output, load_hooks, run_hooks, HookConfig, HookEntry, HookEvent, HookOutcome,
    HookResult, OnFail,
};
pub use mcp_cache::{McpCache, McpCacheEntry, DEFAULT_TTL_SECS};
pub use memory::{
    format_memory_file, parse_memory_file, MemoryEntry, MemoryError, MemoryMeta, MemoryStore,
    MemoryType,
};
pub use permission::{
    build_edit_preview, AcceptEditsPermission, AllowAll, AuditingPermission, DenyAll, Permission,
    PermissionDecision, PolicyPermission,
};
#[cfg(feature = "ctx")]
pub use sandbox_exec::{
    SandboxConfig, SandboxedTool, DEFAULT_PREVIEW_BYTES, DEFAULT_THRESHOLD_BYTES,
};
pub use session::{RecoveryStats, SessionError, SessionStore, SessionSummary};
pub use skills::{builtin_skills, expand_prompt, expand_prompt_full, Skill, SkillRegistry};
pub use subagent::{format_briefing, Subagent, SubagentBrief, SubagentReport, SubagentType};
pub use telemetry::{spent_on, spent_today, today_date, BudgetStatus};
pub use tools::{
    read_wakeup_hint, register_mcp_server, spawn_mcp_server, spawn_mcp_server_with_cache,
    AgentSpawnRequest, AgentSpawnerFn, BackgroundAgents, BackgroundResult, McpTool, PlanState,
    SandboxMode, SpawnedMcpServer, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
    UserInputFn,
};
