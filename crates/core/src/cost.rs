//! Per-prompt token and cost reporting.
//!
//! `aegis`'s headline feature is that every one-shot run finishes with a
//! one-line note on stderr telling you exactly what the prompt cost. This
//! module owns the data types and the formatter behind that line.
//!
//! Design notes:
//!
//! * **Always return a number.** [`ModelPricing::resolve`] never returns
//!   `Option`. If the model name does not match a known entry in the
//!   registry, the resolver hands back a generic mid-tier estimate with
//!   `is_estimated == true`. Callers branch on the flag for the display
//!   marker, never for the math.
//! * **Cache pricing is first-class as of v0.11.** `UsageSnapshot`
//!   carries `cache_read` and `cache_write` counters alongside the
//!   fresh input count, and `ModelPricing` holds per-model multipliers
//!   (cache writes cost a premium on Anthropic, cache reads cost a
//!   small fraction everywhere). The footer only surfaces the cache
//!   segment when at least one of the two counters is non-zero, so
//!   runs against providers without caching read identically to v0.1.
//! * **Substring lookup, not prefix lookup.** Model strings users pass
//!   in vary: `deepseek-chat`, `deepseek/deepseek-chat`, even
//!   `accounts/fireworks/models/deepseek-chat`. A case-insensitive
//!   substring check is more forgiving than a strict prefix match.

/// Per-million-token rates for a single model in US dollars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Cost of a cache **write** token as a multiple of the base input
    /// rate. Anthropic charges 1.25x for ephemeral-cache creation;
    /// OpenAI charges nothing separately for cache writes and uses 0.0.
    pub cache_write_multiplier: f64,
    /// Cost of a cache **read** token as a multiple of the base input
    /// rate. Anthropic charges ~0.10x for ephemeral-cache hits; OpenAI
    /// charges ~0.50x for its automatic prefix cache hits.
    pub cache_read_multiplier: f64,
    /// `true` when the rates were filled in from the generic fallback
    /// rather than a real registry hit. The footer formatter uses this
    /// flag to mark the dollar figure as approximate.
    pub is_estimated: bool,
}

/// Anthropic's published ephemeral-cache multipliers.
const ANTHROPIC_CACHE_WRITE: f64 = 1.25;
const ANTHROPIC_CACHE_READ: f64 = 0.10;

/// OpenAI's automatic prefix-cache discount. Cache writes are free
/// (0.0); reads bill at half the input rate.
const OPENAI_CACHE_WRITE: f64 = 0.0;
const OPENAI_CACHE_READ: f64 = 0.50;

/// DeepSeek's published context-cache discount. Cache writes are
/// effectively free (DeepSeek charges miss tokens at the regular
/// input rate, so we don't double-count). Cache *hits* bill at
/// $0.07/M which is 0.07/0.27 ≈ 0.2593 of the input rate.
/// Source: api-docs.deepseek.com — context caching pricing.
/// The OpenAI 0.50 multiplier was over-estimating DeepSeek runs by
/// nearly 2x, which is why session totals looked alarmingly high.
const DEEPSEEK_CACHE_WRITE: f64 = 0.0;
const DEEPSEEK_CACHE_READ: f64 = 0.2593;

/// Fallback rates returned by [`ModelPricing::resolve`] when the model
/// name is unknown. Picked to sit roughly in the middle of the field —
/// scary enough that an unknown model isn't accidentally treated as
/// free, cheap enough that it doesn't panic users running DeepSeek.
const FALLBACK_INPUT_PER_MILLION: f64 = 3.0;
const FALLBACK_OUTPUT_PER_MILLION: f64 = 15.0;

/// Static registry of known model rates. Order matters: more specific
/// patterns must come before more general ones (e.g. `deepseek-reasoner`
/// before `deepseek`) because the lookup walks the table in declared
/// order and returns the first substring match. Each entry carries the
/// input rate, output rate, and cache multipliers (write, read) —
/// Anthropic models pay 1.25x/0.10x, OpenAI models pay 0.0/0.5, and
/// everything else inherits the OpenAI-style shape as a safe default.
const KNOWN_RATES: &[(&str, f64, f64, f64, f64)] = &[
    // ── DeepSeek ─────────────────────────────────────────────
    // Context caching with its own published multipliers.
    (
        "deepseek-v4-pro",
        1.74,
        3.48,
        DEEPSEEK_CACHE_WRITE,
        DEEPSEEK_CACHE_READ,
    ),
    (
        "deepseek-v4-flash",
        0.14,
        0.28,
        DEEPSEEK_CACHE_WRITE,
        DEEPSEEK_CACHE_READ,
    ),
    (
        "deepseek-reasoner",
        0.55,
        2.19,
        DEEPSEEK_CACHE_WRITE,
        DEEPSEEK_CACHE_READ,
    ),
    (
        "deepseek-chat",
        0.27,
        1.10,
        DEEPSEEK_CACHE_WRITE,
        DEEPSEEK_CACHE_READ,
    ),
    (
        "deepseek",
        0.27,
        1.10,
        DEEPSEEK_CACHE_WRITE,
        DEEPSEEK_CACHE_READ,
    ),
    // ── Anthropic Claude ─────────────────────────────────────
    (
        "opus",
        15.0,
        75.0,
        ANTHROPIC_CACHE_WRITE,
        ANTHROPIC_CACHE_READ,
    ),
    (
        "sonnet",
        3.0,
        15.0,
        ANTHROPIC_CACHE_WRITE,
        ANTHROPIC_CACHE_READ,
    ),
    (
        "haiku",
        1.0,
        5.0,
        ANTHROPIC_CACHE_WRITE,
        ANTHROPIC_CACHE_READ,
    ),
    // ── OpenAI ───────────────────────────────────────────────
    ("o3", 10.0, 40.0, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    ("o4-mini", 1.10, 4.40, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    (
        "gpt-4.1-nano",
        0.10,
        0.40,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    (
        "gpt-4.1-mini",
        0.40,
        1.60,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    ("gpt-4.1", 2.00, 8.00, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    (
        "gpt-4o-mini",
        0.15,
        0.60,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    ("gpt-4o", 2.50, 10.00, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    // ── xAI Grok ─────────────────────────────────────────────
    ("grok-3", 3.0, 15.0, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    ("grok-2", 2.0, 10.0, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    // ── Google Gemini ────────────────────────────────────────
    (
        "gemini-2.5-pro",
        1.25,
        10.0,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    (
        "gemini-2.5-flash",
        0.15,
        0.60,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    ("gemini", 0.15, 0.60, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    // ── ZhipuAI GLM ─────────────────────────────────────────
    // Prices converted to USD from CNY sources; approximate.
    ("glm-5.1", 1.40, 4.40, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    (
        "glm-5-turbo",
        0.50,
        2.00,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    ("glm-5", 1.00, 3.20, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    ("glm-4", 0.14, 0.14, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    ("glm", 0.50, 2.00, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
    // ── MiniMax ──────────────────────────────────────────────
    (
        "minimax-m2.7",
        0.30,
        1.20,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    (
        "minimax-m2.5",
        0.30,
        1.20,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    (
        "minimax-m2.1",
        0.30,
        1.20,
        OPENAI_CACHE_WRITE,
        OPENAI_CACHE_READ,
    ),
    ("minimax", 0.30, 1.20, OPENAI_CACHE_WRITE, OPENAI_CACHE_READ),
];

impl ModelPricing {
    /// Returns the pricing for `model_name`, falling back to a generic
    /// mid-tier estimate if no entry matches. The returned struct is
    /// always usable; check `is_estimated` to know whether the dollar
    /// figure should be shown as approximate.
    pub fn resolve(model_name: &str) -> Self {
        let lowered = model_name.to_ascii_lowercase();
        for (pattern, input, output, cw, cr) in KNOWN_RATES {
            if lowered.contains(pattern) {
                return Self {
                    input_per_million: *input,
                    output_per_million: *output,
                    cache_write_multiplier: *cw,
                    cache_read_multiplier: *cr,
                    is_estimated: false,
                };
            }
        }
        Self {
            input_per_million: FALLBACK_INPUT_PER_MILLION,
            output_per_million: FALLBACK_OUTPUT_PER_MILLION,
            // Generic fallback mirrors the OpenAI-style prefix cache
            // since that's the dominant shape among new providers.
            cache_write_multiplier: OPENAI_CACHE_WRITE,
            cache_read_multiplier: OPENAI_CACHE_READ,
            is_estimated: true,
        }
    }

    /// Computes a [`TokenCost`] for the given snapshot. Cache reads
    /// and writes bill at the base input rate scaled by their
    /// respective multipliers.
    pub fn estimate(&self, usage: &UsageSnapshot) -> TokenCost {
        TokenCost {
            input_usd: cost_for(usage.input_tokens, self.input_per_million),
            output_usd: cost_for(usage.output_tokens, self.output_per_million),
            cache_write_usd: cost_for(
                usage.cache_write_tokens,
                self.input_per_million * self.cache_write_multiplier,
            ),
            cache_read_usd: cost_for(
                usage.cache_read_tokens,
                self.input_per_million * self.cache_read_multiplier,
            ),
        }
    }
}

fn cost_for(tokens: u32, per_million: f64) -> f64 {
    (f64::from(tokens) / 1_000_000.0) * per_million
}

/// A point-in-time view of token counts for one prompt or one session.
/// `input_tokens` is the non-cached fresh prompt count; cache hits and
/// cache writes are tracked separately so they can be billed at their
/// own rates (see [`ModelPricing`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl UsageSnapshot {
    /// Total tokens billed for this snapshot: fresh input, completion,
    /// cache reads, and cache writes combined.
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

/// Computed dollar costs for a single [`UsageSnapshot`]. Cache reads
/// and writes are tracked as separate fields so callers can surface
/// the prompt-cache discount explicitly instead of folding it into the
/// headline input cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenCost {
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_write_usd: f64,
    pub cache_read_usd: f64,
}

impl TokenCost {
    pub fn total_usd(&self) -> f64 {
        self.input_usd + self.output_usd + self.cache_write_usd + self.cache_read_usd
    }
}

/// Trait re-export marker — kept as a documentation anchor for now so
/// users grepping for "Pricing" find this module.
pub trait Pricing {}
impl Pricing for ModelPricing {}

/// Builds the one-line stderr footer printed at the end of each run.
///
/// Baseline shape (no cache activity):
/// ```text
/// [aegis] in=1234 out=567 total=1801 · ~$0.0012 · model=deepseek-chat
/// ```
///
/// When cache reads or cache writes are non-zero, a `cache: …` segment
/// is inserted so users can see the prompt-cache discount at a glance:
/// ```text
/// [aegis] in=120 out=80 · cache: read=4000 write=0 · total=4200 · ~$0.0018 · model=claude-haiku-4-5
/// ```
///
/// When the resolved pricing is a generic fallback the dollar figure
/// gets a trailing `≈` marker so users know not to trust it to four
/// decimal places.
pub fn format_cost_footer(usage: &UsageSnapshot, model: &str) -> String {
    let pricing = ModelPricing::resolve(model);
    let cost = pricing.estimate(usage);
    let approx_marker = if pricing.is_estimated { "≈" } else { "" };
    // Explicitly label the cost as session-scoped so users don't
    // misread it as an account-wide total. The `total` snapshot
    // passed in here is the in-memory accumulator that resets on
    // `/clear`, provider switch, or REPL restart.
    format!("[aegis] session ~${:.4}{}", cost.total_usd(), approx_marker)
}

/// Builds a multi-line cost breakdown for the `/cost` slash command.
/// Shows: token counts by type, cache hit ratio, savings vs no-cache,
/// and total cost. Uses ANSI colors when `colored=true`.
///
/// Example output:
/// ```text
///   model        deepseek-chat
///   input        12,400 tokens   $0.0033
///   output        2,100 tokens   $0.0023
///   cache read    8,000 tokens   $0.0006  (saved $0.0016)
///   cache write   4,000 tokens   $0.0014
///   ─────────────────────────────────────
///   total                        $0.0076
///   cache hit rate  64%
/// ```
pub fn format_cost_breakdown(
    usage: &UsageSnapshot,
    model: &str,
    turns: usize,
    colored: bool,
) -> String {
    let pricing = ModelPricing::resolve(model);
    let cost = pricing.estimate(usage);
    let approx = if pricing.is_estimated { "≈" } else { "" };

    let (dim, bld, cyn, rst) = if colored {
        ("\x1b[2m", "\x1b[1m", "\x1b[36m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    let fmt_tok = |n: u32| {
        let s = n.to_string();
        // insert thousands separators
        let mut out = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out.chars().rev().collect::<String>()
    };

    // Cache hit ratio: cache_read / (input + cache_read)
    let total_input = usage.input_tokens as u64 + usage.cache_read_tokens as u64;
    let cache_hit_pct = if total_input > 0 {
        (usage.cache_read_tokens as f64 / total_input as f64 * 100.0) as u32
    } else {
        0
    };

    // Savings: what the cache_read tokens would have cost at full input rate
    let saved_usd =
        cost_for(usage.cache_read_tokens, pricing.input_per_million) - cost.cache_read_usd;

    let mut s = String::new();
    s.push('\n');
    s.push_str(&format!(
        "  {dim}model       {rst} {cyn}{bld}{model}{rst}\n",
    ));
    s.push_str(&format!("  {dim}turns       {rst} {}\n", turns));
    s.push_str(&format!(
        "  {dim}input       {rst} {:>10} tokens   {bld}${:.4}{rst}\n",
        fmt_tok(usage.input_tokens),
        cost.input_usd
    ));
    s.push_str(&format!(
        "  {dim}output      {rst} {:>10} tokens   {bld}${:.4}{rst}\n",
        fmt_tok(usage.output_tokens),
        cost.output_usd
    ));
    if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
        s.push_str(&format!(
            "  {dim}cache read  {rst} {:>10} tokens   {bld}${:.4}{rst}  {dim}(saved ${:.4}){rst}\n",
            fmt_tok(usage.cache_read_tokens),
            cost.cache_read_usd,
            saved_usd
        ));
        s.push_str(&format!(
            "  {dim}cache write {rst} {:>10} tokens   {bld}${:.4}{rst}\n",
            fmt_tok(usage.cache_write_tokens),
            cost.cache_write_usd
        ));
    }
    s.push_str(&format!(
        "  {dim}─────────────────────────────────────{rst}\n"
    ));
    s.push_str(&format!(
        "  {dim}total       {rst}                    {bld}${:.4}{approx}{rst}\n",
        cost.total_usd()
    ));
    if usage.cache_read_tokens > 0 {
        s.push_str(&format!(
            "  {dim}cache hit   {rst} {cyn}{cache_hit_pct}%{rst}\n"
        ));
    }
    s
}

/// Builds the compact one-line "live" footer printed after every REPL
/// turn. Unlike [`format_cost_footer`], which is intended as a final
/// summary, this variant fits the per-turn delta and the running
/// session total on a single line so the REPL stays quiet between
/// turns.
///
/// Shape:
/// ```text
/// [aegis] turn: in=120 out=80 · $0.0003 · session: in=480 out=320 · $0.0012 · 4 turns
/// ```
///
/// Cache counters are folded into the headline `in=` figure (cache
/// reads + writes count as input tokens for the purposes of the live
/// view; the dedicated cache breakdown still shows up in `/cost` and
/// the exit footer). The dollar columns get a trailing `≈` when the
/// pricing was a generic fallback, matching `format_cost_footer`.
pub fn format_cost_delta(
    turn: &UsageSnapshot,
    session: &UsageSnapshot,
    turns: usize,
    model: &str,
) -> String {
    let pricing = ModelPricing::resolve(model);
    let turn_cost = pricing.estimate(turn).total_usd();
    let session_cost = pricing.estimate(session).total_usd();
    let approx = if pricing.is_estimated { "≈" } else { "" };
    // Compact status line: model · turn cost · session total · turn count
    let short_model = model
        .strip_prefix("deepseek-")
        .or_else(|| model.strip_prefix("gpt-"))
        .or_else(|| model.strip_prefix("claude-"))
        .unwrap_or(model);
    format!(
        "\x1b[2m{short_model} · turn ${:.4}{approx} · session ${:.4}{approx} · {turns} turn{}\x1b[0m",
        turn_cost,
        session_cost,
        if turns == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_chat_resolves_to_real_rates() {
        let pricing = ModelPricing::resolve("deepseek-chat");
        assert!(!pricing.is_estimated);
        assert_eq!(pricing.input_per_million, 0.27);
        assert_eq!(pricing.output_per_million, 1.10);
    }

    #[test]
    fn deepseek_reasoner_takes_precedence_over_generic_deepseek() {
        let pricing = ModelPricing::resolve("deepseek-reasoner");
        assert!(!pricing.is_estimated);
        assert_eq!(pricing.input_per_million, 0.55);
        assert_eq!(pricing.output_per_million, 2.19);
    }

    #[test]
    fn vendor_prefixed_model_names_still_resolve() {
        // OpenRouter / Fireworks style fully-qualified names must still
        // hit the substring lookup.
        let pricing = ModelPricing::resolve("accounts/fireworks/models/deepseek-chat");
        assert!(!pricing.is_estimated);
        assert_eq!(pricing.input_per_million, 0.27);
    }

    #[test]
    fn unknown_model_falls_back_with_estimated_flag() {
        let pricing = ModelPricing::resolve("totally-new-provider-x");
        assert!(pricing.is_estimated);
        assert_eq!(pricing.input_per_million, FALLBACK_INPUT_PER_MILLION);
    }

    #[test]
    fn cost_math_for_one_million_each_way() {
        let usage = UsageSnapshot {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            ..Default::default()
        };
        let pricing = ModelPricing::resolve("deepseek-chat");
        let cost = pricing.estimate(&usage);
        // 1M @ $0.27 + 500k @ $1.10 = $0.27 + $0.55 = $0.82
        assert!((cost.total_usd() - 0.82).abs() < 1e-9);
    }

    #[test]
    fn footer_for_known_model_has_no_approx_marker() {
        let usage = UsageSnapshot {
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
        };
        let line = format_cost_footer(&usage, "deepseek-chat");
        assert!(line.starts_with("[aegis] session ~$"), "footer: {line}");
        assert!(!line.contains("≈"));
    }

    #[test]
    fn footer_for_unknown_model_has_approx_marker() {
        let usage = UsageSnapshot {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let line = format_cost_footer(&usage, "ghost-model-9000");
        assert!(line.contains("≈"), "expected approx marker, got `{line}`");
    }

    #[test]
    fn opus_resolves_to_anthropic_top_tier() {
        let pricing = ModelPricing::resolve("claude-opus-4-6");
        assert!(!pricing.is_estimated);
        assert_eq!(pricing.input_per_million, 15.0);
        assert_eq!(pricing.output_per_million, 75.0);
        // Claude families inherit Anthropic's ephemeral cache multipliers.
        assert_eq!(pricing.cache_write_multiplier, ANTHROPIC_CACHE_WRITE);
        assert_eq!(pricing.cache_read_multiplier, ANTHROPIC_CACHE_READ);
    }

    #[test]
    fn anthropic_cache_math_hits_expected_fractions_of_input_rate() {
        // Sonnet: input $3/M, cache write 1.25x = $3.75/M,
        // cache read 0.10x = $0.30/M. 1M of each makes the arithmetic
        // trivial to verify by hand.
        let usage = UsageSnapshot {
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
        };
        let pricing = ModelPricing::resolve("claude-sonnet-4-5");
        let cost = pricing.estimate(&usage);
        assert!((cost.cache_write_usd - 3.75).abs() < 1e-9);
        assert!((cost.cache_read_usd - 0.30).abs() < 1e-9);
        assert!((cost.input_usd - 0.0).abs() < 1e-9);
        assert!((cost.output_usd - 0.0).abs() < 1e-9);
        assert!((cost.total_usd() - 4.05).abs() < 1e-9);
    }

    #[test]
    fn footer_surfaces_cache_segment_when_cache_counters_are_non_zero() {
        // A realistic second-turn claude run: 80 fresh prompt tokens,
        // 40 output tokens, 4000 tokens read from the cache, no new
        // writes. The footer must expose both cache counters so users
        // can see the prompt-cache discount at a glance.
        let usage = UsageSnapshot {
            input_tokens: 80,
            output_tokens: 40,
            cache_read_tokens: 4000,
            cache_write_tokens: 0,
        };
        let line = format_cost_footer(&usage, "claude-haiku-4-5");
        assert!(line.starts_with("[aegis] session ~$"), "footer: {line}");
    }

    #[test]
    fn cost_delta_renders_turn_and_session_segments() {
        let turn = UsageSnapshot {
            input_tokens: 120,
            output_tokens: 80,
            ..Default::default()
        };
        let session = UsageSnapshot {
            input_tokens: 480,
            output_tokens: 320,
            ..Default::default()
        };
        let line = format_cost_delta(&turn, &session, 4, "deepseek-chat");
        assert!(line.contains("chat"));
        assert!(line.contains("turn $"), "delta: {line}");
        assert!(line.contains("session $"), "delta: {line}");
        assert!(line.contains("4 turns"));
        assert!(!line.contains('≈'));
    }

    #[test]
    fn cost_delta_marks_unknown_model_as_approx() {
        let turn = UsageSnapshot {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        let line = format_cost_delta(&turn, &turn, 1, "ghost-model-9000");
        assert!(line.contains('≈'), "expected approx marker, got {line}");
    }

    #[test]
    fn usage_snapshot_total_includes_cache_counters() {
        let usage = UsageSnapshot {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 100,
            cache_write_tokens: 50,
        };
        assert_eq!(usage.total_tokens(), 165);
    }

    // ========================================================================
    // format_cost_breakdown — failure-driven tests
    // ========================================================================

    /// Basic breakdown must contain model name, input/output lines, and total.
    #[test]
    fn breakdown_contains_required_sections() {
        let usage = UsageSnapshot {
            input_tokens: 12_400,
            output_tokens: 2_100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let out = format_cost_breakdown(&usage, "deepseek-chat", 3, false);
        assert!(out.contains("deepseek-chat"), "missing model name");
        assert!(out.contains("input"), "missing input line");
        assert!(out.contains("output"), "missing output line");
        assert!(out.contains("total"), "missing total line");
        assert!(out.contains("3"), "missing turn count");
    }

    /// When cache tokens are zero, cache lines must NOT appear.
    #[test]
    fn breakdown_omits_cache_lines_when_no_cache() {
        let usage = UsageSnapshot {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let out = format_cost_breakdown(&usage, "deepseek-chat", 1, false);
        assert!(
            !out.contains("cache read"),
            "cache read should be absent: {out}"
        );
        assert!(
            !out.contains("cache write"),
            "cache write should be absent: {out}"
        );
        assert!(
            !out.contains("cache hit"),
            "hit rate should be absent: {out}"
        );
    }

    /// When cache is active, must show read/write/savings/hit-rate.
    #[test]
    fn breakdown_shows_cache_section_when_cache_active() {
        let usage = UsageSnapshot {
            input_tokens: 4_000,
            output_tokens: 500,
            cache_read_tokens: 8_000,
            cache_write_tokens: 4_000,
        };
        let out = format_cost_breakdown(&usage, "deepseek-chat", 2, false);
        assert!(out.contains("cache read"), "cache read missing: {out}");
        assert!(out.contains("cache write"), "cache write missing: {out}");
        assert!(out.contains("saved $"), "savings missing: {out}");
        assert!(out.contains("cache hit"), "hit rate missing: {out}");
        // Hit rate: 8000 / (4000 + 8000) = 66%
        assert!(out.contains("66%"), "wrong hit rate in: {out}");
    }

    /// Total in breakdown must equal sum of component costs.
    #[test]
    fn breakdown_total_matches_component_sum() {
        let usage = UsageSnapshot {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let pricing = ModelPricing::resolve("deepseek-chat");
        let cost = pricing.estimate(&usage);
        let out = format_cost_breakdown(&usage, "deepseek-chat", 1, false);
        // Total should be $0.82
        let expected = format!("${:.4}", cost.total_usd());
        assert!(out.contains(&expected), "total {expected} not in: {out}");
    }

    /// Thousands separator appears in large token counts.
    #[test]
    fn breakdown_formats_thousands_with_commas() {
        let usage = UsageSnapshot {
            input_tokens: 12_400,
            output_tokens: 2_100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let out = format_cost_breakdown(&usage, "deepseek-chat", 1, false);
        assert!(
            out.contains("12,400"),
            "thousands separator missing for input: {out}"
        );
        assert!(
            out.contains("2,100"),
            "thousands separator missing for output: {out}"
        );
    }

    /// Approximate marker appears for unknown models.
    #[test]
    fn breakdown_shows_approx_for_unknown_model() {
        let usage = UsageSnapshot {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let out = format_cost_breakdown(&usage, "totally-unknown-model-xyz", 1, false);
        assert!(
            out.contains('≈'),
            "approx marker missing for unknown model: {out}"
        );
    }

    /// Zero usage produces a valid breakdown without panicking.
    #[test]
    fn breakdown_zero_usage_does_not_panic() {
        let usage = UsageSnapshot::default();
        let out = format_cost_breakdown(&usage, "deepseek-chat", 0, false);
        assert!(out.contains("$0.0000"), "zero cost not shown: {out}");
    }
}
