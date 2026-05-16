//! Session 17 — failure-driven test bench for the agent loop and
//! compactor.
//!
//! This file is the starting point for Session 17's fix work. It pins
//! the two latent bugs surfaced in Session 16 (`docs/sessions/session-16.md`)
//! together with the four invariants that must continue to hold while
//! those bugs are fixed.
//!
//! Discipline:
//!
//! * Every test uses a deterministic [`ScriptedProvider`] — no network,
//!   no real provider, no randomness in usage numbers.
//! * Every test runs in a fresh [`tempdir`] so on-disk state is fully
//!   isolated between cases and after the test process exits.
//! * Every test sets a small [`AgentConfig::max_turns`] cap so a buggy
//!   loop cannot run away with the test runner.
//! * Each test forces ≥3 state transitions and asserts intermediate
//!   state at every loop iteration where one is observable.
//!
//! Layout:
//!
//! 1. `bug_resume_skips_compaction_on_first_turn` — pins BUG #1
//! 2. `bug_max_turns_zero_dangling_user_message` — pins BUG #2
//! 3. `invariant_cumulative_usage_cache_path_equals_fresh_path`
//! 4. `invariant_streaming_usage_events_sum_equals_final_cumulative`
//! 5. `invariant_cache_aware_compaction_trigger_uses_summed_tokens`
//! 6. `invariant_tool_failure_recovery_three_turn_state_machine`
//!
//! After fixing each bug in Session 17 the corresponding `bug_*` test's
//! assertions must be flipped (or the test rewritten as a guard). The
//! four `invariant_*` tests must continue to pass unchanged — they are
//! the regression net.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use aegis_api::{
    ApiResult, ChatChoice, ChatMessage, ChatProvider, ChatRequest, ChatResponse, Role, StreamEvent,
    ToolCall, ToolCallFunction, Usage,
};
use aegis_core::{
    format_briefing, Agent, AgentConfig, AgentError, AuditingPermission, CompactionConfig, DenyAll,
    SessionStore, Subagent, SubagentBrief, SubagentType, ToolContext, ToolRegistry, UsageSnapshot,
};
use std::io::Write as _;

// ============================================================================
// Test fixtures — deterministic provider + isolated workspace + builders.
// ============================================================================

/// Provider whose responses are fully pre-scripted. Each `chat` call
/// pops the next response from the queue and records the request that
/// produced it. If the queue runs dry the test fails loudly with a
/// panic — the agent loop should never call the provider more times
/// than the script anticipates.
struct ScriptedProvider {
    queue: Mutex<Vec<ChatResponse>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            queue: Mutex::new(responses.into_iter().rev().collect()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for ScriptedProvider {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        self.seen.lock().unwrap().push(request.clone());
        let mut q = self.queue.lock().unwrap();
        Ok(q.pop().expect("ScriptedProvider: queue exhausted"))
    }
}

/// Content-addressed provider used by the Session 32 parallel-spawn
/// tests. Unlike [`ScriptedProvider`], responses are keyed by a
/// substring that must appear in the last user message of the incoming
/// request — so when many threads hit the same provider concurrently,
/// each thread still gets the response intended for *its* brief
/// regardless of interleaving. A request that matches no key is a hard
/// test failure, not a silent mismatch.
struct KeyedProvider {
    responses: Mutex<HashMap<String, ChatResponse>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl KeyedProvider {
    fn new(pairs: Vec<(&str, ChatResponse)>) -> Self {
        Self {
            responses: Mutex::new(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for KeyedProvider {
    async fn chat(&self, request: &ChatRequest) -> ApiResult<ChatResponse> {
        self.seen.lock().unwrap().push(request.clone());
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let map = self.responses.lock().unwrap();
        for (key, resp) in map.iter() {
            if last_user.contains(key) {
                return Ok(resp.clone());
            }
        }
        panic!(
            "KeyedProvider: no response matched last user message {last_user:?} \
             (known keys: {:?})",
            map.keys().collect::<Vec<_>>()
        );
    }
}

/// Per-test temporary workspace. Lives under the OS temp dir, keyed by
/// process id and a monotonic counter so parallel test execution does
/// not collide. Caller does not have to clean up — tests are expected
/// to be ephemeral and the OS reclaims the space.
fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("metis-fd-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

/// Builds a `ChatResponse` whose assistant message is a tool_call with
/// the given name and arguments, carrying the supplied `Usage` so the
/// caller can probe cumulative aggregation precisely.
fn assistant_calling_with_usage(
    tool: &str,
    args_json: &str,
    call_id: &str,
    usage: Usage,
) -> ChatResponse {
    ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: Role::Assistant,
                content: None,
                content_blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: call_id.to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: tool.to_string(),
                        arguments: args_json.to_string(),
                    },
                }],
                tool_call_id: None,
                name: None,
                protected: false,
                reasoning_content: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: Some(usage),
    }
}

/// Builds a `ChatResponse` whose assistant message is plain text — the
/// terminating shape that ends the agent loop with an `AgentOutput`.
fn assistant_final_with_usage(text: &str, usage: Usage) -> ChatResponse {
    ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage::assistant_text(text),
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(usage),
    }
}

/// Convenience constructor for a fresh `Usage` snapshot with explicit
/// fields. Keeps test bodies free of repetitive struct literals.
fn usage(prompt: u32, completion: u32, cache_read: u32, cache_write: u32) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion + cache_read + cache_write,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

// ============================================================================
// BUG #1 — resume skips compaction on the first turn
// ============================================================================

/// **REGRESSION GUARD — fixed in Session 18.**
///
/// **Original bug**: when a session was resumed with a transcript that
/// already exceeded the compaction trigger, the FIRST turn of the new
/// run did NOT compact. `Agent::run` initialised its local
/// `last_prompt_tokens` to `0`, so the trigger check at the top of the
/// loop iteration failed on entry. The full uncompacted transcript was
/// shipped to the provider — exactly the situation compaction was
/// supposed to prevent. Only manifested on the *first* turn after a
/// resume; from turn 2 onwards the previous response's usage updated
/// `last_prompt_tokens` and compaction kicked in normally.
///
/// **Fix**: Session 18 seeds `last_prompt_tokens` from the preloaded
/// transcript so the trigger check fires on entry.
///
/// **Multi-step path**: preload session → attach to agent → run a
/// single user prompt → inspect the request the provider received.
///
/// **Assertions** (pinning the FIXED state):
///
/// 1. The provider receives exactly one request.
/// 2. That request contains **fewer** than `preloaded_count + 1`
///    messages (compaction triggered on first turn).
/// 3. A synthetic "compacted" system message is present.
#[tokio::test]
async fn bug_resume_skips_compaction_on_first_turn() {
    let dir = tempdir();
    let mut session = SessionStore::open(&dir, "huge").unwrap();

    // Preload 1 system + 30 user/assistant pairs = 61 messages. This
    // is well over any reasonable `keep_tail`, so a fixed loop would
    // unambiguously compact on entry.
    session.append(&ChatMessage::system("sys")).unwrap();
    for i in 0..30 {
        session.append(&ChatMessage::user(format!("u{i}"))).unwrap();
        session
            .append(&ChatMessage::assistant_text(format!("a{i}")))
            .unwrap();
    }
    let preloaded_count = session.messages().len();
    assert_eq!(preloaded_count, 61);

    // Provider returns one final reply. The interesting state is in
    // the request the provider received, not the response.
    let provider =
        ScriptedProvider::new(vec![assistant_final_with_usage("ok", usage(10, 4, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        // Tight compaction so a fixed loop would unambiguously trigger.
        compaction: CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        },
        // Hard upper bound on iterations so a buggy loop cannot run
        // away with the test runner.
        max_turns: 4,
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    agent
        .run("next question")
        .await
        .expect("run should succeed");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1, "expected exactly one provider call");

    // FIXED (Session 18): the first turn after resume now compacts.
    // The request should be strictly shorter than the full preloaded
    // transcript and must contain the synthetic compaction marker.
    assert!(
        reqs[0].messages.len() < preloaded_count + 1,
        "FIX REGRESSED: first turn after resume still sends full transcript (len={}, preloaded={})",
        reqs[0].messages.len(),
        preloaded_count
    );

    let has_synthetic = reqs[0].messages.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        has_synthetic,
        "FIX REGRESSED: synthetic compaction marker missing on first turn after resume: {:#?}",
        reqs[0].messages
    );
}

// ============================================================================
// BUG #2 — max_turns = 0 persists a dangling user message
// ============================================================================

/// **REGRESSION GUARD — fixed in Session 18, variant re-routed in S22.**
///
/// **Original bug**: `AgentConfig::max_turns = 0` was silently accepted.
/// The for-loop body never executed, so no provider call happened — but
/// the user message was persisted to the session JSONL **before** the
/// loop, leaving a session whose last entry was a user prompt that no
/// assistant ever answered. A subsequent `--resume` would replay a
/// dangling prompt.
///
/// **Fix**: validation now rejects `max_turns = 0` before any persist
/// (Session 18), routed through `AgentError::Config` (Session 22).
///
/// **Multi-step path**: open session → construct agent with
/// `max_turns = 0` → call `run("hello")` → expect `AgentError::Config`
/// → reopen session from disk → inspect persisted messages.
///
/// **Assertions** (pinning the FIXED state):
///
/// 1. `agent.run` returns `AgentError::Config(_)` mentioning `max_turns`.
/// 2. Provider was never called.
/// 3. Reopened session is empty — no dangling prompt on disk.
#[tokio::test]
async fn bug_max_turns_zero_dangling_user_message() {
    let dir = tempdir();
    let session = SessionStore::open(&dir, "zero").unwrap();

    // Empty queue: if the loop ever calls the provider the test will
    // panic with "queue exhausted", which is what we want.
    let provider = ScriptedProvider::new(vec![]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        system_prompt: Some("sys".to_string()),
        max_turns: 0,
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    let err = agent
        .run("hello")
        .await
        .expect_err("max_turns=0 must error");
    // Session 22: rerouted through the dedicated Config variant.
    assert!(
        matches!(&err, AgentError::Config(msg) if msg.contains("max_turns")),
        "wrong error variant: {err:?}"
    );

    // Provider was never called.
    assert!(provider.requests().is_empty());

    // FIXED (Session 18): max_turns=0 is rejected BEFORE any persist.
    // The reopened session must be empty — no system, no user, no
    // dangling prompt on disk.
    let reopened = SessionStore::open(&dir, "zero").unwrap();
    let msgs = reopened.messages();
    assert!(
        msgs.is_empty(),
        "FIX REGRESSED: session not empty after max_turns=0 reject: {msgs:#?}"
    );
}

// ============================================================================
// INVARIANT — cumulative usage is path-independent
// ============================================================================

/// **Regression invariant**: two runs that process the same logical
/// work — one with all-fresh prompt tokens, one with the same total
/// served as a mix of fresh + cache_read + cache_write — must
/// accumulate identical totals across the input-side categories.
///
/// **Risk this catches**: a refactor of `Agent::run`'s usage
/// aggregation block (`agent.rs:243-260`) could silently start
/// double-counting cache reads (e.g. by trusting `prompt_tokens` as
/// the full input on a provider where it has been peeled to fresh-only)
/// or by skipping cache fields. Either failure mode would cause the
/// fresh path and the cache path to disagree on the same logical work.
///
/// **Multi-step path**: each path runs a 3-turn script — `tool call →
/// tool result → final text`. The aggregator is exercised across turns,
/// not just one response, so a per-turn off-by-one would compound.
///
/// **Assertions**:
///
/// 1. `fresh_input_sum == cached_input_sum` (the headline invariant).
/// 2. Both equal `3000` — the deliberate per-turn × turn-count product.
/// 3. Output token totals match exactly between paths.
#[tokio::test]
async fn invariant_cumulative_usage_cache_path_equals_fresh_path() {
    async fn run_path(usages: [Usage; 3]) -> UsageSnapshot {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "x\n").unwrap();
        let script = vec![
            assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", usages[0]),
            assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c2", usages[1]),
            assistant_final_with_usage("done", usages[2]),
        ];
        let provider = ScriptedProvider::new(script);
        let registry = ToolRegistry::with_builtins();
        let ctx = ToolContext::new(&dir);
        let mut agent = Agent::new(
            &provider as &dyn ChatProvider,
            &registry,
            ctx,
            AgentConfig {
                max_turns: 4,
                ..AgentConfig::default()
            },
        );
        agent.run("go").await.unwrap().usage
    }

    // Fresh path: 1000 fresh prompt tokens / turn, 100 completion / turn.
    let fresh = [usage(1000, 100, 0, 0); 3];
    // Cache path: 200 fresh + 600 cache_read + 200 cache_write per
    // turn, same 100 completion. Sum across input categories per
    // turn = 1000, identical to the fresh path's load.
    let cached = [usage(200, 100, 600, 200); 3];

    let fresh_total = run_path(fresh).await;
    let cached_total = run_path(cached).await;

    let fresh_input_sum =
        fresh_total.input_tokens + fresh_total.cache_read_tokens + fresh_total.cache_write_tokens;
    let cached_input_sum = cached_total.input_tokens
        + cached_total.cache_read_tokens
        + cached_total.cache_write_tokens;

    assert_eq!(
        fresh_input_sum, cached_input_sum,
        "INVARIANT VIOLATED: same logical work, different totals.\n  fresh:  {fresh_total:?}\n  cached: {cached_total:?}"
    );
    assert_eq!(fresh_input_sum, 3000);
    assert_eq!(cached_input_sum, 3000);
    assert_eq!(fresh_total.output_tokens, cached_total.output_tokens);
    assert_eq!(fresh_total.output_tokens, 300);
}

// ============================================================================
// INVARIANT — streamed Usage events sum to the final aggregate
// ============================================================================

/// **Regression invariant**: every `Usage` event observed by the
/// stream callback must sum to the cumulative `AgentOutput.usage`.
/// The default `chat_stream` impl synthesises one `TextDelta + Usage`
/// pair per turn, so a 3-turn run must emit exactly 3 `Usage` events
/// whose `prompt_tokens` and `completion_tokens` sums equal the final
/// aggregator state.
///
/// **Risk this catches**: a refactor that splits the streaming pipe
/// from the aggregation pipe (e.g. one fed by SSE chunks, the other
/// fed by the response object) could allow them to disagree silently.
/// Users who watch live cost on stream callbacks would see different
/// numbers than the final cost footer.
///
/// **Multi-step path**: 3 scripted turns with distinct usage values
/// (11/3, 13/5, 17/7) so a missed or duplicated event is immediately
/// visible in the sum.
///
/// **Assertions**:
///
/// 1. Exactly 3 `Usage` events were emitted (not 2, not 4).
/// 2. Sum of `prompt_tokens` across events == `output.usage.input_tokens`.
/// 3. Sum of `completion_tokens` across events == `output.usage.output_tokens`.
#[tokio::test]
async fn invariant_streaming_usage_events_sum_equals_final_cumulative() {
    use std::sync::{Arc, Mutex};

    let dir = tempdir();
    fs::write(dir.join("a.txt"), "x\n").unwrap();

    let u1 = usage(11, 3, 0, 0);
    let u2 = usage(13, 5, 0, 0);
    let u3 = usage(17, 7, 0, 0);
    let script = vec![
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", u1),
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c2", u2),
        assistant_final_with_usage("done", u3),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let sink: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_cb = Arc::clone(&sink);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_stream_callback(move |event| {
        sink_cb.lock().unwrap().push(event);
    });

    let output = agent.run("go").await.expect("run");

    let events = sink.lock().unwrap();
    let usage_events: Vec<&Usage> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage(u) => Some(u),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage_events.len(),
        3,
        "expected one Usage event per turn, got: {events:#?}"
    );

    let stream_prompt_sum: u32 = usage_events.iter().map(|u| u.prompt_tokens).sum();
    let stream_completion_sum: u32 = usage_events.iter().map(|u| u.completion_tokens).sum();
    assert_eq!(
        stream_prompt_sum, output.usage.input_tokens,
        "INVARIANT VIOLATED: streamed Usage prompt sum ≠ aggregated input_tokens"
    );
    assert_eq!(
        stream_completion_sum, output.usage.output_tokens,
        "INVARIANT VIOLATED: streamed Usage completion sum ≠ aggregated output_tokens"
    );
}

// ============================================================================
// INVARIANT — compaction trigger uses the FULL prompt depth
// ============================================================================

/// **Regression invariant**: `maybe_compact` is called from inside
/// the agent loop with the previous turn's reconstructed prompt depth
/// (`fresh + cache_read + cache_write`), not just `prompt_tokens`. A
/// turn whose fresh tokens alone are below the trigger but whose
/// cache-inclusive total CROSSES the trigger MUST cause compaction
/// on the next turn.
///
/// **Risk this catches**: a refactor of `agent.rs:257-260` (the line
/// that builds `last_prompt_tokens`) that drops the `cache_read` /
/// `cache_write` addends would silently make compaction blind to
/// cached prompts. Long Anthropic conversations using ephemeral
/// caching would never compact.
///
/// **Multi-step path**: turn 1 (no compaction yet, `last_prompt_tokens`
/// starts at 0) → turn 2 (compaction *must* trigger because turn 1's
/// response carried cache tokens that pushed reconstructed depth over
/// the threshold) → turn 3 (final, ends the loop).
///
/// **Numbers**: trigger = `1000 * 0.75 = 750`. Turn 1 usage:
/// `prompt = 300, cache_read = 400, cache_write = 100` →
/// reconstructed depth `= 800 > 750` → must compact.
///
/// **Assertion**: turn 2's request (the second one in `provider.requests()`)
/// must contain a synthetic system message starting with "compacted".
/// Without the cache addends, depth would be 300 < 750 and no
/// synthetic message would appear — the test would fail loudly.
#[tokio::test]
async fn invariant_cache_aware_compaction_trigger_uses_summed_tokens() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "x\n").unwrap();

    let usage_with_cache = usage(300, 4, 400, 100);
    let plain = usage(50, 4, 0, 0);

    let script = vec![
        // Turn 1: tool call. Response carries cache tokens that push
        // the reconstructed depth over the trigger.
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", usage_with_cache),
        // Turn 2: another tool call so we get a third request to
        // inspect after compaction.
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c2", plain),
        // Turn 3: final, ends the loop.
        assistant_final_with_usage("done", plain),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        system_prompt: Some("sys".to_string()),
        compaction: CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.75, // trigger = 750
            keep_tail: 1,
        },
        max_turns: 4,
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    agent.run("go").await.expect("run");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    // Turn 1 request: [system, user] (no compaction, last_prompt_tokens=0).
    assert_eq!(reqs[0].messages.len(), 2);

    // Turn 2 request: compaction MUST have run because reconstructed
    // depth from turn 1's response = 300 + 400 + 100 = 800 > 750.
    let turn2_has_synthetic = reqs[1].messages.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        turn2_has_synthetic,
        "INVARIANT VIOLATED: cache tokens did not contribute to compaction trigger.\n  turn 2 messages: {:#?}",
        reqs[1].messages
    );
}

// ============================================================================
// INVARIANT — tool failure recovery is a clean three-turn state machine
// ============================================================================

/// **Regression invariant**: a tool failure on turn 1 must surface as
/// a `tool` message so the model can self-correct on turn 2 and recover
/// on turn 3. The loop must NOT abort on a tool error, the failed
/// call's tool reply MUST contain the error string, and the recovery
/// turn's tool reply MUST contain the real file body. Cumulative usage
/// must include all three turns exactly once — failed turns are billed,
/// not skipped, not doubled.
///
/// **Risk this catches**: a refactor of `Agent::execute_call` that
/// converted tool errors into `AgentError` instead of conversational
/// messages would silently abort the loop on the first failure. A
/// refactor of the usage aggregator that skipped failed turns would
/// undercharge users; one that doubled them would overcharge.
///
/// **Multi-step path**: turn 1 (read missing.txt → tool error) →
/// turn 2 (model self-corrects, reads real.txt → success) → turn 3
/// (assistant returns plain text → loop terminates). Three full
/// state transitions: error → recovery → terminal.
///
/// **Assertions**:
///
/// 1. Loop did NOT abort — `agent.run` returned `Ok`.
/// 2. `output.turns == 3`.
/// 3. Turn 2's tool reply contains "error" (the failed call).
/// 4. Turn 3's request carries BOTH tool replies; the second one
///    contains the recovered file body.
/// 5. Cumulative usage: `input = 10+20+30 = 60`,
///    `output = 4+4+6 = 14`. Failed turns billed exactly once.
#[tokio::test]
async fn invariant_tool_failure_recovery_three_turn_state_machine() {
    let dir = tempdir();
    fs::write(dir.join("real.txt"), "recovered\n").unwrap();

    let script = vec![
        // Turn 1: read_file on a path that does not exist → tool error.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"missing.txt"}"#,
            "call_1",
            usage(10, 4, 0, 0),
        ),
        // Turn 2: model self-corrects, reads the real file.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"real.txt"}"#,
            "call_2",
            usage(20, 4, 0, 0),
        ),
        // Turn 3: plain text reply, loop terminates cleanly.
        assistant_final_with_usage("done", usage(30, 6, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    );
    let output = agent
        .run("read a file")
        .await
        .expect("loop should not abort on tool error");

    assert_eq!(output.turns, 3);
    assert_eq!(output.final_text, "done");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    // Turn 2's request must carry the failed tool reply with an error.
    let turn2_tool = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must contain a tool reply");
    assert!(
        turn2_tool
            .content
            .as_deref()
            .unwrap_or("")
            .contains("error"),
        "turn 2 tool reply (failed call) was: {:?}",
        turn2_tool.content
    );

    // Turn 3 must carry BOTH tool replies; the second one (recovery)
    // must contain the real file body.
    let turn3_tools: Vec<&ChatMessage> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(
        turn3_tools.len(),
        2,
        "turn 3 should see both tool replies (failed + recovered)"
    );
    assert!(
        turn3_tools[1]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("recovered"),
        "recovery tool reply was: {:?}",
        turn3_tools[1].content
    );

    // Cumulative usage invariant: 10+20+30 prompt, 4+4+6 completion.
    // Failed turns billed exactly once — not skipped, not doubled.
    assert_eq!(
        output.usage.input_tokens, 60,
        "input tokens diverged from expected sum"
    );
    assert_eq!(
        output.usage.output_tokens, 14,
        "output tokens diverged from expected sum"
    );
    assert_eq!(output.usage.cache_read_tokens, 0);
    assert_eq!(output.usage.cache_write_tokens, 0);
}

// ============================================================================
// SESSION 19 — new edge cases
// ============================================================================

/// **Edge case (Session 19)**: `max_turns = 1` with a final-text
/// response is the smallest legal happy path. The loop must run
/// exactly once, return `Ok(AgentOutput { turns: 1, .. })`, persist
/// `[system, user, assistant]`, and never error.
///
/// **Risk**: a refactor that off-by-ones the `1..=max_turns` range
/// (e.g. `1..max_turns`) would silently turn the smallest valid
/// configuration into an immediate `MaxTurns(1)` error.
#[tokio::test]
async fn edge_max_turns_one_with_final_text_succeeds() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("hi", usage(7, 2, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let session = SessionStore::open(&dir, "one").unwrap();
    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            system_prompt: Some("sys".to_string()),
            max_turns: 1,
            ..AgentConfig::default()
        },
    )
    .with_session(session);

    let output = agent.run("hello").await.expect("max_turns=1 happy path");
    assert_eq!(output.turns, 1);
    assert_eq!(output.final_text, "hi");
    assert_eq!(provider.requests().len(), 1);

    let reopened = SessionStore::open(&dir, "one").unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(msgs[2].role, Role::Assistant);
    assert_eq!(msgs[2].content.as_deref(), Some("hi"));
}

/// **Edge case (Session 19)**: `max_turns = 1` with a tool-call
/// response. The loop runs the tool but cannot make a second provider
/// call to consume the result, so it MUST return `MaxTurns(1)`. The
/// session on disk must NOT be a dangling state — at minimum, every
/// persisted assistant-with-tool_calls message must have its matching
/// tool reply alongside it. Otherwise a later `--resume` would 400
/// the next provider call with an orphan `tool` message.
///
/// **Multi-step path**:
///
/// 1. Provider returns one assistant-with-tool_call (read_file on a
///    file that exists).
/// 2. Loop persists assistant, executes tool, persists tool reply.
/// 3. Loop iteration ends; for-range exhausted; loop exits.
/// 4. `Err(MaxTurns(1))` returned.
///
/// **Assertions**:
///
/// 1. `agent.run` returns `Err(MaxTurns(1))`.
/// 2. Provider was called exactly once.
/// 3. Reopened session contains both the assistant-with-tool_calls and
///    its matching tool reply — the assistant turn is NOT orphaned.
#[tokio::test]
async fn edge_max_turns_one_with_tool_call_does_not_persist_orphan() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "alpha\n").unwrap();

    let provider = ScriptedProvider::new(vec![assistant_calling_with_usage(
        "read_file",
        r#"{"path":"a.txt"}"#,
        "c1",
        usage(10, 3, 0, 0),
    )]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let session = SessionStore::open(&dir, "one_tool").unwrap();
    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            system_prompt: Some("sys".to_string()),
            max_turns: 1,
            ..AgentConfig::default()
        },
    )
    .with_session(session);

    let err = agent
        .run("read it")
        .await
        .expect_err("max_turns=1 + tool call must error");
    assert!(
        matches!(err, AgentError::MaxTurns(1)),
        "wrong error variant: {err:?}"
    );
    assert_eq!(provider.requests().len(), 1);

    let reopened = SessionStore::open(&dir, "one_tool").unwrap();
    let msgs = reopened.messages();

    let assistant_calls: Vec<&ChatMessage> =
        msgs.iter().filter(|m| !m.tool_calls.is_empty()).collect();
    let tool_replies: Vec<&ChatMessage> = msgs.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        assistant_calls.len(),
        1,
        "expected exactly one assistant-with-tool_calls on disk: {msgs:#?}"
    );
    assert_eq!(
        tool_replies.len(),
        1,
        "expected exactly one tool reply on disk (no orphan assistant): {msgs:#?}"
    );
    // Order: assistant must come before its tool reply.
    let asst_idx = msgs.iter().position(|m| !m.tool_calls.is_empty()).unwrap();
    let tool_idx = msgs.iter().position(|m| m.role == Role::Tool).unwrap();
    assert!(
        asst_idx < tool_idx,
        "tool reply persisted before its assistant turn: {msgs:#?}"
    );
}

/// **Edge case (Session 19)**: resume + first-turn compaction walk-back
/// interaction. The Session 18 fix forces compaction on the first turn
/// after resume. The compactor's walk-back logic must still keep an
/// assistant-with-tool_calls glued to its tool reply, even when the
/// preloaded transcript ends in a mid-tool-batch tail.
///
/// **Setup**: preload `[system, user, asst_with_tool_calls, tool_reply]`
/// (4 messages). Resume, send a new user prompt, configure tight
/// compaction (`keep_tail = 2`). The new transcript on entry to the
/// loop is 5 messages. The naive tail window would be `[tool_reply,
/// new_user]` — orphan tool. Walk-back must pull `asst_with_tool_calls`
/// in too.
///
/// **Assertions**:
///
/// 1. The first provider request must contain BOTH the
///    assistant-with-tool_calls and its matching tool reply.
/// 2. The tool reply must come AFTER the assistant turn.
/// 3. The synthetic compaction marker must be present (proves the
///    Session 18 fix and the walk-back path both ran).
/// 4. The new user prompt is the LAST message in the request.
#[tokio::test]
async fn edge_resume_with_mid_tool_batch_compaction_preserves_tool_pair() {
    let dir = tempdir();
    let mut session = SessionStore::open(&dir, "midbatch").unwrap();

    session.append(&ChatMessage::system("sys")).unwrap();
    session.append(&ChatMessage::user("earlier")).unwrap();
    let asst_calls = ChatMessage {
        role: Role::Assistant,
        content: None,
        content_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "c_old".to_string(),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            },
        }],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    session.append(&asst_calls).unwrap();
    session
        .append(&ChatMessage::tool_result("c_old", "read_file", "alpha"))
        .unwrap();
    assert_eq!(session.messages().len(), 4);

    let provider =
        ScriptedProvider::new(vec![assistant_final_with_usage("ack", usage(5, 2, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        compaction: CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        },
        max_turns: 4,
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    agent.run("next").await.expect("resume + compact run");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    let msgs = &reqs[0].messages;

    let asst_idx = msgs
        .iter()
        .position(|m| !m.tool_calls.is_empty())
        .expect("assistant-with-tool_calls dropped during compaction");
    let tool_idx = msgs
        .iter()
        .position(|m| m.role == Role::Tool)
        .expect("tool reply dropped during compaction — orphan would 400 the provider");
    assert!(
        asst_idx < tool_idx,
        "tool reply landed before its assistant turn: {msgs:#?}"
    );

    let has_synthetic = msgs.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        has_synthetic,
        "synthetic compaction marker missing — Session 18 fix regressed: {msgs:#?}"
    );

    let last = msgs.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.as_deref(), Some("next"));
}

// ============================================================================
// SESSION 20 — new edge cases
// ============================================================================

/// **Edge case (Session 20)**: the model emits a tool call whose
/// `arguments` JSON is malformed. The agent must NOT panic, NOT abort
/// the loop with an `AgentError`, and MUST surface the failure as a
/// conversational `tool` message so the model can self-correct on the
/// next turn. This is the same recovery contract as
/// `invariant_tool_failure_recovery_three_turn_state_machine`, but
/// stresses the *parse* failure path rather than the *execution*
/// failure path.
///
/// **Bug hypothesis**: a refactor that `unwrap()`s the
/// `serde_json::from_str` result on tool arguments would crash the
/// agent on a single bad token from the model. Equivalently, a
/// refactor that bubbled the parse error as `AgentError::Tool` would
/// abort the loop instead of letting the model self-correct.
///
/// **Multi-step path**:
///
/// 1. Turn 1 — model emits `read_file` with `arguments = "{not json"`.
///    Agent must persist a `tool` reply describing the error.
/// 2. Turn 2 — model emits a clean `read_file` on a real path.
///    Agent must execute it and persist the file body.
/// 3. Turn 3 — model emits final text. Loop terminates `Ok`.
///
/// **Assertions**:
///
/// 1. `agent.run` returns `Ok` (loop did not abort).
/// 2. Turn 2's request carries a `tool` reply whose body contains
///    "error" (the malformed-JSON path was reported as such).
/// 3. Turn 3's request carries BOTH tool replies; the recovery one
///    contains the real file body.
/// 4. `output.turns == 3`.
#[tokio::test]
async fn edge_malformed_tool_arguments_become_conversational_error_not_panic() {
    let dir = tempdir();
    fs::write(dir.join("good.txt"), "payload\n").unwrap();

    let script = vec![
        // Turn 1: malformed JSON arguments.
        assistant_calling_with_usage("read_file", r#"{not json"#, "bad_call", usage(8, 2, 0, 0)),
        // Turn 2: well-formed call against a real file.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"good.txt"}"#,
            "good_call",
            usage(12, 2, 0, 0),
        ),
        // Turn 3: terminal text.
        assistant_final_with_usage("done", usage(15, 4, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    );

    let output = agent
        .run("read")
        .await
        .expect("malformed JSON must NOT abort the loop");
    assert_eq!(output.turns, 3);

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    let turn2_tool = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must contain a tool reply for the malformed call");
    assert!(
        turn2_tool
            .content
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("error"),
        "expected error string in tool reply, got: {:?}",
        turn2_tool.content
    );

    let turn3_tools: Vec<&ChatMessage> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(
        turn3_tools.len(),
        2,
        "turn 3 should carry both the failed and the recovered tool reply"
    );
    assert!(
        turn3_tools[1]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("payload"),
        "recovery tool reply did not carry file body: {:?}",
        turn3_tools[1].content
    );
}

/// **Edge case (Session 20)**: a single assistant turn that emits
/// MULTIPLE tool calls. The agent must execute every call in order,
/// persist every reply with the matching `tool_call_id`, and the next
/// provider request must contain all of them adjacent to their
/// assistant turn.
///
/// **Bug hypothesis**: a refactor of the per-turn tool dispatch loop
/// could (a) execute only the first call, (b) run them out of order,
/// or (c) drop a `tool_call_id` mapping, any of which would cause
/// OpenAI-compatible providers to 400 the next request.
///
/// **Multi-step path**:
///
/// 1. Turn 1 — assistant emits TWO `read_file` tool_calls
///    (`a.txt`, `b.txt`) in a single message.
/// 2. Agent executes both and persists two `tool` replies in order.
/// 3. Turn 2 — assistant emits final text.
///
/// **Assertions**:
///
/// 1. Turn 2's request contains exactly two `tool` messages.
/// 2. The first tool reply pairs with `c_a` and contains "alpha".
/// 3. The second tool reply pairs with `c_b` and contains "beta".
/// 4. Both tool replies are positioned AFTER the assistant-with-calls
///    message in the request.
#[tokio::test]
async fn edge_multi_tool_batch_in_single_turn_executes_all_in_order() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    fs::write(dir.join("b.txt"), "beta\n").unwrap();

    let multi = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: Role::Assistant,
                content: None,
                content_blocks: Vec::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "c_a".to_string(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"a.txt"}"#.to_string(),
                        },
                    },
                    ToolCall {
                        id: "c_b".to_string(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"b.txt"}"#.to_string(),
                        },
                    },
                ],
                tool_call_id: None,
                name: None,
                protected: false,
                reasoning_content: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: Some(usage(20, 5, 0, 0)),
    };
    let script = vec![multi, assistant_final_with_usage("ok", usage(10, 3, 0, 0))];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    );

    let output = agent
        .run("multi")
        .await
        .expect("multi-tool batch must succeed");
    assert_eq!(output.turns, 2);

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);

    let turn2 = &reqs[1].messages;
    let asst_idx = turn2
        .iter()
        .position(|m| !m.tool_calls.is_empty())
        .expect("assistant-with-tool_calls missing in turn 2 request");
    let tool_msgs: Vec<&ChatMessage> = turn2.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        tool_msgs.len(),
        2,
        "expected exactly two tool replies in turn 2, got: {turn2:#?}"
    );
    // Both tool replies must come after the assistant turn that
    // requested them.
    for tm in &tool_msgs {
        let idx = turn2.iter().position(|m| std::ptr::eq(m, *tm)).unwrap();
        assert!(
            idx > asst_idx,
            "tool reply landed before its assistant turn"
        );
    }
    assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("c_a"));
    assert_eq!(tool_msgs[1].tool_call_id.as_deref(), Some("c_b"));
    assert!(
        tool_msgs[0]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("alpha"),
        "first reply was: {:?}",
        tool_msgs[0].content
    );
    assert!(
        tool_msgs[1]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("beta"),
        "second reply was: {:?}",
        tool_msgs[1].content
    );
}

/// **Edge case (Session 20)**: compaction's two guards meet at the
/// boundary `transcript.len() == keep_tail + 3`. At `keep_tail + 2`
/// the early-return guard fires (nothing to drop). At `keep_tail + 3`
/// exactly one message can be dropped — the smallest possible
/// reduction. This pins that boundary so a future `<=` ↔ `<` flip in
/// `compaction.rs` is caught immediately by an integration test (the
/// existing unit tests cover it directly, this one covers it through
/// the agent loop with real persistence).
///
/// **Bug hypothesis**: a refactor of the second guard
/// (`transcript.len() <= cfg.keep_tail + 2`) to `< cfg.keep_tail + 2`
/// would mis-fire and either (a) drop a real message at the boundary
/// minus one, or (b) leave a no-op compaction at the boundary itself.
///
/// **Setup**: preload `keep_tail + 2` messages
/// (`[system, u1, a1, u2]`), resume, send a new user prompt → on
/// loop entry the transcript is `keep_tail + 3 = 5` messages long.
/// Compaction must run and drop exactly one message from the middle.
///
/// **Assertions**:
///
/// 1. The first provider request contains a synthetic compaction
///    marker.
/// 2. The first request length is exactly `head + synthetic + tail +
///    new_user = 1 + 1 + keep_tail + 1 = keep_tail + 3`. (Same length,
///    different shape — one user message replaced by the synthetic.)
/// 3. The original `u1` message is GONE.
/// 4. The new user prompt is the LAST message.
#[tokio::test]
async fn edge_compaction_minimum_reduction_boundary_drops_exactly_one() {
    let dir = tempdir();
    let mut session = SessionStore::open(&dir, "min").unwrap();
    session.append(&ChatMessage::system("sys")).unwrap();
    session.append(&ChatMessage::user("u1_doomed")).unwrap();
    session.append(&ChatMessage::assistant_text("a1")).unwrap();
    session.append(&ChatMessage::user("u2")).unwrap();
    assert_eq!(session.messages().len(), 4); // keep_tail + 2

    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("ok", usage(5, 2, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let cfg = CompactionConfig {
        context_window: 1000,
        trigger_ratio: 0.5,
        keep_tail: 2,
    };
    let config = AgentConfig {
        compaction: cfg,
        max_turns: 4,
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    agent.run("u3").await.expect("run");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    let msgs = &reqs[0].messages;

    let has_synthetic = msgs.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        has_synthetic,
        "compaction did not run at the minimum-reduction boundary: {msgs:#?}"
    );

    // Original `u1_doomed` must have been dropped.
    let still_has_u1 = msgs
        .iter()
        .any(|m| m.content.as_deref() == Some("u1_doomed"));
    assert!(!still_has_u1, "expected u1_doomed to be dropped: {msgs:#?}");

    // New user prompt is the last message.
    let last = msgs.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.as_deref(), Some("u3"));
}

// ============================================================================
// SESSION 21 — streaming TextDelta ordering invariant
// ============================================================================

/// Provider variant whose `chat_stream` impl emits the assistant's
/// final text as a SEQUENCE of `TextDelta` chunks (instead of the
/// default impl's single delta). Lets the test exercise the real
/// chunked-streaming path that an SSE provider would walk.
struct ChunkedTextProvider {
    chunks: Vec<String>,
    full_response: ChatResponse,
}

impl ChunkedTextProvider {
    fn new(chunks: Vec<&str>, usage: Usage) -> Self {
        let full = chunks.concat();
        Self {
            chunks: chunks.into_iter().map(String::from).collect(),
            full_response: assistant_final_with_usage(&full, usage),
        }
    }
}

#[async_trait]
impl ChatProvider for ChunkedTextProvider {
    async fn chat(&self, _request: &ChatRequest) -> ApiResult<ChatResponse> {
        Ok(self.full_response.clone())
    }

    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> ApiResult<ChatResponse> {
        for c in &self.chunks {
            on_event(StreamEvent::TextDelta(c.clone()));
        }
        if let Some(u) = self.full_response.usage {
            on_event(StreamEvent::Usage(u));
        }
        Ok(self.full_response.clone())
    }
}

/// **Invariant (Session 21)**: every `TextDelta` event observed by the
/// stream callback, concatenated in arrival order, must equal the
/// final `AgentOutput.final_text` byte for byte. This is the contract
/// the REPL streaming UX (`crates/cli/src/repl.rs:165-168`) relies on
/// — it prints each delta to stdout as it arrives and never reprints
/// the final aggregated text. If the deltas diverged from the
/// aggregator (lost chunk, duplicated chunk, reordered chunk), REPL
/// users would see one thing on stdout and a different thing in the
/// session JSONL.
///
/// **Bug hypothesis**: a refactor that reads `TextDelta` chunks from
/// one pipe and the final text from another (e.g. SSE chunks vs the
/// response object) could let them disagree. A bug that splits a
/// multi-byte UTF-8 codepoint across chunks but only normalises one
/// of the two paths would break the equality.
///
/// **Multi-step path**:
///
/// 1. ChunkedTextProvider emits 5 distinct deltas: `["Hel", "lo, ",
///    "wor", "ld", "!"]`.
/// 2. Agent loop runs once, callback collects every TextDelta in order.
/// 3. After `agent.run` returns, the collected concatenation is
///    compared with `output.final_text`.
///
/// **Assertions**:
///
/// 1. Exactly 5 `TextDelta` events were observed (not 4, not 6 — proves
///    the chunked path actually ran, not the default single-delta one).
/// 2. Concatenated deltas, in order, equal `output.final_text`.
/// 3. Concatenated deltas equal the literal `"Hello, world!"`.
/// 4. The persisted assistant message on disk also equals
///    `"Hello, world!"` — the on-disk path must agree with the stream
///    path.
#[tokio::test]
async fn invariant_streaming_text_deltas_concatenate_to_final_text() {
    use std::sync::{Arc, Mutex};

    let dir = tempdir();
    let provider =
        ChunkedTextProvider::new(vec!["Hel", "lo, ", "wor", "ld", "!"], usage(9, 4, 0, 0));
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let deltas_cb = Arc::clone(&deltas);

    let session = SessionStore::open(&dir, "stream").unwrap();
    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            system_prompt: Some("sys".to_string()),
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_session(session)
    .with_stream_callback(move |event| {
        if let StreamEvent::TextDelta(t) = event {
            deltas_cb.lock().unwrap().push(t);
        }
    });

    let output = agent.run("greet").await.expect("run");

    let collected = deltas.lock().unwrap();
    assert_eq!(
        collected.len(),
        5,
        "expected 5 TextDelta events from the chunked path, got {}: {collected:?}",
        collected.len()
    );

    let joined: String = collected.concat();
    assert_eq!(
        joined, output.final_text,
        "INVARIANT VIOLATED: streamed deltas diverge from final_text\n  stream: {joined:?}\n  final:  {:?}",
        output.final_text
    );
    assert_eq!(joined, "Hello, world!");

    // On-disk path must agree with the stream path.
    let reopened = SessionStore::open(&dir, "stream").unwrap();
    let last = reopened
        .messages()
        .last()
        .cloned()
        .expect("at least one message");
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.content.as_deref(), Some("Hello, world!"));
}

// ============================================================================
// SESSION 22 — session ID collision under load + Config error variant
// ============================================================================

/// **Edge case (Session 22)**: a tight loop that mints session ids
/// back-to-back must never produce a duplicate, even if many calls
/// land in the same nanosecond. Pre-Session-22 the id was
/// `secs-pid-nanos`, which collided when two calls observed identical
/// nanosecond readings (the `new_id_is_unique_and_shortish` unit test
/// flaked under parallel execution as a result). The fix adds a
/// process-wide monotonic `AtomicU64` counter to the suffix.
///
/// **Bug hypothesis pinned**: any future refactor that drops the
/// monotonic suffix (e.g. "let's go back to a clean timestamp-only id
/// for prettier filenames") would re-introduce the collision class.
/// This test catches it deterministically by minting 10_000 ids in a
/// hot loop and checking the set size equals the count.
///
/// **Multi-step path**: mint → collect → set-size compare → spot
/// check that ids are well-formed (4 dash-separated hex segments).
///
/// **Assertions**:
///
/// 1. 10_000 minted ids are all unique.
/// 2. Every id has exactly 4 dash-separated segments (the new shape).
/// 3. The monotonic suffix strictly increases across consecutive ids
///    when parsed as hex.
#[tokio::test]
async fn edge_session_id_collision_under_load() {
    let n = 10_000;
    let ids: Vec<String> = (0..n).map(|_| SessionStore::new_id()).collect();

    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "session id collision: {} minted, {} unique",
        ids.len(),
        unique.len()
    );

    for id in &ids {
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 4, "id shape changed: {id}");
    }

    // The monotonic suffix must strictly increase across consecutive
    // ids minted on the same thread.
    let mut prev: Option<u64> = None;
    for id in &ids {
        let suffix_hex = id.rsplit('-').next().unwrap();
        let seq = u64::from_str_radix(suffix_hex, 16).expect("hex suffix");
        if let Some(p) = prev {
            assert!(seq > p, "monotonic counter regressed: {p} → {seq}");
        }
        prev = Some(seq);
    }
}

// ============================================================================
// SESSION 23 — permission gate hardening + audit trail
// ============================================================================

/// **Edge case (Session 23)**: a permission gate that DENIES a tool
/// call must surface the denial as a conversational `tool` reply that
/// the model can read on the next turn. The agent loop must NOT abort
/// — denial is a routine policy outcome, not a fatal error. The model
/// must be free to either retry with a different argument shape or
/// give up gracefully and emit a final text reply.
///
/// **Bug hypothesis pinned**: a refactor that bubbled
/// `PermissionDecision::Deny` as `AgentError::Permission` would abort
/// the loop on the first deny, breaking the "policy → conversation"
/// design. Equivalently, a refactor that swallowed the deny silently
/// and ran the tool anyway would defeat the gate.
///
/// **Multi-step path**:
///
/// 1. Turn 1 — model emits `read_file` against `forbidden.txt`.
///    `DenyAll("policy: read_file is gated")` denies. Tool reply
///    surfaces the reason verbatim.
/// 2. Turn 2 — model emits a final-text reply.
/// 3. Loop returns `Ok` with `turns == 2`.
///
/// **Assertions**:
///
/// 1. `agent.run` returns `Ok` (loop did not abort on deny).
/// 2. `output.turns == 2`.
/// 3. Turn 2's request carries a `tool` reply containing both
///    "permission denied" and the policy reason string.
/// 4. The forbidden file was NEVER actually read (the file does not
///    exist; if the gate had been bypassed the tool would have
///    surfaced an "no such file" error instead of the deny string).
#[tokio::test]
async fn edge_permission_deny_surfaces_as_tool_reply_not_abort() {
    let dir = tempdir();
    // Deliberately do NOT create forbidden.txt; if the gate fails open,
    // the test sees a different error string and fails on the contains
    // assertion.

    let script = vec![
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"forbidden.txt"}"#,
            "c1",
            usage(8, 2, 0, 0),
        ),
        assistant_final_with_usage("ok, giving up", usage(12, 5, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let policy = std::sync::Arc::new(DenyAll("policy: read_file is gated".to_string()));

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_permission(policy);

    let output = agent
        .run("read it")
        .await
        .expect("deny must NOT abort the loop");
    assert_eq!(output.turns, 2);
    assert_eq!(output.final_text, "ok, giving up");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);
    let turn2_tool = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must contain a tool reply for the denied call");
    let body = turn2_tool.content.as_deref().unwrap_or("");
    assert!(
        body.contains("permission denied"),
        "tool reply missing 'permission denied' marker: {body:?}"
    );
    assert!(
        body.contains("policy: read_file is gated"),
        "tool reply missing policy reason: {body:?}"
    );
}

/// **Edge case (Session 23)**: an `AuditingPermission` decorator
/// must record one JSONL line per `check` call, capturing the tool
/// name, the parsed arguments, the boolean decision, and the deny
/// reason if any. The log must survive across multiple turns of the
/// same agent run, and the order of entries must match the order of
/// tool dispatch.
///
/// **Bug hypothesis pinned**: a refactor that batched audit writes
/// could lose the last entry on early loop exit; one that wrote on
/// `Deny` only would leave allows invisible (compliance gap); one
/// that wrote on `Allow` only would leave denies invisible
/// (security gap). The test pins both directions.
///
/// **Multi-step path**: 3 turns. Turn 1: an `Allow`-routed
/// `read_file` (auto-allowed). Turn 2: a `Deny`-routed
/// `read_file`. Turn 3: final text. Note: `DenyAll` denies all,
/// so for this test we use a custom `MixedPermission` that allows
/// the first call and denies the second.
///
/// **Assertions**:
///
/// 1. The audit log file exists after the run.
/// 2. It contains exactly 2 lines (one per tool dispatch — turn 3
///    has no tool call).
/// 3. Line 1 is `{"tool":"read_file","args":{...},"allowed":true,
///    "reason":null}`.
/// 4. Line 2 is `{"tool":"read_file","args":{...},"allowed":false,
///    "reason":"second-call denied"}`.
/// 5. The order of entries matches the order of dispatch.
#[tokio::test]
async fn edge_permission_audit_log_records_decision() {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Allows the first call, denies the second. Lets the test pin
    /// BOTH directions of the audit trail in one run.
    struct MixedPermission {
        seen: AtomicU32,
    }
    impl aegis_core::Permission for MixedPermission {
        fn check(&self, _tool: &str, _args: &serde_json::Value) -> aegis_core::PermissionDecision {
            let n = self.seen.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                aegis_core::PermissionDecision::Allow
            } else {
                aegis_core::PermissionDecision::Deny("second-call denied".to_string())
            }
        }
    }

    let dir = tempdir();
    fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    let log_path = dir.join("audit.jsonl");

    let script = vec![
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", usage(8, 2, 0, 0)),
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c2", usage(10, 2, 0, 0)),
        assistant_final_with_usage("done", usage(12, 4, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let auditing = std::sync::Arc::new(AuditingPermission::new(
        MixedPermission {
            seen: AtomicU32::new(0),
        },
        &log_path,
    ));

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_permission(auditing);

    let output = agent.run("go").await.expect("run");
    assert_eq!(output.turns, 3);

    let log = fs::read_to_string(&log_path).expect("audit log written");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 audit lines (one per tool dispatch), got: {log}"
    );

    let line1: serde_json::Value = serde_json::from_str(lines[0]).expect("line 1 valid json");
    assert_eq!(line1["tool"], "read_file");
    assert_eq!(line1["allowed"], true);
    assert!(line1["reason"].is_null());

    let line2: serde_json::Value = serde_json::from_str(lines[1]).expect("line 2 valid json");
    assert_eq!(line2["tool"], "read_file");
    assert_eq!(line2["allowed"], false);
    assert_eq!(line2["reason"], "second-call denied");
}

// ============================================================================
// SESSION 24 — tool output truncation × compaction interaction
// ============================================================================

/// **Edge case (Session 24)**: a `read_file` against a multi-megabyte
/// text file must NOT return the full body. Pre-Session-24
/// `ReadFile::execute` had no byte cap and would happily blow the
/// context window. Fix: `READ_FILE_MAX_BYTES` (~48 KB) — output over
/// the cap is truncated at a UTF-8 boundary and a "[truncated: …]"
/// placeholder is appended.
///
/// **Bug hypothesis pinned**: a refactor that drops the cap (or moves
/// it to a config knob with a missing default) re-introduces the
/// context-blow class.
///
/// **Assertions**:
///
/// 1. Loop returns `Ok`, `output.turns == 2`.
/// 2. Persisted tool reply length is <= 60 KB.
/// 3. Reply contains the literal "truncated".
/// 4. Reply length is strictly less than the source file size.
#[tokio::test]
async fn edge_huge_tool_output_does_not_blow_context_window() {
    let dir = tempdir();
    let line = "the quick brown fox jumps over the lazy dog\n";
    let mut huge = String::with_capacity(200_000);
    while huge.len() < 200_000 {
        huge.push_str(line);
    }
    fs::write(dir.join("huge.txt"), &huge).unwrap();

    let script = vec![
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"huge.txt"}"#,
            "c1",
            usage(20, 4, 0, 0),
        ),
        assistant_final_with_usage("read it", usage(30, 6, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let session = SessionStore::open(&dir, "huge").unwrap();

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_session(session);

    let output = agent.run("read huge").await.expect("loop must not blow up");
    assert_eq!(output.turns, 2);

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);
    let turn2_tool = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must contain a tool reply");
    let body = turn2_tool.content.as_deref().unwrap_or("");
    assert!(
        body.len() <= 60_000,
        "tool reply not truncated: {} bytes",
        body.len()
    );
    assert!(
        body.len() < huge.len(),
        "reply ({} bytes) was not smaller than source ({} bytes)",
        body.len(),
        huge.len()
    );
    assert!(
        body.contains("truncated"),
        "tool reply missing 'truncated' marker"
    );
}

/// **Invariant (Session 24)**: a truncated tool reply must round-trip
/// cleanly through the compactor. The walk-back logic that keeps an
/// assistant-with-tool_calls glued to its tool reply must not break
/// when the reply has been clipped, and rebuilding the transcript
/// must not re-expand or duplicate the reply.
///
/// **Bug hypothesis pinned**: a refactor that drops, replaces, or
/// reorders truncated replies during compaction would 400 the next
/// provider call (orphan tool message). One that re-expanded
/// truncated replies on rebuild would defeat the cap.
///
/// **Assertions**:
///
/// 1. Loop returns `Ok`, `output.turns == 3`.
/// 2. Turn 2's request contains the synthetic compaction marker.
/// 3. Turn 2 still has its assistant-with-tool_calls / tool-reply pair
///    in the right order.
/// 4. Every tool reply in turn 2 is still <= 60 KB after rebuild.
#[tokio::test]
async fn invariant_truncated_tool_reply_still_round_trips_through_compaction() {
    let dir = tempdir();
    let line = "the quick brown fox jumps over the lazy dog\n";
    let mut huge = String::with_capacity(200_000);
    while huge.len() < 200_000 {
        huge.push_str(line);
    }
    fs::write(dir.join("huge.txt"), &huge).unwrap();
    fs::write(dir.join("small.txt"), "small\n").unwrap();

    // Preload prior chatter so the transcript on entry to turn 2 is
    // comfortably above the `keep_tail + 2` early-return guard.
    let mut session = SessionStore::open(&dir, "rt").unwrap();
    session.append(&ChatMessage::system("sys")).unwrap();
    for i in 0..3 {
        session.append(&ChatMessage::user(format!("u{i}"))).unwrap();
        session
            .append(&ChatMessage::assistant_text(format!("a{i}")))
            .unwrap();
    }

    let script = vec![
        // Cache addends push the reconstructed depth over the trigger
        // on entry to turn 2.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"huge.txt"}"#,
            "c1",
            usage(300, 4, 400, 100),
        ),
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"small.txt"}"#,
            "c2",
            usage(20, 4, 0, 0),
        ),
        assistant_final_with_usage("done", usage(10, 4, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        system_prompt: Some("sys".to_string()),
        compaction: CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.75, // trigger = 750
            keep_tail: 2,
        },
        max_turns: 4,
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    let output = agent.run("go").await.expect("run");
    assert_eq!(output.turns, 3);

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    let turn2 = &reqs[1].messages;
    let has_synthetic = turn2.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        has_synthetic,
        "compaction did not run for turn 2: {turn2:#?}"
    );

    let asst_idx = turn2
        .iter()
        .position(|m| !m.tool_calls.is_empty())
        .expect("turn 2 must contain an assistant-with-tool_calls");
    let tool_idx = turn2
        .iter()
        .position(|m| m.role == Role::Tool)
        .expect("turn 2 must contain a tool reply");
    assert!(
        asst_idx < tool_idx,
        "tool reply landed before its assistant turn: {turn2:#?}"
    );

    for tm in turn2.iter().filter(|m| m.role == Role::Tool) {
        let body = tm.content.as_deref().unwrap_or("");
        assert!(
            body.len() <= 60_000,
            "round-tripped tool reply blew the cap: {} bytes",
            body.len()
        );
    }
}

// ============================================================================
// Session 25 — Resume hardening: tolerant JSONL load.
// ============================================================================

/// Builds a valid JSONL byte stream of N user/assistant pairs prefixed
/// by a system message. Used by both Session 25 recovery tests so the
/// "good prefix" they assert on is identical between cases.
fn good_jsonl_prefix(pairs: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let sys = serde_json::to_string(&ChatMessage::system("sys")).unwrap();
    out.extend_from_slice(sys.as_bytes());
    out.push(b'\n');
    for i in 0..pairs {
        let u = serde_json::to_string(&ChatMessage::user(format!("u{i}"))).unwrap();
        out.extend_from_slice(u.as_bytes());
        out.push(b'\n');
        let a = serde_json::to_string(&ChatMessage::assistant_text(format!("a{i}"))).unwrap();
        out.extend_from_slice(a.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Locates the on-disk path of a session file under a workspace.
fn session_path(workspace: &std::path::Path, id: &str) -> PathBuf {
    workspace
        .join(".metis")
        .join("sessions")
        .join(format!("{id}.jsonl"))
}

/// **Bug-or-edge (Session 25)**: a session whose last line is
/// truncated mid-JSON (the previous run was killed mid-`append` after
/// the bytes hit the page cache but before the trailing `\n`) must
/// still be openable. The good prefix loads, the broken tail is
/// dropped, and recovery stats reflect what happened.
///
/// **Bug hypothesis pinned**: a refactor that propagates
/// `serde_json::Error` out of `load` would lock the user out of every
/// session that crashed mid-write. Pre-Session 25 behaviour was
/// exactly that — `load` returned `SessionError::Decode` on the bad
/// line. This test would have failed against the old loader.
///
/// **Assertions**:
///
/// 1. `SessionStore::open` returns `Ok`.
/// 2. The good prefix (5 messages) is loaded verbatim.
/// 3. `recovery_stats().recovered == 5`, `skipped == 1`.
/// 4. Re-opening after a fresh `append` round-trips the new tail too.
#[tokio::test]
async fn bug_or_edge_resume_with_truncated_last_line_recovers() {
    let dir = tempdir();
    // Create the .metis/sessions directory by opening a throw-away
    // session — this is the only way the test can avoid duplicating
    // the path layout knowledge baked into SessionStore::open.
    {
        let _ = SessionStore::open(&dir, "warmup").unwrap();
    }

    let mut bytes = good_jsonl_prefix(2); // [sys, u0, a0, u1, a1] = 5 lines
                                          // Append a half-written user message — opening `{` without the
                                          // closing brace, no trailing newline. This is exactly what a
                                          // killed-mid-append leaves behind.
    bytes.extend_from_slice(br#"{"role":"user","content":"oh n"#);

    let path = session_path(&dir, "crashed");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&bytes).unwrap();
    drop(f);

    let store = SessionStore::open(&dir, "crashed").expect("tolerant load must succeed");
    assert_eq!(
        store.messages().len(),
        5,
        "good prefix length mismatch: {:#?}",
        store.messages()
    );
    let stats = store.recovery_stats();
    assert_eq!(stats.recovered, 5, "stats: {stats:?}");
    assert_eq!(stats.skipped, 1, "stats: {stats:?}");

    // Make sure the in-memory state actually matches what was on disk.
    assert_eq!(store.messages()[0].content.as_deref(), Some("sys"));
    assert_eq!(store.messages()[1].content.as_deref(), Some("u0"));
    assert_eq!(store.messages()[4].content.as_deref(), Some("a1"));

    // Append after recovery and re-open: the new tail must persist
    // alongside the recovered prefix.
    let mut store = store;
    store.append(&ChatMessage::user("after recovery")).unwrap();
    let reopened = SessionStore::open(&dir, "crashed").unwrap();
    assert_eq!(reopened.messages().len(), 6);
    assert_eq!(
        reopened.messages()[5].content.as_deref(),
        Some("after recovery")
    );
    // Note: after the clean append, the broken bytes are still
    // physically in the file before the new line, so they will still
    // count as 1 skipped on the next open.
    assert_eq!(reopened.recovery_stats().skipped, 1);
}

/// **Edge (Session 25)**: a session where invalid UTF-8 bytes appear
/// *between* two valid JSONL lines must skip the bad line and keep
/// loading the rest. Pins the bug-class where a single byte error
/// halts the loader and discards everything after it.
///
/// **Assertions**:
///
/// 1. `SessionStore::open` returns `Ok`.
/// 2. Both surrounding good lines are present, in order.
/// 3. `recovery_stats().recovered == 2`, `skipped == 1`.
#[tokio::test]
async fn edge_resume_with_invalid_utf8_in_middle_skips_and_continues() {
    let dir = tempdir();
    {
        let _ = SessionStore::open(&dir, "warmup").unwrap();
    }

    let good_a = serde_json::to_string(&ChatMessage::user("before")).unwrap();
    let good_b = serde_json::to_string(&ChatMessage::assistant_text("after")).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(good_a.as_bytes());
    bytes.push(b'\n');
    // A line that is structurally a JSONL line (terminated by \n) but
    // whose content is not valid UTF-8: a lone 0xFF byte.
    bytes.extend_from_slice(&[0xFFu8, 0xFE, 0xFD]);
    bytes.push(b'\n');
    bytes.extend_from_slice(good_b.as_bytes());
    bytes.push(b'\n');

    let path = session_path(&dir, "utf8bad");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&bytes).unwrap();
    drop(f);

    let store = SessionStore::open(&dir, "utf8bad").expect("tolerant load must succeed");
    assert_eq!(store.messages().len(), 2, "{:#?}", store.messages());
    assert_eq!(store.messages()[0].content.as_deref(), Some("before"));
    assert_eq!(store.messages()[1].content.as_deref(), Some("after"));
    let stats = store.recovery_stats();
    assert_eq!(stats.recovered, 2, "{stats:?}");
    assert_eq!(stats.skipped, 1, "{stats:?}");
}

// ============================================================================
// Session 30 — Subagent infrastructure
//
// Five tests pin the contract:
//   * invariant: spawn returns brief description + final text + usage
//   * invariant: parent's session/transcript is untouched after spawn
//   * edge: subagent error is surfaced, parent state still untouched
//   * edge: parent and subagent run on the same workspace without
//           sharing transcript bytes
//   * edge: spawning twice gives two independent runs (no carry-over)
// ============================================================================

#[tokio::test]
async fn invariant_subagent_spawn_returns_text_and_usage() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "child answer",
        usage(7, 3, 0, 0),
    )]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        max_turns: 4,
        ..AgentConfig::default()
    };

    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);
    let report = sub
        .spawn(SubagentBrief {
            description: "answer one thing".to_string(),
            prompt: "what is 2 + 2".to_string(),
            system_prompt: Some("be terse".to_string()),
        })
        .await
        .expect("spawn must succeed");

    assert_eq!(report.description, "answer one thing");
    assert_eq!(report.final_text, "child answer");
    assert_eq!(report.usage.input_tokens, 7);
    assert_eq!(report.usage.output_tokens, 3);
    assert_eq!(report.turns, 1);

    // Provider must have seen the system override and the user prompt.
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    let msgs = &reqs[0].messages;
    assert!(msgs
        .iter()
        .any(|m| m.role == Role::System && m.content.as_deref() == Some("be terse")));
    assert!(msgs
        .iter()
        .any(|m| m.role == Role::User && m.content.as_deref() == Some("what is 2 + 2")));
}

#[tokio::test]
async fn invariant_subagent_does_not_touch_parent_session() {
    let dir = tempdir();

    // Parent agent: completes one turn, persists to its session.
    let parent_provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "parent ok",
        usage(2, 2, 0, 0),
    )]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let parent_session = SessionStore::open(&dir, "parent").unwrap();
    let mut parent = Agent::new(
        &parent_provider as &dyn ChatProvider,
        &registry,
        ctx.clone(),
        AgentConfig {
            system_prompt: Some("psys".to_string()),
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_session(parent_session);
    parent.run("parent prompt").await.unwrap();

    let parent_msgs_before: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert!(!parent_msgs_before.is_empty());

    // Subagent run on the SAME workspace, with its own provider.
    let child_provider =
        ScriptedProvider::new(vec![assistant_final_with_usage("child", usage(1, 1, 0, 0))]);
    let sub = Subagent::new(
        &child_provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            system_prompt: Some("csys".to_string()),
            max_turns: 4,
            ..AgentConfig::default()
        },
    );
    let _ = sub
        .spawn(SubagentBrief {
            description: "child task".to_string(),
            prompt: "child prompt".to_string(),
            system_prompt: None,
        })
        .await
        .unwrap();

    // Parent's in-memory session bytes are unchanged.
    let parent_msgs_after: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(parent_msgs_before, parent_msgs_after);

    // Parent session file on disk also unchanged — reopen and compare.
    let reopened = SessionStore::open(&dir, "parent").unwrap();
    let reopened_msgs: Vec<String> = reopened
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(reopened_msgs, parent_msgs_after);

    // The subagent must NOT have created a "child" session file —
    // attempting to open one fresh should yield an empty store.
    let probe = SessionStore::open(&dir, "child").unwrap();
    assert!(
        probe.messages().is_empty(),
        "subagent leaked transcript into a session file: {:#?}",
        probe.messages()
    );
}

#[tokio::test]
async fn edge_subagent_error_does_not_corrupt_parent() {
    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    // Parent runs once, successfully.
    let parent_provider =
        ScriptedProvider::new(vec![assistant_final_with_usage("p", usage(1, 1, 0, 0))]);
    let parent_session = SessionStore::open(&dir, "p").unwrap();
    let mut parent = Agent::new(
        &parent_provider as &dyn ChatProvider,
        &registry,
        ctx.clone(),
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    )
    .with_session(parent_session);
    parent.run("hello").await.unwrap();
    let snapshot_before: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    // Subagent: empty queue → ScriptedProvider would panic on call,
    // so use max_turns = 0 to force AgentError::Config before any call.
    let child_provider = ScriptedProvider::new(vec![]);
    let sub = Subagent::new(
        &child_provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 0,
            ..AgentConfig::default()
        },
    );
    let err = sub
        .spawn(SubagentBrief {
            description: "broken".to_string(),
            prompt: "doomed".to_string(),
            system_prompt: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Config(_)), "got {err:?}");

    // Parent state is byte-for-byte unchanged.
    let snapshot_after: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(snapshot_after, snapshot_before);
}

#[tokio::test]
async fn edge_subagent_inherits_base_system_prompt_when_brief_omits_it() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("ok", usage(2, 1, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        system_prompt: Some("base sys".to_string()),
        max_turns: 2,
        ..AgentConfig::default()
    };
    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);
    sub.spawn(SubagentBrief {
        description: "x".to_string(),
        prompt: "go".to_string(),
        system_prompt: None,
    })
    .await
    .unwrap();

    let reqs = provider.requests();
    let sys_msgs: Vec<_> = reqs[0]
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .collect();
    assert_eq!(sys_msgs.len(), 1);
    assert_eq!(sys_msgs[0].content.as_deref(), Some("base sys"));
}

#[tokio::test]
async fn edge_two_spawns_are_independent_no_carry_over() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![
        assistant_final_with_usage("first", usage(3, 1, 0, 0)),
        assistant_final_with_usage("second", usage(5, 2, 0, 0)),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    );

    let r1 = sub
        .spawn(SubagentBrief {
            description: "a".to_string(),
            prompt: "first prompt".to_string(),
            system_prompt: None,
        })
        .await
        .unwrap();
    let r2 = sub
        .spawn(SubagentBrief {
            description: "b".to_string(),
            prompt: "second prompt".to_string(),
            system_prompt: None,
        })
        .await
        .unwrap();

    assert_eq!(r1.final_text, "first");
    assert_eq!(r2.final_text, "second");
    assert_eq!(r1.usage.input_tokens, 3);
    assert_eq!(r2.usage.input_tokens, 5);

    // Each spawn should have produced exactly ONE provider call, and
    // the second call must NOT contain the first prompt anywhere in
    // its messages — that would prove transcript carry-over.
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1]
        .messages
        .iter()
        .all(|m| m.content.as_deref() != Some("first prompt")));
    assert!(reqs[1]
        .messages
        .iter()
        .all(|m| m.content.as_deref() != Some("first")));
}

// ============================================================================
// Session 31 — SubagentType, allowlist permission, briefing format
//
// Six tests extend the Session 30 contract with the typed-spawn path:
//   * invariant: spawn_typed applies the type's system_prompt OVER the
//     base AgentConfig's system_prompt (classic precedence bug)
//   * invariant: a brief-level system_prompt still beats the type default
//     — the override ordering is brief > type > base
//   * edge:      allowlist denies a non-member tool, the denial surfaces
//                as a tool reply the model can read, and a SECOND tool
//                call against a listed tool runs for real — 3 provider
//                calls, full recovery path pinned
//   * edge:      allowlist is an AND-gate: a listed tool still has to
//                pass the INNER permission, so a `DenyAll` underneath
//                rejects even allowlisted tools (and the inner's reason
//                is what reaches the model, not the allowlist's)
//   * edge:      format_briefing shape is a stable contract — three
//                inputs (plain / newline in description / empty prompt)
//                each pin exact bytes
//   * edge:      a typed spawn whose tool call gets denied must not
//                corrupt the parent session on disk — byte-for-byte
//                comparison before and after
// ============================================================================

#[tokio::test]
async fn invariant_spawn_typed_uses_type_system_prompt_over_base() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("ok", usage(4, 1, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    // Base config carries a deliberately distinctive system prompt so
    // any accidental fall-through is trivial to spot in the assertion.
    let config = AgentConfig {
        system_prompt: Some("BASE_PARENT_SYSTEM_PROMPT".to_string()),
        max_turns: 2,
        ..AgentConfig::default()
    };
    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    let ty = SubagentType::general_purpose();
    let expected_type_sys = ty.system_prompt.clone();

    sub.spawn_typed(
        &ty,
        SubagentBrief {
            description: "some task".to_string(),
            prompt: "do a thing".to_string(),
            system_prompt: None,
        },
    )
    .await
    .expect("typed spawn must succeed");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1, "expected one provider call");

    let sys_msgs: Vec<&ChatMessage> = reqs[0]
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .collect();
    assert_eq!(
        sys_msgs.len(),
        1,
        "spawn_typed must emit exactly one system message, got {sys_msgs:#?}"
    );
    assert_eq!(
        sys_msgs[0].content.as_deref(),
        Some(expected_type_sys.as_str()),
        "spawn_typed leaked the BASE system prompt instead of the type default"
    );
    // Safety net: the base string must not appear anywhere in the
    // request — not even as a prefix fragment — or a partial refactor
    // could paste both system prompts end-to-end and slip past the
    // first assertion.
    assert!(
        reqs[0].messages.iter().all(|m| !m
            .content
            .as_deref()
            .unwrap_or("")
            .contains("BASE_PARENT_SYSTEM_PROMPT")),
        "base system prompt leaked into messages: {:#?}",
        reqs[0].messages
    );
}

#[tokio::test]
async fn invariant_spawn_typed_brief_override_beats_type_default() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("ok", usage(3, 1, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        system_prompt: Some("BASE_SYS".to_string()),
        max_turns: 2,
        ..AgentConfig::default()
    };
    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    let ty = SubagentType::general_purpose();
    let type_default = ty.system_prompt.clone();
    let override_sys = "BRIEF_LEVEL_OVERRIDE_WINS".to_string();

    sub.spawn_typed(
        &ty,
        SubagentBrief {
            description: "d".to_string(),
            prompt: "p".to_string(),
            system_prompt: Some(override_sys.clone()),
        },
    )
    .await
    .unwrap();

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    let sys_msgs: Vec<&ChatMessage> = reqs[0]
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .collect();
    assert_eq!(sys_msgs.len(), 1);
    assert_eq!(sys_msgs[0].content.as_deref(), Some(override_sys.as_str()));
    // Neither the type default NOR the base prompt may appear anywhere
    // in the request — the brief override is the ONLY system the child
    // should have seen.
    for m in &reqs[0].messages {
        let c = m.content.as_deref().unwrap_or("");
        assert!(
            !c.contains(type_default.as_str()),
            "type default leaked with brief override present: {c:?}"
        );
        assert!(
            !c.contains("BASE_SYS"),
            "base system leaked with brief override present: {c:?}"
        );
    }
}

#[tokio::test]
async fn edge_spawn_typed_allowlist_denies_nonmember_then_agent_recovers() {
    let dir = tempdir();
    // The file the recovery turn will read for real.
    fs::write(dir.join("target.txt"), "hello world\n").unwrap();

    // Turn 1: model calls `bash` — not on the allowlist, must be denied.
    // Turn 2: model self-corrects, calls `read_file` on a real file.
    // Turn 3: plain text reply, loop terminates.
    let script = vec![
        assistant_calling_with_usage(
            "bash",
            r#"{"command":"echo denied"}"#,
            "call_bash",
            usage(8, 4, 0, 0),
        ),
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"target.txt"}"#,
            "call_read",
            usage(12, 4, 0, 0),
        ),
        assistant_final_with_usage("recovered and done", usage(20, 6, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 5,
            ..AgentConfig::default()
        },
    );

    let ty = SubagentType {
        name: "readonly-subagent".to_string(),
        description: "reads files, nothing else".to_string(),
        system_prompt: "you may only read files".to_string(),
        allowed_tools: Some(vec!["read_file".to_string()]),
    };

    let report = sub
        .spawn_typed(
            &ty,
            SubagentBrief {
                description: "explore".to_string(),
                prompt: "look at target.txt".to_string(),
                system_prompt: None,
            },
        )
        .await
        .expect("loop must survive a permission denial");

    assert_eq!(report.turns, 3, "full 3-turn state machine required");
    assert_eq!(report.final_text, "recovered and done");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3, "expected three provider calls");

    // Turn 2's request must carry the denied tool reply, and the reply
    // must name BOTH the tool and the allowlist — vague errors would
    // leave the model guessing.
    let turn2_tool = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must see the denied tool reply");
    let turn2_body = turn2_tool.content.as_deref().unwrap_or("");
    assert!(
        turn2_body.contains("permission denied"),
        "denial reply missing `permission denied`: {turn2_body:?}"
    );
    assert!(
        turn2_body.contains("bash"),
        "denial reply must name the offending tool: {turn2_body:?}"
    );
    assert!(
        turn2_body.contains("allowlist"),
        "denial reply must cite the allowlist: {turn2_body:?}"
    );

    // Turn 3's request must carry BOTH tool replies (denied + recovered),
    // and the second one must contain the real file body.
    let turn3_tools: Vec<&ChatMessage> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(
        turn3_tools.len(),
        2,
        "turn 3 should carry both tool replies (denied + recovered)"
    );
    assert!(
        turn3_tools[1]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("hello world"),
        "recovery tool reply missing file body: {:?}",
        turn3_tools[1].content
    );

    // Safety: the allowlist's type system_prompt must have been applied,
    // not the default — brief.system_prompt was None, and SubagentType
    // overrode it, so "you may only read files" must be the one system
    // message the provider saw on every turn.
    for (i, r) in reqs.iter().enumerate() {
        let sys_count = r.messages.iter().filter(|m| m.role == Role::System).count();
        assert_eq!(sys_count, 1, "turn {i} must carry one system message");
        let sys_body = r
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");
        assert_eq!(
            sys_body, "you may only read files",
            "turn {i} system prompt drifted"
        );
    }
}

#[tokio::test]
async fn edge_spawn_typed_allowlist_still_consults_inner_permission() {
    let dir = tempdir();
    fs::write(dir.join("doc.txt"), "unreachable body\n").unwrap();

    // Two turns: the inner permission denies, model gives up with a
    // plain text reply.
    let script = vec![
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"doc.txt"}"#,
            "call_read",
            usage(5, 2, 0, 0),
        ),
        assistant_final_with_usage("can't read, giving up", usage(9, 3, 0, 0)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    // Inner permission is DenyAll — even for tools on the allowlist the
    // inner must still be consulted. This pins the AND-gate contract:
    // allowlist is necessary but not sufficient for Allow.
    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    )
    .with_permission(std::sync::Arc::new(DenyAll(
        "INNER_BLOCKED_BY_PARENT_POLICY".to_string(),
    )));

    let ty = SubagentType {
        name: "reader".to_string(),
        description: "reads".to_string(),
        system_prompt: "reader".to_string(),
        allowed_tools: Some(vec!["read_file".to_string()]),
    };

    let report = sub
        .spawn_typed(
            &ty,
            SubagentBrief {
                description: "d".to_string(),
                prompt: "p".to_string(),
                system_prompt: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(report.turns, 2);
    assert_eq!(report.final_text, "can't read, giving up");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);

    // Turn 2's tool reply must surface the INNER permission's reason,
    // NOT the allowlist message — read_file is on the allowlist, so it
    // reached the inner, and the inner denied.
    let tool_reply = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("turn 2 must carry a tool reply")
        .content
        .as_deref()
        .unwrap_or("");
    assert!(
        tool_reply.contains("INNER_BLOCKED_BY_PARENT_POLICY"),
        "inner permission reason missing — allowlist short-circuited: {tool_reply:?}"
    );
    assert!(
        !tool_reply.contains("allowlist"),
        "allowlist short-circuit triggered for an allowlisted tool: {tool_reply:?}"
    );

    // The real file must NOT have been read — the tool body is
    // recognisable and would be a smoking gun for an actual execution.
    assert!(
        !tool_reply.contains("unreachable body"),
        "tool executed despite denial: {tool_reply:?}"
    );
}

#[tokio::test]
async fn edge_format_briefing_shape_is_stable_markdown() {
    // Plain case pins the canonical shape.
    let plain = format_briefing(&SubagentBrief {
        description: "find the bug".to_string(),
        prompt: "look in src/foo.rs".to_string(),
        system_prompt: None,
    });
    assert_eq!(plain, "# Task: find the bug\n\nlook in src/foo.rs");

    // A newline in the description must pass through verbatim. If a
    // future refactor flattens descriptions for a single-line preview,
    // it must not do so in the briefing format — that surface is the
    // child's only context.
    let multi_desc = format_briefing(&SubagentBrief {
        description: "line 1\nline 2".to_string(),
        prompt: "body".to_string(),
        system_prompt: None,
    });
    assert_eq!(multi_desc, "# Task: line 1\nline 2\n\nbody");

    // An empty prompt still produces the canonical two-newline gap
    // followed by the empty body — no trimming, no default filler.
    let empty_prompt = format_briefing(&SubagentBrief {
        description: "desc".to_string(),
        prompt: String::new(),
        system_prompt: None,
    });
    assert_eq!(empty_prompt, "# Task: desc\n\n");

    // system_prompt field has no influence on the formatted briefing
    // — it rides a separate channel (AgentConfig.system_prompt). Pin
    // the invariance so a refactor can't quietly start embedding it.
    let with_sys = format_briefing(&SubagentBrief {
        description: "d".to_string(),
        prompt: "p".to_string(),
        system_prompt: Some("SHOULD_NOT_APPEAR_IN_BRIEFING".to_string()),
    });
    assert_eq!(with_sys, "# Task: d\n\np");
    assert!(!with_sys.contains("SHOULD_NOT_APPEAR_IN_BRIEFING"));
}

#[tokio::test]
async fn edge_spawn_typed_denial_does_not_corrupt_parent_session() {
    let dir = tempdir();

    // Parent: one normal turn, persists to its own session.
    let parent_provider =
        ScriptedProvider::new(vec![assistant_final_with_usage("p ok", usage(2, 2, 0, 0))]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let parent_session = SessionStore::open(&dir, "parent_typed").unwrap();
    let mut parent = Agent::new(
        &parent_provider as &dyn ChatProvider,
        &registry,
        ctx.clone(),
        AgentConfig {
            system_prompt: Some("psys".to_string()),
            max_turns: 2,
            ..AgentConfig::default()
        },
    )
    .with_session(parent_session);
    parent.run("parent prompt").await.unwrap();

    let before: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert!(!before.is_empty());

    // Child: denied tool call → recovery → final text. Same workspace
    // as the parent. The child's tool_call MUST be denied by the
    // allowlist (bash is not listed), the loop must recover, and when
    // the dust settles the parent's session file must be byte-for-byte
    // identical to what it was before.
    let child_provider = ScriptedProvider::new(vec![
        assistant_calling_with_usage(
            "bash",
            r#"{"command":"whoami"}"#,
            "cbash",
            usage(4, 2, 0, 0),
        ),
        assistant_final_with_usage("gave up", usage(7, 2, 0, 0)),
    ]);
    let sub = Subagent::new(
        &child_provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 4,
            ..AgentConfig::default()
        },
    );
    let ty = SubagentType {
        name: "strict".to_string(),
        description: "strict reader".to_string(),
        system_prompt: "strict".to_string(),
        allowed_tools: Some(vec!["read_file".to_string()]),
    };
    let report = sub
        .spawn_typed(
            &ty,
            SubagentBrief {
                description: "d".to_string(),
                prompt: "p".to_string(),
                system_prompt: None,
            },
        )
        .await
        .expect("child must still complete after a recoverable denial");
    assert_eq!(report.final_text, "gave up");
    assert_eq!(report.turns, 2);

    // Parent in-memory session bytes — unchanged.
    let after_mem: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(after_mem, before, "parent in-memory session mutated");

    // Parent on-disk session — reopen and compare.
    let reopened = SessionStore::open(&dir, "parent_typed").unwrap();
    let after_disk: Vec<String> = reopened
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(after_disk, before, "parent on-disk session mutated");

    // The child must not have leaked its transcript into any session
    // file the parent's workspace knows about. Enumerating the
    // session dir is the strongest check — if a new file appeared
    // with a name we did not pick, the test fails loudly.
    let session_dir = dir.join(".metis").join("sessions");
    if session_dir.exists() {
        let entries: Vec<String> = fs::read_dir(&session_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // Exactly one file — the parent's. Child left nothing.
        assert_eq!(
            entries,
            vec!["parent_typed.jsonl".to_string()],
            "child leaked a session file: {entries:?}"
        );
    }
}

// ============================================================================
// Session 32 — parallel subagent execution
//
// Six tests pin the `Subagent::spawn_parallel` contract — the first real
// multi-threaded surface in metis-core. Each test uses a content-addressed
// [`KeyedProvider`] so thread interleaving cannot confuse the assertions:
// every request carries a distinctive substring and looks up its own
// response, regardless of which thread's `chat()` call lands first.
//
//   * invariant: results come back in the same order as the input briefs
//                even when the provider sees them in arbitrary order
//   * invariant: transcripts are isolated — a worker's request must NOT
//                contain any other worker's prompt, final text, or system
//                prompt fragment
//   * invariant: per-brief usage aggregates add up cleanly (no double
//                counting, no stolen tokens across workers)
//   * invariant: a parallel batch does NOT touch the parent agent's
//                session store, in-memory or on-disk (byte-for-byte)
//   * edge:      an empty `briefs` vector is a noop — zero threads
//                spawned, zero provider calls, and a follow-up non-empty
//                call still works (3-transition state machine)
//   * edge:      a single brief's failure (NoChoices) lands in its own
//                slot as `Err(...)` and does NOT cancel the siblings —
//                the other two slots are still `Ok(...)` with correct
//                per-brief state
// ============================================================================

#[tokio::test]
async fn invariant_spawn_parallel_preserves_input_order() {
    let dir = tempdir();
    let provider = KeyedProvider::new(vec![
        (
            "alpha-task",
            assistant_final_with_usage("alpha-answer", usage(11, 3, 0, 0)),
        ),
        (
            "beta-task",
            assistant_final_with_usage("beta-answer", usage(13, 4, 0, 0)),
        ),
        (
            "gamma-task",
            assistant_final_with_usage("gamma-answer", usage(17, 5, 0, 0)),
        ),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        max_turns: 2,
        ..AgentConfig::default()
    };

    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);
    let briefs = vec![
        SubagentBrief {
            description: "first".to_string(),
            prompt: "alpha-task".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "second".to_string(),
            prompt: "beta-task".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "third".to_string(),
            prompt: "gamma-task".to_string(),
            system_prompt: None,
        },
    ];

    let results = sub.spawn_parallel(briefs).await;
    assert_eq!(results.len(), 3, "one result per brief");

    // Transition 1: every slot must be Ok.
    let reports: Vec<_> = results
        .into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or_else(|e| panic!("brief {i} errored: {e:?}")))
        .collect();

    // Transition 2: input index → description (input was "first"/"second"/"third").
    assert_eq!(reports[0].description, "first");
    assert_eq!(reports[1].description, "second");
    assert_eq!(reports[2].description, "third");

    // Transition 3: input index → final text (via keyed provider mapping).
    assert_eq!(reports[0].final_text, "alpha-answer");
    assert_eq!(reports[1].final_text, "beta-answer");
    assert_eq!(reports[2].final_text, "gamma-answer");

    // Each worker hit the provider exactly once — three total calls.
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn invariant_spawn_parallel_isolates_transcripts() {
    let dir = tempdir();
    let provider = KeyedProvider::new(vec![
        (
            "red-prompt",
            assistant_final_with_usage("red-answer", usage(5, 2, 0, 0)),
        ),
        (
            "blue-prompt",
            assistant_final_with_usage("blue-answer", usage(6, 2, 0, 0)),
        ),
        (
            "green-prompt",
            assistant_final_with_usage("green-answer", usage(7, 2, 0, 0)),
        ),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let config = AgentConfig {
        system_prompt: Some("shared-system".to_string()),
        max_turns: 2,
        ..AgentConfig::default()
    };
    let sub = Subagent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    let briefs = vec![
        SubagentBrief {
            description: "r".to_string(),
            prompt: "red-prompt".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "b".to_string(),
            prompt: "blue-prompt".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "g".to_string(),
            prompt: "green-prompt".to_string(),
            system_prompt: None,
        },
    ];

    let results = sub.spawn_parallel(briefs).await;
    assert!(results.iter().all(Result::is_ok));

    // Walk every request the provider saw; group by which prompt it carried.
    // Each request must carry exactly ONE of the three prompts, and must NOT
    // carry any fragment of another worker's prompt or final text.
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    let colours = ["red-prompt", "blue-prompt", "green-prompt"];
    let answers = ["red-answer", "blue-answer", "green-answer"];
    for req in &reqs {
        let joined: String = req
            .messages
            .iter()
            .filter_map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("||");
        let hits: Vec<&&str> = colours.iter().filter(|c| joined.contains(**c)).collect();
        assert_eq!(
            hits.len(),
            1,
            "request must carry exactly one worker's prompt, joined={joined:?}"
        );
        // A worker must never see another worker's final text leaked in as
        // if it were an assistant turn of its own transcript.
        for a in &answers {
            assert!(
                !joined.contains(a),
                "worker saw another worker's final text in its transcript: {joined:?}"
            );
        }
    }
}

#[tokio::test]
async fn invariant_spawn_parallel_aggregate_usage_matches_sum() {
    let dir = tempdir();
    // Distinct token counts per brief so any accidental aggregation is
    // trivially catchable by arithmetic.
    let provider = KeyedProvider::new(vec![
        ("tok-a", assistant_final_with_usage("a", usage(10, 3, 0, 0))),
        ("tok-b", assistant_final_with_usage("b", usage(20, 5, 0, 0))),
        ("tok-c", assistant_final_with_usage("c", usage(40, 7, 0, 0))),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    );

    let briefs = vec![
        SubagentBrief {
            description: "a".to_string(),
            prompt: "tok-a".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "b".to_string(),
            prompt: "tok-b".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "c".to_string(),
            prompt: "tok-c".to_string(),
            system_prompt: None,
        },
    ];
    let results = sub.spawn_parallel(briefs).await;
    let reports: Vec<_> = results.into_iter().map(Result::unwrap).collect();

    // Per-brief pin: each report carries exactly the tokens KeyedProvider
    // returned for *that* prompt — no cross-pollination.
    assert_eq!(reports[0].usage.input_tokens, 10);
    assert_eq!(reports[0].usage.output_tokens, 3);
    assert_eq!(reports[1].usage.input_tokens, 20);
    assert_eq!(reports[1].usage.output_tokens, 5);
    assert_eq!(reports[2].usage.input_tokens, 40);
    assert_eq!(reports[2].usage.output_tokens, 7);

    // Aggregate pin: summing the per-brief usage yields the exact token
    // totals the provider handed out — no worker stole another's tokens,
    // no worker dropped any.
    let sum_in: u32 = reports.iter().map(|r| r.usage.input_tokens).sum();
    let sum_out: u32 = reports.iter().map(|r| r.usage.output_tokens).sum();
    assert_eq!(sum_in, 70);
    assert_eq!(sum_out, 15);

    // Each worker took exactly one turn.
    for r in &reports {
        assert_eq!(r.turns, 1);
    }
}

#[tokio::test]
async fn invariant_spawn_parallel_does_not_touch_parent_session() {
    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    // Parent completes one turn and persists to its own session first.
    let parent_provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "parent ok",
        usage(3, 2, 0, 0),
    )]);
    let parent_session = SessionStore::open(&dir, "parent_par").unwrap();
    let mut parent = Agent::new(
        &parent_provider as &dyn ChatProvider,
        &registry,
        ctx.clone(),
        AgentConfig {
            system_prompt: Some("psys".to_string()),
            max_turns: 2,
            ..AgentConfig::default()
        },
    )
    .with_session(parent_session);
    parent.run("parent prompt").await.unwrap();

    let before: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert!(!before.is_empty());

    // Child batch: three parallel workers on the same workspace.
    let child_provider = KeyedProvider::new(vec![
        ("p1", assistant_final_with_usage("r1", usage(2, 1, 0, 0))),
        ("p2", assistant_final_with_usage("r2", usage(2, 1, 0, 0))),
        ("p3", assistant_final_with_usage("r3", usage(2, 1, 0, 0))),
    ]);
    let sub = Subagent::new(
        &child_provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    );
    let briefs = vec![
        SubagentBrief {
            description: "d1".to_string(),
            prompt: "p1".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "d2".to_string(),
            prompt: "p2".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "d3".to_string(),
            prompt: "p3".to_string(),
            system_prompt: None,
        },
    ];
    let results = sub.spawn_parallel(briefs).await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(Result::is_ok));

    // In-memory: unchanged.
    let after_mem: Vec<String> = parent
        .session()
        .unwrap()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(after_mem, before, "parent in-memory session mutated");

    // On-disk: unchanged.
    let reopened = SessionStore::open(&dir, "parent_par").unwrap();
    let after_disk: Vec<String> = reopened
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(after_disk, before, "parent on-disk session mutated");

    // Workers must not have created their own session files.
    let session_dir = dir.join(".metis").join("sessions");
    if session_dir.exists() {
        let entries: Vec<String> = fs::read_dir(&session_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["parent_par.jsonl".to_string()],
            "child workers leaked session files: {entries:?}"
        );
    }
}

#[tokio::test]
async fn edge_spawn_parallel_empty_briefs_is_noop() {
    let dir = tempdir();
    // Non-empty queue: if the empty path accidentally calls the provider
    // the test will still pass a response, but the seen count will be
    // non-zero and the assertion will catch it.
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "unused",
        usage(1, 1, 0, 0),
    )]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    );

    // Transition 1: empty input → empty output, no panic, no thread spawn.
    let empty = sub.spawn_parallel(Vec::new()).await;
    assert!(empty.is_empty());

    // Transition 2: provider was never called.
    assert_eq!(
        provider.requests().len(),
        0,
        "empty spawn_parallel must not touch the provider"
    );

    // Transition 3: a subsequent non-empty call on the SAME subagent
    // still works — the empty path did not poison any internal state.
    let report = sub
        .spawn(SubagentBrief {
            description: "after".to_string(),
            prompt: "anything".to_string(),
            system_prompt: None,
        })
        .await
        .expect("post-empty spawn must succeed");
    assert_eq!(report.final_text, "unused");
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn edge_spawn_parallel_one_failure_preserves_sibling_results() {
    let dir = tempdir();
    // Middle brief gets a response with empty `choices`, which the agent
    // loop surfaces as `AgentError::NoChoices`. Outer two get valid
    // finals. Tests that (a) a sibling's error does NOT cancel peers,
    // (b) the error lands at the correct index, (c) peers' state is
    // still intact and correct.
    let provider = KeyedProvider::new(vec![
        (
            "ok-first",
            assistant_final_with_usage("first-done", usage(4, 2, 0, 0)),
        ),
        (
            "crash-middle",
            ChatResponse {
                choices: vec![],
                usage: None,
            },
        ),
        (
            "ok-last",
            assistant_final_with_usage("last-done", usage(6, 3, 0, 0)),
        ),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let sub = Subagent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        },
    );

    let briefs = vec![
        SubagentBrief {
            description: "A".to_string(),
            prompt: "ok-first".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "B".to_string(),
            prompt: "crash-middle".to_string(),
            system_prompt: None,
        },
        SubagentBrief {
            description: "C".to_string(),
            prompt: "ok-last".to_string(),
            system_prompt: None,
        },
    ];
    let results = sub.spawn_parallel(briefs).await;
    assert_eq!(results.len(), 3);

    // Index 0: Ok — first-done, preserved despite sibling crash.
    let r0 = results[0].as_ref().expect("sibling A must survive");
    assert_eq!(r0.final_text, "first-done");
    assert_eq!(r0.description, "A");
    assert_eq!(r0.usage.input_tokens, 4);

    // Index 1: Err — the NoChoices variant, reported at the correct slot.
    let e1 = results[1]
        .as_ref()
        .expect_err("crashing brief must surface as Err");
    assert!(
        matches!(e1, AgentError::NoChoices),
        "expected NoChoices, got {e1:?}"
    );

    // Index 2: Ok — last-done, preserved despite sibling crash.
    let r2 = results[2].as_ref().expect("sibling C must survive");
    assert_eq!(r2.final_text, "last-done");
    assert_eq!(r2.description, "C");
    assert_eq!(r2.usage.input_tokens, 6);

    // The provider must have been hit exactly three times — no retries,
    // no cancellations; every worker ran to its natural terminus.
    assert_eq!(provider.requests().len(), 3);
}

// ============================================================================
// Session 33 — Memory store invariants + edge cases
// ============================================================================

use aegis_core::{
    format_memory_file, parse_memory_file, MemoryEntry, MemoryMeta, MemoryStore, MemoryType,
};

fn memory_entry(filename: &str, name: &str, mt: MemoryType, body: &str) -> MemoryEntry {
    MemoryEntry {
        meta: MemoryMeta {
            name: name.to_string(),
            description: format!("desc for {name}"),
            memory_type: mt,
        },
        body: body.to_string(),
        filename: filename.to_string(),
    }
}

/// **Invariant (Session 33)**: save → read → delete → read-again forms a
/// clean three-transition lifecycle: (1) save succeeds and the file
/// appears on disk, (2) read returns the same content, (3) delete removes
/// both the file and the index entry, and a subsequent read returns
/// NotFound.
#[tokio::test]
async fn invariant_memory_save_read_delete_lifecycle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(tmp.path()).unwrap();

    let entry = memory_entry(
        "lifecycle.md",
        "Lifecycle test",
        MemoryType::User,
        "body text\n",
    );

    // Transition 1: save
    let path = store.save(&entry).unwrap();
    assert!(path.exists(), "file must exist after save");
    let idx = store.read_index().unwrap();
    assert!(idx.contains("(lifecycle.md)"), "index must reference file");

    // Transition 2: read returns same content
    let loaded = store.read("lifecycle.md").unwrap();
    assert_eq!(loaded.meta.name, "Lifecycle test");
    assert_eq!(loaded.meta.memory_type, MemoryType::User);
    assert_eq!(loaded.body.trim(), "body text");

    // Transition 3: delete removes everything
    store.delete("lifecycle.md").unwrap();
    assert!(!path.exists(), "file must be gone after delete");
    let idx = store.read_index().unwrap();
    assert!(
        !idx.contains("lifecycle.md"),
        "index must not reference deleted file"
    );
    assert!(
        matches!(
            store.read("lifecycle.md"),
            Err(aegis_core::MemoryError::NotFound(_))
        ),
        "read after delete must return NotFound"
    );
}

/// **Invariant (Session 33)**: format → parse round-trip preserves all
/// four memory types, arbitrary body content, and multi-line bodies
/// with special characters.
#[tokio::test]
async fn invariant_memory_frontmatter_round_trip_all_types() {
    for mt in [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ] {
        let meta = MemoryMeta {
            name: format!("test-{mt}"),
            description: format!("desc for {mt}"),
            memory_type: mt,
        };
        let body = "Line 1\n**Why:** reason\n**How to apply:** guidance\nSpecial: $100 → €90\n";
        let rendered = format_memory_file(&meta, body);
        let parsed = parse_memory_file(&rendered, "test.md").unwrap();
        assert_eq!(parsed.meta, meta, "meta mismatch for type {mt}");
        assert_eq!(
            parsed.body.trim(),
            body.trim(),
            "body mismatch for type {mt}"
        );
    }
}

/// **Edge (Session 33)**: saving two entries, then deleting one, must
/// leave the other's index entry intact and the surviving file readable.
/// A buggy index rewrite that truncated on delete would break this.
#[tokio::test]
async fn edge_memory_delete_preserves_sibling_index_entries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(tmp.path()).unwrap();

    let a = memory_entry("a.md", "Alpha", MemoryType::User, "aaa\n");
    let b = memory_entry("b.md", "Beta", MemoryType::Feedback, "bbb\n");
    store.save(&a).unwrap();
    store.save(&b).unwrap();

    // Delete Alpha
    store.delete("a.md").unwrap();

    // Beta must survive in both index and on disk
    let idx = store.read_index().unwrap();
    assert!(idx.contains("(b.md)"), "Beta must survive in index");
    assert!(!idx.contains("(a.md)"), "Alpha must be gone from index");
    let loaded = store.read("b.md").unwrap();
    assert_eq!(loaded.meta.name, "Beta");
}

/// **Edge (Session 33)**: updating a memory entry three times must leave
/// exactly one index line, and the final content must be the third
/// version. A buggy upsert that appended instead of replacing would
/// duplicate lines.
#[tokio::test]
async fn edge_memory_triple_update_single_index_line() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(tmp.path()).unwrap();

    let mut e = memory_entry("evolving.md", "V1", MemoryType::Project, "first\n");
    store.save(&e).unwrap();

    e.meta.name = "V2".to_string();
    e.body = "second\n".to_string();
    store.update(&e).unwrap();

    e.meta.name = "V3".to_string();
    e.body = "third\n".to_string();
    store.update(&e).unwrap();

    // Index: exactly one line for this file, showing V3
    let idx = store.read_index().unwrap();
    let matching: Vec<&str> = idx
        .lines()
        .filter(|l| l.contains("(evolving.md)"))
        .collect();
    assert_eq!(matching.len(), 1, "must have exactly one index entry");
    assert!(matching[0].contains("V3"), "index must show latest name");

    // File content: V3
    let loaded = store.read("evolving.md").unwrap();
    assert_eq!(loaded.meta.name, "V3");
    assert_eq!(loaded.body.trim(), "third");
}

/// **Edge (Session 33)**: list on a directory with a mix of valid,
/// invalid, and non-markdown files must return only the valid .md
/// entries, skip MEMORY.md, and survive without error.
#[tokio::test]
async fn edge_memory_list_mixed_content_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(tmp.path()).unwrap();

    // One valid entry via store
    store
        .save(&memory_entry("valid.md", "Good", MemoryType::User, "ok\n"))
        .unwrap();

    // Malformed .md file
    fs::write(store.dir().join("broken.md"), "no frontmatter at all").unwrap();
    // Non-markdown file (should be ignored)
    fs::write(store.dir().join("notes.txt"), "plain text").unwrap();
    // MEMORY.md itself (should be ignored by list)
    // (already created by save's index update)

    let entries = store.list().unwrap();
    assert_eq!(entries.len(), 1, "only the valid entry should appear");
    assert_eq!(entries[0].filename, "valid.md");
}

// ============================================================================
// Session 34 — Memory tools (save/list/read/delete via Tool trait)
// ============================================================================

use aegis_core::Tool;

/// **Invariant (Session 34)**: the save_memory tool creates a file with
/// correct frontmatter, the list_memories tool shows it in the index,
/// read_memory returns the content, and delete_memory removes it.
/// Full 4-transition lifecycle through the Tool interface.
#[tokio::test]
async fn invariant_memory_tools_full_crud_lifecycle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let ctx = ToolContext::new(workspace);

    let save = aegis_core::tools::SaveMemory;
    let list = aegis_core::tools::ListMemories;
    let read = aegis_core::tools::ReadMemory;
    let delete = aegis_core::tools::DeleteMemory;

    // 1. Save
    let result = save
        .execute(
            serde_json::json!({
                "filename": "user_role.md",
                "name": "User role",
                "description": "Who the user is",
                "type": "user",
                "body": "Senior Rust developer."
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.contains("saved"), "save result: {result}");

    // 2. List — index should contain the entry
    let result = list.execute(serde_json::json!({}), &ctx).await.unwrap();
    assert!(
        result.contains("user_role.md"),
        "list must show the saved file"
    );
    assert!(result.contains("User role"), "list must show the title");

    // 3. Read — content should round-trip
    let result = read
        .execute(serde_json::json!({"filename": "user_role.md"}), &ctx)
        .await
        .unwrap();
    assert!(result.contains("name: User role"));
    assert!(result.contains("type: user"));
    assert!(result.contains("Senior Rust developer"));

    // 4. Delete — file and index entry gone
    let result = delete
        .execute(serde_json::json!({"filename": "user_role.md"}), &ctx)
        .await
        .unwrap();
    assert!(result.contains("deleted"), "delete result: {result}");

    // List after delete should be empty
    let result = list.execute(serde_json::json!({}), &ctx).await.unwrap();
    assert_eq!(result, "No memories stored yet.");
}

/// **Edge (Session 34)**: save_memory on an existing filename should
/// update rather than error. The upsert behavior is critical for the
/// model — it shouldn't have to check existence before saving.
#[tokio::test]
async fn edge_memory_save_tool_upserts_on_existing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ctx = ToolContext::new(tmp.path().to_path_buf());
    let save = aegis_core::tools::SaveMemory;
    let read = aegis_core::tools::ReadMemory;

    // First save
    save.execute(
        serde_json::json!({
            "filename": "note.md",
            "name": "V1",
            "description": "first version",
            "type": "project",
            "body": "original"
        }),
        &ctx,
    )
    .await
    .unwrap();

    // Second save — same filename, different content
    let result = save
        .execute(
            serde_json::json!({
                "filename": "note.md",
                "name": "V2",
                "description": "second version",
                "type": "project",
                "body": "updated"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        result.contains("updated"),
        "second save must report update: {result}"
    );

    // Read should show V2
    let content = read
        .execute(serde_json::json!({"filename": "note.md"}), &ctx)
        .await
        .unwrap();
    assert!(content.contains("name: V2"));
    assert!(content.contains("updated"));
    assert!(!content.contains("original"));
}

// ============================================================================
// Session 38 — overthink / extended thinking
// ============================================================================

/// `Agent::take_session` extracts the session, leaving None behind,
/// so the REPL can transfer it to a rebuilt agent on `/overthink`.
#[tokio::test]
async fn edge_take_session_extracts_and_leaves_none() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(dir.clone());
    let session = SessionStore::open(&dir, "take-test").unwrap();
    let mut agent = Agent::new(&provider, &registry, ctx, AgentConfig::default());
    // Before attaching: take_session returns None.
    assert!(agent.take_session().is_none());
    // Attach then take: should get Some, then None.
    agent = agent.with_session(session);
    assert!(agent.session().is_some());
    let taken = agent.take_session();
    assert!(taken.is_some());
    assert!(agent.session().is_none());
}

/// When `AgentConfig.thinking` is true, the ChatRequest sent to the
/// provider must carry `thinking: true` and the configured budget.
#[tokio::test]
async fn invariant_thinking_config_propagates_to_chat_request() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "thought about it",
        Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            ..Default::default()
        },
    )]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(dir.clone());
    let config = AgentConfig {
        thinking: true,
        thinking_budget: 8192,
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(&provider, &registry, ctx, config);
    let _out = agent.run("think hard".to_string()).await.unwrap();
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].thinking, "request must carry thinking=true");
    assert_eq!(reqs[0].thinking_budget, 8192);
}

/// When `AgentConfig.thinking` is false (default), the request must
/// NOT carry thinking=true.
#[tokio::test]
async fn invariant_thinking_disabled_by_default() {
    let dir = tempdir();
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage(
        "no thinking",
        Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            ..Default::default()
        },
    )]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(dir.clone());
    let mut agent = Agent::new(&provider, &registry, ctx, AgentConfig::default());
    let _out = agent.run("hello".to_string()).await.unwrap();
    let reqs = provider.requests();
    assert!(!reqs[0].thinking);
    // Budget carries the default value but is ignored when thinking=false.
    assert_eq!(
        reqs[0].thinking_budget,
        AgentConfig::default().thinking_budget
    );
}

// Session 45 — hooks fire at correct agent loop points

#[tokio::test]
async fn hooks_session_start_injects_into_transcript() {
    let dir = tempdir();
    let metis_dir = dir.join(".metis");
    std::fs::create_dir_all(&metis_dir).unwrap();
    std::fs::write(
        metis_dir.join("hooks.toml"),
        "[[session_start]]\ncommand = \"echo 'session started'\"\non_fail = \"warn\"\n",
    )
    .unwrap();

    let hooks = aegis_core::load_hooks(&dir);
    let u = Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("done", u)]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(dir.clone()).with_hooks(hooks);
    let config = AgentConfig {
        system_prompt: Some("test".into()),
        ..Default::default()
    };
    let mut agent = Agent::new(&provider, &registry, ctx, config);
    let out = agent.run("hi").await.unwrap();
    let has_hook_msg = out.transcript.iter().any(|m| {
        m.role == Role::System
            && m.content
                .as_deref()
                .unwrap_or("")
                .contains("session started")
    });
    assert!(
        has_hook_msg,
        "session_start hook output should be in transcript"
    );
}

#[tokio::test]
async fn hooks_pre_tool_use_can_block_tool() {
    let dir = tempdir();
    let metis_dir = dir.join(".metis");
    std::fs::create_dir_all(&metis_dir).unwrap();
    std::fs::write(
        metis_dir.join("hooks.toml"),
        "[[pre_tool_use]]\ncommand = \"echo 'blocked' >&2; exit 1\"\non_fail = \"block\"\n",
    )
    .unwrap();

    let hooks = aegis_core::load_hooks(&dir);
    let u = Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let provider = ScriptedProvider::new(vec![
        assistant_calling_with_usage("read_file", r#"{"path":"x.txt"}"#, "c1", u),
        assistant_final_with_usage("done", u),
    ]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(dir.clone()).with_hooks(hooks);
    let mut agent = Agent::new(&provider, &registry, ctx, AgentConfig::default());
    let out = agent.run("test").await.unwrap();
    let has_block = out.transcript.iter().any(|m| {
        m.role == Role::Tool
            && m.content
                .as_deref()
                .unwrap_or("")
                .contains("pre-tool-use hook blocked")
    });
    assert!(
        has_block,
        "blocked tool call should have error in transcript"
    );
}

#[tokio::test]
async fn hooks_empty_config_does_not_affect_agent() {
    let dir = tempdir();
    // Override HOME so user-level ~/.metis/hooks.toml is not picked up on
    // machines that have one installed (e.g. the CI runner).
    std::env::set_var("HOME", &dir);
    let hooks = aegis_core::load_hooks(&dir);
    std::env::remove_var("HOME");
    assert!(hooks.is_empty());
    let u = Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let provider = ScriptedProvider::new(vec![assistant_final_with_usage("ok", u)]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(dir.clone()).with_hooks(hooks);
    let mut agent = Agent::new(&provider, &registry, ctx, AgentConfig::default());
    let out = agent.run("hi").await.unwrap();
    assert_eq!(out.final_text, "ok");
}

// ============================================================================
// Session 58 — SubagentType::explore / plan / by_name + Agent tool wiring
// ============================================================================

#[tokio::test]
async fn subagent_type_by_name_resolves_known_types() {
    let gp = SubagentType::by_name("general-purpose");
    assert!(gp.is_some());
    assert_eq!(gp.unwrap().name, "general-purpose");

    let explore = SubagentType::by_name("explore");
    assert!(explore.is_some());
    let explore = explore.unwrap();
    assert_eq!(explore.name, "explore");
    assert!(
        explore.allowed_tools.is_some(),
        "explore must have an allowlist"
    );
    let tools = explore.allowed_tools.as_ref().unwrap();
    assert!(tools.contains(&"read_file".to_string()));
    assert!(tools.contains(&"grep".to_string()));
    assert!(tools.contains(&"glob".to_string()));
    assert!(
        !tools.contains(&"bash".to_string()),
        "explore must not allow bash"
    );
    assert!(
        !tools.contains(&"edit_file".to_string()),
        "explore must not allow edit"
    );

    let plan = SubagentType::by_name("plan");
    assert!(plan.is_some());
    let plan = plan.unwrap();
    assert_eq!(plan.name, "plan");
    assert!(plan.allowed_tools.is_some(), "plan must have an allowlist");
    let tools = plan.allowed_tools.as_ref().unwrap();
    assert!(
        !tools.contains(&"bash".to_string()),
        "plan must not allow bash"
    );
    assert!(
        !tools.contains(&"write_file".to_string()),
        "plan must not allow write"
    );

    assert!(SubagentType::by_name("nonexistent").is_none());
}

#[tokio::test]
async fn subagent_explore_type_denies_mutating_tools() {
    let dir = tempdir();
    let u = Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    // Scripted: model calls bash, gets denied, then returns final text.
    let provider = ScriptedProvider::new(vec![
        // Turn 1: model tries to call bash
        ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: Role::Assistant,
                    content: None,
                    content_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".to_string(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: "bash".to_string(),
                            arguments: r#"{"command":"echo hi"}"#.to_string(),
                        },
                    }],
                    tool_call_id: None,
                    name: None,
                    protected: false,
                    reasoning_content: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: Some(u),
        },
        // Turn 2: model gives up and returns text
        assistant_final_with_usage("denied as expected", u),
    ]);

    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(dir.clone());
    let ty = SubagentType::explore();
    let spawner = Subagent::new(&provider, &registry, ctx, AgentConfig::default());
    let brief = SubagentBrief {
        description: "test explore".to_string(),
        prompt: "try bash".to_string(),
        system_prompt: None,
    };
    let report = spawner.spawn_typed(&ty, brief).await.unwrap();
    assert_eq!(report.final_text, "denied as expected");

    // Verify the tool error was a denial, not execution
    let reqs = provider.requests();
    assert!(reqs.len() >= 2, "should have at least 2 turns");
    let second_req = &reqs[1];
    let tool_msg = second_req.messages.iter().find(|m| m.role == Role::Tool);
    assert!(tool_msg.is_some(), "should have a tool result message");
    let content = tool_msg.unwrap().content.as_deref().unwrap_or("");
    assert!(
        content.contains("not in this subagent's allowlist"),
        "tool result should contain allowlist denial, got: {content}"
    );
}

#[tokio::test]
async fn agent_tool_with_spawner_callback() {
    use aegis_core::AgentSpawnRequest;

    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();

    // Set up a simple spawner that echoes the request
    let spawner: aegis_core::AgentSpawnerFn = std::sync::Arc::new(|req: AgentSpawnRequest| {
        Ok(format!(
            "type={} desc={} model={}",
            req.subagent_type.unwrap_or_else(|| "none".into()),
            req.description,
            req.model.unwrap_or_else(|| "default".into()),
        ))
    });
    let ctx = ToolContext::new(dir.clone()).with_agent_spawner(spawner);

    // Find the agent tool and execute it directly
    let agent_tool = registry.get("agent").expect("agent tool must exist");
    let result = agent_tool
        .execute(
            serde_json::json!({
                "description": "test task",
                "prompt": "do something",
                "subagent_type": "explore",
                "model": "haiku"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.contains("type=explore"));
    assert!(result.contains("desc=test task"));
    assert!(result.contains("model=haiku"));
}

#[tokio::test]
async fn agent_tool_background_returns_immediately() {
    use aegis_core::{AgentSpawnRequest, BackgroundAgents, BackgroundResult};

    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let bg = BackgroundAgents::new();

    let bg_clone = bg.clone();
    let spawner: aegis_core::AgentSpawnerFn = std::sync::Arc::new(move |req: AgentSpawnRequest| {
        if req.run_in_background {
            let bg = bg_clone.clone();
            let desc = req.description.clone();
            bg.inc_pending();
            // Simulate background completion
            bg.push_completed(BackgroundResult {
                description: desc.clone(),
                result: Ok("background done".to_string()),
            });
            Ok(format!("Agent \"{}\" started in background.", desc))
        } else {
            Ok("foreground done".to_string())
        }
    });
    let mut ctx = ToolContext::new(dir.clone()).with_agent_spawner(spawner);
    ctx.background_agents = bg.clone();

    let agent_tool = registry.get("agent").expect("agent tool must exist");

    // Background call should return immediately
    let result = agent_tool
        .execute(
            serde_json::json!({
                "description": "bg task",
                "prompt": "do something",
                "run_in_background": true
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.contains("started in background"));

    // Background result should be drainable
    let completed = bg.drain_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].description, "bg task");
    assert_eq!(completed[0].result.as_ref().unwrap(), "background done");

    // Second drain should be empty
    assert!(bg.drain_completed().is_empty());
}

#[tokio::test]
async fn agent_tool_without_spawner_returns_error() {
    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(dir);

    let agent_tool = registry.get("agent").expect("agent tool must exist");
    let result = agent_tool
        .execute(
            serde_json::json!({
                "description": "test",
                "prompt": "do something"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_err(), "agent tool without spawner should error");
}
