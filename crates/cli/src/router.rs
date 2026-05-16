//! Auto-model routing: pick the cheapest model that can handle the task.
//!
//! The router classifies each user prompt into a complexity tier and
//! selects the appropriate model. This lets Aegis use a fast/cheap model
//! for simple queries and a stronger model for complex tasks — cutting
//! cost without sacrificing quality where it matters.
//!
//! # Configuration
//!
//! In `config.toml`:
//!
//! ```toml
//! [routing]
//! auto_route = true
//! fast_model = "deepseek-chat"
//! strong_model = "deepseek-reasoner"
//! ```
//!
//! CLI `--model` always overrides the router.

use serde::Deserialize;

/// Complexity tier for a user prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// Simple: short questions, single-file reads, grep, explain, status.
    Fast,
    /// Complex: multi-file edits, refactoring, architecture, debugging,
    /// planning, code generation, multi-step tasks.
    Strong,
}

/// Routing configuration from config.toml.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Enable auto-routing. When false, always use the default model.
    pub auto_route: Option<bool>,
    /// Model for simple/fast tasks.
    pub fast_model: Option<String>,
    /// Model for complex/strong tasks.
    pub strong_model: Option<String>,
    /// Single legacy fallback. Deprecated in favour of `fallback_chain`,
    /// which supports an ordered list. Kept for config back-compat: when
    /// set, it's treated as the first entry of the chain.
    pub fallback_model: Option<String>,
    /// Ordered failover chain. Each entry is `"model"` or
    /// `"provider:model"`. When the primary client returns a transient
    /// error (5xx, 429, 408, network), `FailoverProvider` walks this
    /// list in order until one succeeds or the chain is exhausted.
    /// Per-link circuit breakers (3 consecutive transient failures →
    /// 60s blacklist) prevent re-hammering a known-down provider.
    ///
    /// Example:
    /// ```toml
    /// [routing]
    /// fallback_chain = ["gemini:gemini-2.5-flash", "nvidia:deepseek-ai/deepseek-v4-flash"]
    /// ```
    pub fallback_chain: Vec<String>,
    /// Where to retry when the primary refuses an image attachment with
    /// a "model doesn't support image input" 400. Same `provider:model`
    /// shape as `fallback_model`. Decoupled from the generic transient
    /// fallback so a vision-only retarget does not get triggered on
    /// every 5xx, and so a 5xx fallback to another text-only model
    /// does not silently swallow vision attachments.
    ///
    /// Example:
    /// ```toml
    /// [routing]
    /// vision_fallback = "nvidia:meta/llama-3.2-90b-vision-instruct"
    /// ```
    pub vision_fallback: Option<String>,
    /// Architect/editor swap: model used while the TUI is in `Plan`
    /// permission mode. Plan turns are read-only drafting, so a cheap
    /// model is usually enough. When unset, mode changes don't touch
    /// the active model. Same `provider:model` shape as the others.
    pub plan_model: Option<String>,
    /// Architect/editor swap: model used while the TUI is in
    /// `AcceptEdits` or `Bypass` permission mode — i.e. when the user
    /// has authorised real edits. Pairs with `plan_model`: keep cheap
    /// for drafting, swap up to a stronger model when actually
    /// modifying code. When unset, mode changes don't touch the model.
    pub build_model: Option<String>,
}

impl RoutingConfig {
    pub fn merge(self, other: RoutingConfig) -> RoutingConfig {
        RoutingConfig {
            auto_route: other.auto_route.or(self.auto_route),
            fast_model: other.fast_model.or(self.fast_model),
            strong_model: other.strong_model.or(self.strong_model),
            fallback_model: other.fallback_model.or(self.fallback_model),
            fallback_chain: if other.fallback_chain.is_empty() {
                self.fallback_chain
            } else {
                other.fallback_chain
            },
            vision_fallback: other.vision_fallback.or(self.vision_fallback),
            plan_model: other.plan_model.or(self.plan_model),
            build_model: other.build_model.or(self.build_model),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.auto_route.unwrap_or(false)
    }

    /// Pick which fallback target the REPL should retry against after a
    /// primary-call failure. The classifier looks at the error text:
    /// when it carries the client-side "doesn't support image input"
    /// rejection emitted by `OpenAICompatClient`, the picker prefers
    /// `vision_fallback` so the retry actually lands on a vision-capable
    /// provider — falling back to `fallback_model` here would just hit
    /// the same 400 again on another text-only model. For every other
    /// error class, the generic transient `fallback_model` wins.
    ///
    /// `original` is the routing config before any per-turn override
    /// (e.g. the auto-router temporarily disabling itself); both layers
    /// are checked so a config that lives in only one of them is still
    /// honoured.
    pub fn select_retry_fallback(
        err_text: &str,
        primary: &RoutingConfig,
        original: &RoutingConfig,
    ) -> Option<String> {
        let is_image_error = err_text.contains("doesn't support image input");
        if is_image_error {
            primary
                .vision_fallback
                .clone()
                .or_else(|| original.vision_fallback.clone())
                .or_else(|| primary.fallback_model.clone())
                .or_else(|| original.fallback_model.clone())
        } else {
            primary
                .fallback_model
                .clone()
                .or_else(|| original.fallback_model.clone())
        }
    }

    /// Build the resolved failover chain by combining the legacy
    /// `fallback_model` (if set) with `fallback_chain`. The legacy
    /// entry is prepended so existing configs keep working unchanged
    /// while new configs can express richer chains.
    pub fn resolved_fallback_chain(&self) -> Vec<String> {
        let mut chain = Vec::new();
        if let Some(legacy) = &self.fallback_model {
            chain.push(legacy.clone());
        }
        for entry in &self.fallback_chain {
            if !chain.iter().any(|existing| existing == entry) {
                chain.push(entry.clone());
            }
        }
        chain
    }
}

/// Heuristic signals that push toward the Strong tier.
const STRONG_SIGNALS: &[&str] = &[
    // Multi-file / large scope
    "refactor",
    "restructure",
    "migrate",
    "rewrite",
    "redesign",
    "rearchitect",
    "overhaul",
    // Code generation
    "implement",
    "build",
    "create",
    "write a",
    "add feature",
    "new feature",
    // Debugging
    "debug",
    "fix bug",
    "investigate",
    "root cause",
    "diagnose",
    // Planning / architecture
    "plan",
    "design",
    "architect",
    "strategy",
    "roadmap",
    // Multi-step
    "step by step",
    "then",
    "after that",
    "first.*then",
    // Complex analysis
    "analyze",
    "review",
    "audit",
    "optimize",
    "performance",
    "security",
];

/// Heuristic signals that suggest the Fast tier is sufficient.
const FAST_SIGNALS: &[&str] = &[
    // Simple queries
    "what is",
    "what does",
    "how does",
    "where is",
    "show me",
    "find",
    "search",
    "list",
    "grep",
    "read",
    "cat",
    "explain",
    "describe",
    // Status
    "status",
    "diff",
    "log",
    "history",
    // Quick fixes
    "rename",
    "typo",
    "format",
    "lint",
];

/// Classify a user prompt into a complexity tier.
///
/// Uses keyword matching + length heuristics. This is intentionally
/// simple — a more sophisticated classifier could use embeddings or
/// an LLM call, but the latency/cost trade-off isn't worth it for
/// routing decisions.
pub fn classify(prompt: &str) -> ModelTier {
    let lower = prompt.to_lowercase();
    let word_count = prompt.split_whitespace().count();

    // Very short prompts (< 8 words) are almost always simple.
    if word_count < 8 {
        // Unless they contain strong signals
        let has_strong = STRONG_SIGNALS.iter().any(|s| lower.contains(s));
        if has_strong {
            return ModelTier::Strong;
        }
        return ModelTier::Fast;
    }

    // Count signal matches
    let strong_count = STRONG_SIGNALS
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
    let fast_count = FAST_SIGNALS.iter().filter(|s| lower.contains(**s)).count();

    // Long prompts (> 50 words) lean toward Strong — the user is
    // giving detailed instructions, which implies a complex task.
    let length_bonus = if word_count > 50 { 2 } else { 0 };

    // Multi-file indicators: multiple file paths or "files" plural
    let multi_file = lower.contains("files")
        || lower.matches('/').count() > 2
        || lower.contains(" and ")
            && (lower.contains(".rs")
                || lower.contains(".ts")
                || lower.contains(".py")
                || lower.contains(".go"));

    let strong_score = strong_count + length_bonus + if multi_file { 2 } else { 0 };

    if strong_score > fast_count {
        ModelTier::Strong
    } else {
        ModelTier::Fast
    }
}

/// Parsed routing target: model name + optional provider override.
///
/// Config supports `"provider:model"` syntax, e.g. `"glm:glm-5.1"`.
/// When the provider prefix is present the caller must switch the active
/// client in addition to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub model: String,
    /// Non-None when the config entry contained a `provider:model` prefix.
    pub provider: Option<String>,
}

impl RouteTarget {
    pub fn parse(s: &str) -> Self {
        if let Some((provider, model)) = s.split_once(':') {
            RouteTarget {
                model: model.to_string(),
                provider: Some(provider.to_string()),
            }
        } else {
            RouteTarget {
                model: s.to_string(),
                provider: None,
            }
        }
    }
}

/// Pick the model based on routing config and prompt classification.
/// Returns None if routing is disabled (caller should use default model).
pub fn route(config: &RoutingConfig, prompt: &str) -> Option<RouteTarget> {
    if !config.is_enabled() {
        return None;
    }

    let tier = classify(prompt);
    let raw = match tier {
        ModelTier::Fast => config.fast_model.as_deref(),
        ModelTier::Strong => config.strong_model.as_deref(),
    }?;
    Some(RouteTarget::parse(raw))
}

/// Architect/editor swap: pick a model for a TUI permission-mode
/// transition. Independent from `route()` (which classifies prompts) —
/// this keys solely off the mode label so the swap is deterministic and
/// happens at mode-change time, not per-turn.
///
/// Mode mapping (case-insensitive, dash/underscore tolerant):
/// - `plan` → `plan_model`
/// - `accept-edits` / `accept_edits` / `acceptedits` / `bypass` / `yolo`
///   → `build_model`
/// - `default` or anything unrecognised → `None`
///
/// Returns `None` when no model is configured for the requested mode,
/// so callers can no-op cleanly without forcing an opt-in flag.
///
/// Currently only the test suite exercises this entry point — the TUI
/// performs the swap inline in `set_permission_mode` against the same
/// rules. The helper is kept (and lint-suppressed) so future callers
/// outside the TUI hot path don't have to re-derive the mapping.
#[allow(dead_code)]
pub fn pick_for_mode(config: &RoutingConfig, mode_label: &str) -> Option<RouteTarget> {
    let key = mode_label.trim().to_lowercase();
    let raw = match key.as_str() {
        "plan" => config.plan_model.as_deref(),
        "accept-edits" | "accept_edits" | "acceptedits" | "bypass" | "yolo" => {
            config.build_model.as_deref()
        }
        _ => None,
    }?;
    Some(RouteTarget::parse(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_simple_prompt_is_fast() {
        assert_eq!(classify("what does this function do?"), ModelTier::Fast);
    }

    #[test]
    fn short_with_strong_signal_is_strong() {
        assert_eq!(classify("refactor the auth module"), ModelTier::Strong);
    }

    #[test]
    fn explain_is_fast() {
        assert_eq!(
            classify("explain how the compaction works"),
            ModelTier::Fast
        );
    }

    #[test]
    fn multi_file_refactor_is_strong() {
        assert_eq!(
            classify("refactor the error handling across src/agent.rs and src/tools.rs"),
            ModelTier::Strong
        );
    }

    #[test]
    fn implement_feature_is_strong() {
        assert_eq!(
            classify("implement a new caching layer for the provider responses"),
            ModelTier::Strong
        );
    }

    #[test]
    fn grep_is_fast() {
        assert_eq!(classify("grep for TODO comments"), ModelTier::Fast);
    }

    #[test]
    fn long_detailed_prompt_is_strong() {
        let prompt = "I need you to analyze the entire codebase and find all places \
            where we handle errors incorrectly. Then refactor them to use the new \
            error type we defined. Make sure all tests pass after the change and \
            update the documentation to reflect the new error handling approach.";
        assert_eq!(classify(prompt), ModelTier::Strong);
    }

    #[test]
    fn route_disabled_returns_none() {
        let cfg = RoutingConfig::default();
        assert_eq!(route(&cfg, "anything"), None);
    }

    /// Regression: `--provider gemini --model gemini-2.5-flash` on a
    /// simple prompt used to get rewritten to the globally-configured
    /// `fast_model` (often `deepseek-chat`) while the provider stayed
    /// on Gemini, producing a 404 NOT_FOUND at request time. main.rs
    /// now force-disables `auto_route` whenever the user sets either
    /// `--model` or `--provider`, so `route()` must return `None` when
    /// the config reflects that.
    #[test]
    fn explicit_model_flag_disables_routing() {
        let disabled = RoutingConfig {
            auto_route: Some(false),
            fast_model: Some("deepseek-chat".into()),
            strong_model: Some("deepseek-reasoner".into()),
            fallback_model: None,
            fallback_chain: vec![],
            vision_fallback: None,
            plan_model: None,
            build_model: None,
        };
        assert_eq!(route(&disabled, "what is 2+2?"), None);
        assert_eq!(
            route(&disabled, "refactor this whole module end to end"),
            None
        );
    }

    #[test]
    fn route_enabled_returns_model() {
        let cfg = RoutingConfig {
            auto_route: Some(true),
            fast_model: Some("deepseek-v4-flash".into()),
            strong_model: Some("deepseek-v4-pro".into()),
            fallback_model: None,
            fallback_chain: vec![],
            vision_fallback: None,
            plan_model: None,
            build_model: None,
        };
        assert_eq!(
            route(&cfg, "what is this?"),
            Some(RouteTarget {
                model: "deepseek-v4-flash".into(),
                provider: None
            })
        );
        assert_eq!(
            route(&cfg, "refactor the entire auth system"),
            Some(RouteTarget {
                model: "deepseek-v4-pro".into(),
                provider: None
            })
        );
    }

    #[test]
    fn route_target_parses_provider_prefix() {
        let t = RouteTarget::parse("glm:glm-5.1");
        assert_eq!(t.model, "glm-5.1");
        assert_eq!(t.provider, Some("glm".into()));

        let t2 = RouteTarget::parse("gemini-2.5-flash");
        assert_eq!(t2.model, "gemini-2.5-flash");
        assert_eq!(t2.provider, None);
    }

    #[test]
    fn route_cross_provider() {
        let cfg = RoutingConfig {
            auto_route: Some(true),
            fast_model: Some("gemini-2.5-flash".into()),
            strong_model: Some("glm:glm-5.1".into()),
            fallback_model: None,
            fallback_chain: vec![],
            vision_fallback: None,
            plan_model: None,
            build_model: None,
        };
        assert_eq!(
            route(&cfg, "refactor the entire auth system"),
            Some(RouteTarget {
                model: "glm-5.1".into(),
                provider: Some("glm".into())
            })
        );
    }

    fn empty_routing() -> RoutingConfig {
        RoutingConfig {
            auto_route: None,
            fast_model: None,
            strong_model: None,
            fallback_model: None,
            fallback_chain: vec![],
            vision_fallback: None,
            plan_model: None,
            build_model: None,
        }
    }

    #[test]
    fn select_retry_fallback_image_error_prefers_vision() {
        // The exact error string `OpenAICompatClient::chat` emits when
        // a text-only model gets handed an image. The classifier must
        // route the retry to `vision_fallback`, not `fallback_model`,
        // otherwise the retry lands on another text-only model and
        // hits the same client-side 400 a second time.
        let primary = RoutingConfig {
            fallback_model: Some("gemini:gemini-2.5-flash".into()),
            vision_fallback: Some("nvidia:meta/llama-3.2-90b-vision-instruct".into()),
            ..empty_routing()
        };
        let original = empty_routing();
        let err = "model `deepseek-v4-flash` doesn't support image input. Switch to a vision-capable model";
        assert_eq!(
            RoutingConfig::select_retry_fallback(err, &primary, &original),
            Some("nvidia:meta/llama-3.2-90b-vision-instruct".into())
        );
    }

    #[test]
    fn select_retry_fallback_image_error_falls_back_to_generic_when_no_vision() {
        // No vision_fallback configured, but a generic fallback_model
        // exists. The classifier still has to return *something* so the
        // REPL doesn't just give up — and the user can at least see the
        // retry path even if the second model also rejects images.
        let primary = RoutingConfig {
            fallback_model: Some("openai:gpt-4o".into()),
            vision_fallback: None,
            ..empty_routing()
        };
        let original = empty_routing();
        let err = "doesn't support image input";
        assert_eq!(
            RoutingConfig::select_retry_fallback(err, &primary, &original),
            Some("openai:gpt-4o".into())
        );
    }

    #[test]
    fn select_retry_fallback_generic_error_ignores_vision_fallback() {
        // A 5xx / network error has nothing to do with attachments, so
        // `vision_fallback` must NOT steal the retry — using a vision
        // model for every transient blip would burn the bigger model's
        // quota for no reason.
        let primary = RoutingConfig {
            fallback_model: Some("openai:gpt-4o-mini".into()),
            vision_fallback: Some("nvidia:meta/llama-3.2-90b-vision-instruct".into()),
            ..empty_routing()
        };
        let original = empty_routing();
        let err = "503 Service Unavailable";
        assert_eq!(
            RoutingConfig::select_retry_fallback(err, &primary, &original),
            Some("openai:gpt-4o-mini".into())
        );
    }

    #[test]
    fn select_retry_fallback_falls_through_to_original_layer() {
        // `routing` is the per-turn override (e.g. auto-router off);
        // `original` is the on-disk config. When the override layer is
        // empty, the picker still has to honour vision_fallback set on
        // the original config — otherwise the auto-router's per-turn
        // adjustments would silently disable image fallback.
        let primary = empty_routing();
        let original = RoutingConfig {
            vision_fallback: Some("nvidia:meta/llama-3.2-11b-vision-instruct".into()),
            ..empty_routing()
        };
        let err = "model `x` doesn't support image input";
        assert_eq!(
            RoutingConfig::select_retry_fallback(err, &primary, &original),
            Some("nvidia:meta/llama-3.2-11b-vision-instruct".into())
        );
    }

    #[test]
    fn select_retry_fallback_returns_none_when_nothing_configured() {
        let primary = empty_routing();
        let original = empty_routing();
        assert_eq!(
            RoutingConfig::select_retry_fallback("anything", &primary, &original),
            None
        );
    }

    #[test]
    fn rename_is_fast() {
        assert_eq!(classify("rename the variable foo to bar"), ModelTier::Fast);
    }

    #[test]
    fn debug_is_strong() {
        assert_eq!(
            classify("debug why the tests are failing on CI"),
            ModelTier::Strong
        );
    }

    #[test]
    fn pick_for_mode_plan_uses_plan_model() {
        let cfg = RoutingConfig {
            plan_model: Some("deepseek-v4-flash".into()),
            build_model: Some("deepseek-v4-pro".into()),
            ..empty_routing()
        };
        assert_eq!(
            pick_for_mode(&cfg, "plan"),
            Some(RouteTarget {
                model: "deepseek-v4-flash".into(),
                provider: None,
            })
        );
    }

    #[test]
    fn pick_for_mode_accept_edits_and_bypass_use_build_model() {
        // AcceptEdits and Bypass both authorise real edits — the spec
        // is that a single `build_model` covers both, so the user
        // doesn't have to think about Bypass as a separate tier.
        let cfg = RoutingConfig {
            plan_model: Some("flash".into()),
            build_model: Some("nvidia:deepseek-v4-pro".into()),
            ..empty_routing()
        };
        let want = Some(RouteTarget {
            model: "deepseek-v4-pro".into(),
            provider: Some("nvidia".into()),
        });
        assert_eq!(pick_for_mode(&cfg, "accept-edits"), want);
        assert_eq!(pick_for_mode(&cfg, "accept_edits"), want);
        assert_eq!(pick_for_mode(&cfg, "bypass"), want);
        assert_eq!(pick_for_mode(&cfg, "yolo"), want);
    }

    #[test]
    fn pick_for_mode_default_returns_none() {
        // Default mode must never override — the user's CLI/--model
        // pick stays authoritative until they explicitly cycle modes.
        let cfg = RoutingConfig {
            plan_model: Some("flash".into()),
            build_model: Some("pro".into()),
            ..empty_routing()
        };
        assert_eq!(pick_for_mode(&cfg, "default"), None);
        assert_eq!(pick_for_mode(&cfg, ""), None);
        assert_eq!(pick_for_mode(&cfg, "garbage"), None);
    }

    #[test]
    fn pick_for_mode_unset_model_returns_none() {
        // Plan/Build not configured for this mode → no swap. Lets the
        // feature stay opt-in without a separate flag: if you want the
        // swap, set the field; if not, mode changes are model-neutral.
        let cfg = RoutingConfig {
            plan_model: Some("flash".into()),
            // build_model intentionally absent
            ..empty_routing()
        };
        assert_eq!(pick_for_mode(&cfg, "bypass"), None);
        assert_eq!(
            pick_for_mode(&cfg, "plan"),
            Some(RouteTarget {
                model: "flash".into(),
                provider: None,
            })
        );
    }
}
