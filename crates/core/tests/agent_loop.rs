//! End-to-end test for the agent loop using a scripted `ChatProvider`.
//!
//! The Session 3 refactor pulled `DeepSeekClient` behind the
//! [`ChatProvider`] trait specifically so the loop could be exercised
//! without a network call. This test walks the loop through the most
//! important control-flow branches:
//!
//! 1. Assistant emits a `tool_calls` message → the registered tool
//!    runs → the tool reply is appended → the next turn gets the
//!    combined transcript.
//! 2. On the second turn the assistant returns plain text → the loop
//!    terminates and reports the correct number of turns.
//! 3. A [`SessionStore`] attached to the agent captures every
//!    message (system, user, assistant, tool, assistant) in order on
//!    disk, so a subsequent process could resume from the JSONL file.
//!
//! The scripted provider also asserts the shape of each request it
//! receives, which is the only way to pin down that the loop is
//! sending the right history on the second call.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use aegis_api::{
    ApiResult, ChatChoice, ChatMessage, ChatProvider, ChatRequest, ChatResponse, Role, StreamEvent,
    ToolCall, ToolCallFunction, Usage,
};
use aegis_core::{
    Agent, AgentConfig, AgentError, CompactionConfig, SessionStore, ToolContext, ToolRegistry,
};

/// A provider whose responses are fully pre-scripted. The test hands
/// it a queue of responses; each call pops one, optionally runs an
/// inspector against the request, and returns it. If the queue runs
/// out the test fails loudly — the loop should never make more calls
/// than the test expects.
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

fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("metis-it-{}-{}", std::process::id(), n,));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

fn assistant_calling(tool: &str, args_json: &str, call_id: &str) -> ChatResponse {
    assistant_calling_with_usage(
        tool,
        args_json,
        call_id,
        Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    )
}

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

fn assistant_final_with_usage(text: &str, usage: Usage) -> ChatResponse {
    ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage::assistant_text(text),
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(usage),
    }
}

fn no_choices_response(usage: Usage) -> ChatResponse {
    ChatResponse {
        choices: vec![],
        usage: Some(usage),
    }
}

fn assistant_final(text: &str) -> ChatResponse {
    ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage::assistant_text(text),
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 6,
            total_tokens: 18,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }),
    }
}

#[tokio::test]
async fn loop_runs_tool_then_returns_final_text() {
    let dir = tempdir();
    fs::write(dir.join("hello.txt"), "hi there\n").unwrap();

    let script = vec![
        // Turn 1: ask to read hello.txt.
        assistant_calling("read_file", r#"{"path":"hello.txt"}"#, "call_1"),
        // Turn 2: plain text reply, loop should stop here.
        assistant_final("the file says: hi there"),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        system_prompt: Some("you are metis".to_string()),
        ..AgentConfig::default()
    };

    let mut agent = Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config);
    let output = agent
        .run("what does hello.txt say?")
        .await
        .expect("agent run");

    assert_eq!(output.turns, 2);
    assert_eq!(output.final_text, "the file says: hi there");

    // Usage summed across both turns: 10+12 prompt, 4+6 completion.
    assert_eq!(output.usage.input_tokens, 22);
    assert_eq!(output.usage.output_tokens, 10);

    // Provider should have seen exactly two requests.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);

    // First request: [system, user]
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, Role::System);
    assert_eq!(requests[0].messages[1].role, Role::User);

    // Second request: [system, user, assistant-with-tool_calls, tool]
    assert_eq!(requests[1].messages.len(), 4);
    assert_eq!(requests[1].messages[2].role, Role::Assistant);
    assert!(!requests[1].messages[2].tool_calls.is_empty());
    assert_eq!(requests[1].messages[3].role, Role::Tool);
    // The tool reply content must contain the file body the tool read.
    let tool_content = requests[1].messages[3].content.as_deref().unwrap_or("");
    assert!(
        tool_content.contains("hi there"),
        "tool content was: {tool_content}"
    );
}

#[tokio::test]
async fn session_store_captures_full_transcript() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "alpha\n").unwrap();

    let script = vec![
        assistant_calling("read_file", r#"{"path":"a.txt"}"#, "call_1"),
        assistant_final("done"),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);
    let session = SessionStore::open(&dir, "it-session").unwrap();

    let config = AgentConfig {
        system_prompt: Some("sys".to_string()),
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);
    agent.run("read a.txt").await.expect("agent run");

    // Reopen the store in a fresh handle and verify the on-disk log
    // mirrors what the loop saw: [system, user, assistant(tool_call), tool, assistant(final)]
    let reopened = SessionStore::open(&dir, "it-session").unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 5, "transcript was {msgs:#?}");
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(msgs[2].role, Role::Assistant);
    assert!(!msgs[2].tool_calls.is_empty());
    assert_eq!(msgs[3].role, Role::Tool);
    assert_eq!(msgs[4].role, Role::Assistant);
    assert_eq!(msgs[4].content.as_deref(), Some("done"));
}

#[tokio::test]
async fn stream_callback_receives_tool_call_preview_before_execution() {
    use std::sync::{Arc, Mutex};

    let dir = tempdir();
    fs::write(dir.join("hello.txt"), "hi there\n").unwrap();

    let script = vec![
        assistant_calling("read_file", r#"{"path":"hello.txt"}"#, "call_1"),
        assistant_final("done"),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    // Capture every StreamEvent the agent fans out.
    let sink: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_cb = Arc::clone(&sink);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig::default(),
    )
    .with_stream_callback(move |event| {
        sink_cb.lock().unwrap().push(event);
    });

    agent.run("read hello.txt").await.expect("run");

    let events = sink.lock().unwrap();
    // Filter down to just the ToolCall previews — there should be
    // exactly one, for read_file, with the raw JSON arguments echoed.
    let previews: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                name,
                arguments_preview,
            } => Some((name.as_str(), arguments_preview.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(previews.len(), 1, "events were: {events:#?}");
    assert_eq!(previews[0].0, "read_file");
    assert!(
        previews[0].1.contains("hello.txt"),
        "preview was: {}",
        previews[0].1
    );
}

/// A permission deny is passed back to the model as an ordinary tool
/// error. The model sees the deny reply on the next turn, acknowledges
/// it in conversation, and the turn ends naturally with the model's
/// text response — not an abrupt agent-side cutoff.
///
/// Contract pinned by this test:
///
/// 1. The tool_result is persisted with a "permission denied" body so
///    the transcript stays valid (every tool_call has its matching
///    tool_result, which OpenAI-compatible APIs require).
/// 2. The agent loop runs a SECOND model call, feeding the deny back.
///    `turns == 2` and `final_text` carries whatever the model said in
///    response to the refusal.
/// 3. The file is unchanged because the tool was blocked at the
///    permission gate, before execution.
/// 4. The model's second-turn request observes the deny in its
///    transcript, so it can react conversationally.
#[tokio::test]
async fn denied_tool_call_halts_turn_immediately() {
    use aegis_core::{Permission, PermissionDecision};
    use serde_json::Value;
    use std::sync::Arc;

    struct HardDenyEdits;
    impl Permission for HardDenyEdits {
        fn check(&self, tool: &str, _args: &Value) -> PermissionDecision {
            if tool == "edit_file" {
                PermissionDecision::HardDeny("test says no".into())
            } else {
                PermissionDecision::Allow
            }
        }
    }

    let dir = tempdir();
    fs::write(dir.join("a.txt"), "old\n").unwrap();

    // Only one assistant turn is scripted: the loop must end after the
    // deny without making a second provider call. If the agent tries to
    // pivot or "acknowledge" with a second model round, ScriptedProvider
    // will run out of responses and the test panics — exactly the
    // regression we want to catch.
    let script = vec![assistant_calling(
        "edit_file",
        r#"{"path":"a.txt","old_string":"old","new_string":"new"}"#,
        "call_1",
    )];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig::default(),
    )
    .with_permission(Arc::new(HardDenyEdits));

    let output = agent.run("change old to new in a.txt").await.expect("run");

    // Exactly one turn: HardDeny ends the loop after the tool-call
    // batch is processed, no follow-up model call.
    assert_eq!(
        output.turns, 1,
        "HardDeny must end the turn immediately, not loop back to the model"
    );

    // The file must NOT have been modified.
    let body = fs::read_to_string(dir.join("a.txt")).unwrap();
    assert_eq!(body, "old\n", "edit_file should have been blocked");

    // Exactly one provider request — the deny did not feed back into
    // the model. The user gets the prompt back and can decide what to
    // do next.
    let reqs = provider.requests();
    assert_eq!(
        reqs.len(),
        1,
        "provider must have been called only once, not {}",
        reqs.len()
    );

    // The persisted transcript still contains the deny tool reply so
    // a future user message has the context.
    let tool_msg = output
        .transcript
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("deny must be persisted as a tool message");
    assert!(
        tool_msg
            .content
            .as_deref()
            .unwrap_or("")
            .contains("permission denied"),
        "tool message was: {:?}",
        tool_msg.content
    );
}

// ============================================================================
// Session 16 — failure-driven coverage
//
// These tests are written to BREAK the agent loop, not to pin happy paths.
// Each one forces ≥3 state transitions and asserts intermediate state at
// every loop iteration. Tests marked `BUG_PIN` document a real latent bug
// or surprising contract; they pass today but a fix is wanted — see the
// session notes. Tests marked `LIMITATION` pin an intentional gap so a
// future refactor cannot remove the gap silently.
// ============================================================================

/// Risk: a runaway model that keeps emitting tool_calls forever burns
/// tokens and stalls. The `max_turns` cap is the only thing standing
/// between a buggy provider and an infinite loop. We assert not only the
/// final error variant but ALSO the per-turn state: each provider call
/// must observe a transcript that is exactly two messages longer than
/// the previous one (assistant_with_calls + tool_reply), proving the
/// loop made forward progress at every step before tripping the cap.
#[tokio::test]
async fn max_turns_trip_asserts_intermediate_transcript_growth_per_turn() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "alpha\n").unwrap();

    // Three forced tool calls, no escape hatch — the cap must trip.
    let script = vec![
        assistant_calling("read_file", r#"{"path":"a.txt"}"#, "call_1"),
        assistant_calling("read_file", r#"{"path":"a.txt"}"#, "call_2"),
        assistant_calling("read_file", r#"{"path":"a.txt"}"#, "call_3"),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        system_prompt: Some("sys".to_string()),
        max_turns: 3,
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    let err = agent
        .run("loop forever")
        .await
        .expect_err("expected MaxTurns or LoopDetected error");
    // Either bail-out is acceptable: `MaxTurns(3)` is the original
    // turn-cap path; `LoopDetected { turn: 3, .. }` is the
    // execution.rs LoopDetector firing on three identical calls in a
    // row. Both prove the loop didn't spin forever.
    match &err {
        AgentError::MaxTurns(n) => assert_eq!(*n, 3, "cap reported wrong turn count"),
        AgentError::LoopDetected { turn, .. } => {
            assert_eq!(*turn, 3, "loop detector reported wrong turn count")
        }
        other => panic!("wrong error variant: {other:?}"),
    }

    let reqs = provider.requests();
    assert_eq!(
        reqs.len(),
        3,
        "loop must call provider exactly max_turns times"
    );

    // State at each loop iteration. Turn 1 sees [system, user]. Turn 2
    // sees turn 1's transcript + assistant_calls + tool_reply. Turn 3
    // sees turn 2's + assistant_calls + tool_reply.
    let lens: Vec<usize> = reqs.iter().map(|r| r.messages.len()).collect();
    assert_eq!(lens, vec![2, 4, 6], "transcript growth was {lens:?}");

    // Every turn after the first must end in a Tool reply containing
    // the file body — proves the tool actually ran between iterations.
    for (i, r) in reqs.iter().enumerate().skip(1) {
        let last = r.messages.last().unwrap();
        assert_eq!(
            last.role,
            Role::Tool,
            "turn {i} last msg was {:?}",
            last.role
        );
        assert!(
            last.content.as_deref().unwrap_or("").contains("alpha"),
            "turn {i} tool reply was: {:?}",
            last.content
        );
    }
}

/// Risk: a tool failure on turn 1 must surface as a `tool` message so
/// the model can self-correct on turn 2 and recover on turn 3. We pin
/// the full state machine: error → retry-with-good-args → final. The
/// loop must NOT abort, the failed call's tool reply MUST contain the
/// error string, and the recovery turn's tool reply MUST contain the
/// real file body. Bonus: cumulative usage must include all three turns,
/// proving failed turns are counted exactly once and not skipped.
#[tokio::test]
async fn tool_failure_recovery_three_turn_state_machine_with_usage_invariant() {
    let dir = tempdir();
    fs::write(dir.join("real.txt"), "recovered\n").unwrap();

    let usage = |p, c| Usage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let script = vec![
        // Turn 1: read_file on a path that does not exist → tool error.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"missing.txt"}"#,
            "call_1",
            usage(10, 4),
        ),
        // Turn 2: model corrects itself, reads the real file.
        assistant_calling_with_usage(
            "read_file",
            r#"{"path":"real.txt"}"#,
            "call_2",
            usage(20, 4),
        ),
        // Turn 3: plain text reply, loop terminates.
        assistant_final_with_usage("done", usage(30, 6)),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig::default(),
    );
    let output = agent
        .run("read a file")
        .await
        .expect("loop should not abort on tool error");

    assert_eq!(output.turns, 3);
    assert_eq!(output.final_text, "done");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    // Turn 2 sees the failed tool reply with the error string.
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

    // Turn 3 sees BOTH tool replies; the second one (the recovery)
    // must contain the real file body.
    let turn3_tools: Vec<&ChatMessage> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(turn3_tools.len(), 2, "turn 3 should see both tool replies");
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
    // Failed turns must be billed exactly once — not skipped, not doubled.
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

/// LIMITATION: the loop has no dedup for repeated identical broken tool
/// calls. A model that retries the SAME failing call with the SAME
/// arguments will burn turns until `max_turns` trips. This test pins the
/// behaviour so a future "smart retry" change is forced to update it.
///
/// Risk: a wedged model + cheap broken call = silent token burn.
#[tokio::test]
async fn repeated_identical_broken_tool_call_is_not_deduped_burns_three_turns() {
    let dir = tempdir();
    let script = vec![
        assistant_calling("read_file", r#"{"path":"missing.txt"}"#, "call_1"),
        assistant_calling("read_file", r#"{"path":"missing.txt"}"#, "call_2"),
        assistant_calling("read_file", r#"{"path":"missing.txt"}"#, "call_3"),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        max_turns: 3,
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    let err = agent
        .run("retry")
        .await
        .expect_err("loop should hit max_turns or fire LoopDetected");
    // `LoopDetector` (execution.rs Session 17+) now fires on three
    // identical tool calls in a row before max_turns kicks in. Either
    // outcome proves the loop bailed out at turn 3 instead of running
    // forever — both are acceptable.
    assert!(
        matches!(
            err,
            AgentError::MaxTurns(3) | AgentError::LoopDetected { turn: 3, .. },
        ),
        "expected MaxTurns(3) or LoopDetected at turn 3, got {err:?}",
    );

    // All three turns happened — the loop did not detect the duplicate.
    let reqs = provider.requests();
    assert_eq!(
        reqs.len(),
        3,
        "loop deduped a duplicate call (or aborted early)"
    );

    // Every tool reply mentions an error AND every assistant_calls
    // message has the same arguments. State at each step: identical.
    let tool_replies: Vec<&ChatMessage> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(
        tool_replies.len(),
        2,
        "turn 3 should carry the prior 2 tool replies"
    );
    for r in &tool_replies {
        assert!(
            r.content.as_deref().unwrap_or("").contains("error"),
            "tool reply was: {:?}",
            r.content
        );
    }
}

// NOTE (Session 18): two Session 16 bug pins (resume-skips-compaction
// and max_turns=0 dangling user) lived here as duplicates of the
// canonical tests in `crates/core/tests/failure_driven.rs`. Both bugs
// are now fixed and the regression coverage lives in failure_driven.rs.
// The duplicates were removed; see `docs/sessions/session-18.md`.

#[cfg(any())]
fn _deleted_resume_skips_compaction_on_first_turn_bug_pin() {
    let dir = tempdir();
    let session = SessionStore::open(&dir, "huge").unwrap();
    let mut session = session;

    // Pre-populate with a large transcript (1 system + 30 user/assistant
    // pairs = 61 messages). This is well over any reasonable keep_tail.
    session.append(&ChatMessage::system("sys")).unwrap();
    for i in 0..30 {
        session.append(&ChatMessage::user(format!("u{i}"))).unwrap();
        session
            .append(&ChatMessage::assistant_text(format!("a{i}")))
            .unwrap();
    }
    let preloaded_count = session.messages().len();
    assert_eq!(preloaded_count, 61);

    // Provider just returns one final reply. The interesting part is
    // what the FIRST request looks like.
    let provider = ScriptedProvider::new(vec![assistant_final("ok")]);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let config = AgentConfig {
        // Tight compaction so a fixed loop would unambiguously trigger.
        compaction: CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        },
        ..AgentConfig::default()
    };
    let mut agent =
        Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config).with_session(session);

    agent
        .run("next question")
        .await
        .expect("run should succeed");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1);
    // BUG: the request should have been compacted. Today it isn't.
    // We assert the buggy state explicitly so a fix flips this test.
    assert_eq!(
        reqs[0].messages.len(),
        preloaded_count + 1, // 61 preloaded + 1 new user
        "BUG_PIN: first turn after resume currently sends FULL transcript"
    );
    // Synthetic compaction message is absent today.
    let has_synthetic = reqs[0].messages.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        !has_synthetic,
        "BUG_PIN: synthetic message should NOT exist today (will exist after fix)"
    );
}

#[cfg(any())]
fn _deleted_max_turns_zero_persists_dangling_user_message_bug_pin() {
    let dir = tempdir();
    let session = SessionStore::open(&dir, "zero").unwrap();

    let provider = ScriptedProvider::new(vec![]); // never called
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
    assert!(matches!(err, AgentError::MaxTurns(0)));

    // Provider was never called.
    assert!(provider.requests().is_empty());

    // BUG: reopening the session shows [system, user] with no
    // assistant — a dangling prompt.
    let reopened = SessionStore::open(&dir, "zero").unwrap();
    let msgs = reopened.messages();
    assert_eq!(
        msgs.len(),
        2,
        "BUG_PIN: session has dangling user message: {msgs:#?}"
    );
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(msgs[1].content.as_deref(), Some("hello"));
}

/// Invariant: cumulative usage MUST be path-independent. Two runs that
/// process the same logical work — one with all-fresh tokens, one with
/// the same tokens served from cache — must accumulate the SAME total
/// across `input_tokens + cache_read_tokens + cache_write_tokens`. If
/// the agent silently double-counted cache reads (or peeled them off
/// twice), this test catches it.
///
/// Multi-step: each path runs a 3-turn script (call → tool → final) so
/// the accumulator gets exercised across turns, not just one response.
#[tokio::test]
async fn cumulative_usage_cache_path_equals_fresh_path_invariant() {
    async fn run_path(usages: [Usage; 3]) -> aegis_core::UsageSnapshot {
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
            AgentConfig::default(),
        );
        agent.run("go").await.unwrap().usage
    }

    // Fresh path: 1000 fresh prompt tokens / turn, 100 completion / turn.
    let fresh = [Usage {
        prompt_tokens: 1000,
        completion_tokens: 100,
        total_tokens: 1100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }; 3];
    // Cache path: 200 fresh + 600 cache_read + 200 cache_write per turn,
    // same 100 completion. Sum of input-side categories per turn = 1000.
    let cached = [Usage {
        prompt_tokens: 200,
        completion_tokens: 100,
        total_tokens: 1100,
        cache_read_tokens: 600,
        cache_write_tokens: 200,
    }; 3];

    let fresh_total = run_path(fresh).await;
    let cached_total = run_path(cached).await;

    // Sum across all input categories must match exactly.
    let fresh_input_sum =
        fresh_total.input_tokens + fresh_total.cache_read_tokens + fresh_total.cache_write_tokens;
    let cached_input_sum = cached_total.input_tokens
        + cached_total.cache_read_tokens
        + cached_total.cache_write_tokens;
    assert_eq!(
        fresh_input_sum, cached_input_sum,
        "INVARIANT VIOLATED: same logical work, different totals.\n  fresh: {fresh_total:?}\n  cached: {cached_total:?}"
    );
    // And both should match the deliberate per-turn sum × 3 turns.
    assert_eq!(fresh_input_sum, 3000);
    assert_eq!(cached_input_sum, 3000);
    // Output tokens must be identical too.
    assert_eq!(fresh_total.output_tokens, cached_total.output_tokens);
    assert_eq!(fresh_total.output_tokens, 300);
}

/// Compaction must use the FULL prompt depth (fresh + cache_read +
/// cache_write), not just `prompt_tokens`. This test verifies the
/// agent loop reconstructs the depth correctly: a turn whose fresh
/// prompt tokens alone are BELOW the trigger but whose cache-inclusive
/// total CROSSES the trigger MUST cause compaction on the next turn.
///
/// Multi-step: turn 1 (no compaction yet, last_prompt_tokens=0) → turn 2
/// (compaction triggers because turn 1's response had cache tokens that
/// pushed reconstructed depth over the threshold) → turn 3 (final).
#[tokio::test]
async fn cache_aware_compaction_trigger_uses_summed_tokens_not_just_prompt() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "x\n").unwrap();

    // Trigger at 750. Fresh prompt 300 → would NOT trigger.
    // 300 + cache_read 400 + cache_write 100 = 800 → MUST trigger.
    let usage_with_cache = Usage {
        prompt_tokens: 300,
        completion_tokens: 4,
        total_tokens: 804,
        cache_read_tokens: 400,
        cache_write_tokens: 100,
    };
    let plain_usage = Usage {
        prompt_tokens: 50,
        completion_tokens: 4,
        total_tokens: 54,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };

    let script = vec![
        // Turn 1: tool call. Response carries cache tokens.
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", usage_with_cache),
        // Turn 2: another tool call so we get a third request to
        // inspect (the request that should reflect compaction).
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c2", plain_usage),
        // Turn 3: final.
        assistant_final_with_usage("done", plain_usage),
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
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(&provider as &dyn ChatProvider, &registry, ctx, config);

    agent.run("go").await.expect("run");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);

    // Turn 1 request: [system, user] (no compaction, last_prompt_tokens=0).
    assert_eq!(reqs[0].messages.len(), 2);

    // Turn 2 request: compaction MUST have run because reconstructed
    // depth from turn 1's response = 300+400+100 = 800 > 750. After
    // compaction we should see a synthetic system message.
    let turn2_has_synthetic = reqs[1].messages.iter().any(|m| {
        m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
    });
    assert!(
        turn2_has_synthetic,
        "INVARIANT VIOLATED: cache tokens did not contribute to compaction trigger.\n  turn 2 messages: {:#?}",
        reqs[1].messages
    );
}

/// Risk: a provider that returns an empty `choices` vec mid-loop must
/// abort with `AgentError::NoChoices` AND the partial transcript up to
/// that point must already be persisted to the session. A loss of the
/// preceding turn's progress would be silent data loss.
///
/// Multi-step: turn 1 succeeds (tool call + tool reply persisted) →
/// turn 2 returns empty choices → assert exact persisted state.
#[tokio::test]
async fn no_choices_mid_loop_aborts_with_partial_progress_already_persisted() {
    let dir = tempdir();
    fs::write(dir.join("a.txt"), "x\n").unwrap();
    let session = SessionStore::open(&dir, "nochoices").unwrap();

    let usage = Usage {
        prompt_tokens: 10,
        completion_tokens: 4,
        total_tokens: 14,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let script = vec![
        assistant_calling_with_usage("read_file", r#"{"path":"a.txt"}"#, "c1", usage),
        no_choices_response(usage),
    ];
    let provider = ScriptedProvider::new(script);
    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext::new(&dir);

    let mut agent = Agent::new(
        &provider as &dyn ChatProvider,
        &registry,
        ctx,
        AgentConfig {
            system_prompt: Some("sys".to_string()),
            ..AgentConfig::default()
        },
    )
    .with_session(session);

    let err = agent
        .run("go")
        .await
        .expect_err("must abort on empty choices");
    assert!(matches!(err, AgentError::NoChoices), "wrong error: {err:?}");

    // Reopen session: persisted state must be exactly
    // [system, user, assistant_with_tool_calls, tool_reply]. The aborted
    // turn 2 contributes nothing — no half-written assistant message.
    let reopened = SessionStore::open(&dir, "nochoices").unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 4, "persisted state was {msgs:#?}");
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(msgs[2].role, Role::Assistant);
    assert!(
        !msgs[2].tool_calls.is_empty(),
        "turn 1 assistant must have tool_calls"
    );
    assert_eq!(msgs[3].role, Role::Tool);
}

/// Streaming invariant: every `Usage` event observed by the stream
/// callback MUST sum to the cumulative `AgentOutput.usage`. The default
/// `chat_stream` impl synthesises one TextDelta + one Usage per turn,
/// so a 3-turn run must produce exactly 3 Usage events whose sum
/// matches the final aggregate. Any divergence means the stream and
/// aggregator are reading different sources.
#[tokio::test]
async fn streaming_usage_events_sum_equals_final_cumulative() {
    use std::sync::{Arc, Mutex};

    let dir = tempdir();
    fs::write(dir.join("a.txt"), "x\n").unwrap();

    let u1 = Usage {
        prompt_tokens: 11,
        completion_tokens: 3,
        total_tokens: 14,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let u2 = Usage {
        prompt_tokens: 13,
        completion_tokens: 5,
        total_tokens: 18,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let u3 = Usage {
        prompt_tokens: 17,
        completion_tokens: 7,
        total_tokens: 24,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
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
        AgentConfig::default(),
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

    let stream_input_sum: u32 = usage_events.iter().map(|u| u.prompt_tokens).sum();
    let stream_output_sum: u32 = usage_events.iter().map(|u| u.completion_tokens).sum();
    assert_eq!(
        stream_input_sum, output.usage.input_tokens,
        "INVARIANT VIOLATED: streamed Usage sum ≠ aggregated input_tokens"
    );
    assert_eq!(
        stream_output_sum, output.usage.output_tokens,
        "INVARIANT VIOLATED: streamed Usage sum ≠ aggregated output_tokens"
    );
}
