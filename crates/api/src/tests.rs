use async_trait::async_trait;

use super::*;

/// Helper: build a single OpenAI SSE payload that carries the
/// given text in the `choices[0].delta.content` slot.
fn delta_payload(text: &str) -> String {
    format!(
        r#"{{"choices":[{{"delta":{{"content":{}}}}}]}}"#,
        serde_json::Value::String(text.to_string())
    )
}

#[test]
fn scan_for_repeat_quiet_on_short_content() {
    let mut acc = OpenAiStreamAccumulator::new();
    let mut events = Vec::new();
    let cont = acc
        .feed(&delta_payload("merhaba!"), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont, "short content must not trigger");
    assert!(!acc.repeat_canceled);
}

#[test]
fn scan_for_repeat_quiet_on_unique_paragraph() {
    let mut acc = OpenAiStreamAccumulator::new();
    let mut events = Vec::new();
    let prose = "Burası tamamen orijinal bir cümledir ve hiçbir kısmı kendini tekrar etmez. \
                 İkinci cümle de farklı kelimeler içeriyor ve onun da içinde tekrar yok. \
                 Üçüncü cümle de farklı şeyler söylüyor.";
    let cont = acc
        .feed(&delta_payload(prose), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont, "natural prose must not trigger");
    assert!(!acc.repeat_canceled);
}

#[test]
fn scan_for_repeat_catches_mid_stream_loop_with_unique_tail() {
    // The exact failure mode the user reported: a duplicated
    // fragment in the middle of the response followed by a short
    // unique tail. The previous suffix-anchored detector missed
    // this because the unique tail pushed the duplicate out of
    // the suffix window.
    let mut acc = OpenAiStreamAccumulator::new();
    // kill_on_repeat is true by default; this is explicit for clarity.
    acc.kill_on_repeat = true;
    let mut events: Vec<StreamEvent> = Vec::new();
    let body = "İyiyim, teşekkürler! Burada kod yazmaya, dosyaları düzenlemeye, hata ayıklamaya yardım etmek için hazırım. Üzerinde çalıştığın bir şe\
                Burada kod yazmaya, dosyaları düzenlemeye, hata ayıklamaya yardım etmek için hazırım. Üzerinde çalıştığın bir şey var mı?";
    let cont = acc
        .feed(&delta_payload(body), &mut |e| events.push(e))
        .expect("ok");
    assert!(!cont, "mid-stream duplicate must trigger cancel");
    assert!(acc.repeat_canceled);
    // Last event must be the truncation notice.
    let last = events.last().expect("at least one event");
    match last {
        StreamEvent::TextDelta(s) => assert!(
            s.contains("repeat loop"),
            "expected truncation notice, got: {s}"
        ),
        _ => panic!("expected trailing TextDelta notice, got {last:?}"),
    }
}

#[test]
fn scan_for_repeat_catches_back_to_back_phrase_across_chunks() {
    let mut acc = OpenAiStreamAccumulator::new();
    acc.kill_on_repeat = true;
    let mut events: Vec<StreamEvent> = Vec::new();
    // Phrase must be >= REPEAT_WINDOW_CHARS (60) so back-to-back copies
    // contain an overlapping window that matches itself.
    let phrase = "Haklısın, daha önceki 'nasıl yardımcı olabilirim' tarzı cevabım gereksizdi. ";
    assert!(phrase.chars().count() >= REPEAT_WINDOW_CHARS);
    let cont1 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont1, "first chunk must not trigger");
    let cont2 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(!cont2, "second chunk must trigger");
    // Third feed must stay canceled.
    let cont3 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(!cont3);
}

#[test]
fn scan_for_repeat_handles_multibyte_chars_without_panic() {
    let mut acc = OpenAiStreamAccumulator::new();
    acc.kill_on_repeat = true;
    let mut events = Vec::new();
    // Need at least 2 * REPEAT_WINDOW_CHARS chars for scan to engage.
    let s = "🙂".repeat(REPEAT_WINDOW_CHARS * 2 + 10);
    let cont = acc
        .feed(&delta_payload(&s), &mut |e| events.push(e))
        .expect("ok");
    // Identical chars trivially repeat — must trigger, must not panic.
    assert!(!cont);
    assert!(acc.repeat_canceled);
}

#[test]
fn scan_for_repeat_allows_distant_repetition_outside_proximity() {
    // A 30-char window may legitimately appear twice in a long
    // response (e.g. an identifier the model mentions in two
    // separate sections). As long as the duplicates are farther
    // apart than REPEAT_PROXIMITY_CHARS, the detector should
    // stay quiet.
    let mut acc = OpenAiStreamAccumulator::new();
    let mut events = Vec::new();
    let phrase = "the_long_unique_identifier_name_x";
    let filler: String = "a".repeat(REPEAT_PROXIMITY_CHARS + 100);
    let body = format!("{phrase} {filler} {phrase}");
    let cont = acc
        .feed(&delta_payload(&body), &mut |e| events.push(e))
        .expect("ok");
    // Filler is all 'a' so it has its own self-overlap and WILL
    // trigger — but that's the right call (filler is itself a
    // 30-char repeat). Use a more realistic distant test:
    let _ = cont;
    let mut acc2 = OpenAiStreamAccumulator::new();
    acc2.kill_on_repeat = true;
    let mut events2 = Vec::new();
    let realistic = format!(
        "{phrase} blah blah blah blah blah blah blah blah blah \
         blah blah blah blah blah blah blah blah blah \
         {phrase}"
    );
    // Realistic: short identifier, less than the proximity gap
    // — this WILL trigger. Document expected behavior:
    let cont2 = acc2
        .feed(&delta_payload(&realistic), &mut |e| events2.push(e))
        .expect("ok");
    // 30-char window match within ~150 chars triggers (correct).
    assert!(!cont2);
}

#[test]
fn warn_only_mode_lets_stream_continue_after_repeat() {
    // Default (kill_on_repeat = false): a tripped detector should
    // emit a one-shot dim warning event but the stream must keep
    // going. The user explicitly does not want hard-truncation
    // anymore for interactive REPL sessions.
    let mut acc = OpenAiStreamAccumulator::new();
    // Make sure env var didn't accidentally enable kill mode in
    // this test process.
    acc.kill_on_repeat = false;
    let mut events: Vec<StreamEvent> = Vec::new();
    // Phrase must be >= REPEAT_WINDOW_CHARS to yield a repeating window.
    let phrase = "Haklısın, daha önceki 'nasıl yardımcı olabilirim' tarzı cevabım gereksizdi. ";
    assert!(phrase.chars().count() >= REPEAT_WINDOW_CHARS);
    let cont1 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont1, "first chunk must not trigger");
    let cont2 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont2, "warn-only mode must NOT cancel — stream continues");
    assert!(!acc.repeat_canceled, "warn-only must not set canceled");
    assert!(acc.repeat_warned, "warn must be marked emitted");
    // The warning event must have been delivered exactly once.
    let warns: Vec<&StreamEvent> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::TextDelta(s) if s.contains("repeat detector tripped")))
        .collect();
    assert_eq!(warns.len(), 1, "exactly one warning, got {}", warns.len());
    // A third feed must still be accepted (not stuck in canceled).
    let cont3 = acc
        .feed(&delta_payload(phrase), &mut |e| events.push(e))
        .expect("ok");
    assert!(cont3, "subsequent chunks still flow in warn-only mode");
    // No additional warning emitted on the third feed.
    let warns2: Vec<&StreamEvent> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::TextDelta(s) if s.contains("repeat detector tripped")))
        .collect();
    assert_eq!(warns2.len(), 1, "warning is one-shot");
}

#[test]
fn user_message_serializes_to_openai_shape() {
    let message = ChatMessage::user("hello");
    let value = serde_json::to_value(&message).expect("serialize");
    // The wire format must contain role + content and nothing else
    // when there are no tool calls.
    assert_eq!(value["role"], "user");
    assert_eq!(value["content"], "hello");
    assert!(value.get("tool_calls").is_none());
    assert!(value.get("tool_call_id").is_none());
}

#[test]
fn tool_result_carries_call_id_and_name() {
    let message = ChatMessage::tool_result("call_123", "read_file", "ok");
    let value = serde_json::to_value(&message).expect("serialize");
    assert_eq!(value["role"], "tool");
    assert_eq!(value["tool_call_id"], "call_123");
    assert_eq!(value["name"], "read_file");
    assert_eq!(value["content"], "ok");
}

#[test]
fn chat_response_parses_assistant_message_with_tool_call() {
    let body = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_42",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"src/main.rs\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14
        }
    }"#;
    let parsed: ChatResponse = serde_json::from_str(body).expect("parse response");
    assert_eq!(parsed.choices.len(), 1);
    let message = &parsed.choices[0].message;
    assert_eq!(message.role, Role::Assistant);
    assert!(message.content.is_none());
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].function.name, "read_file");
    let usage = parsed.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 4);
    assert_eq!(usage.total_tokens, 14);
}

#[test]
fn missing_key_yields_descriptive_error() {
    // Snapshot the env var so we can put it back after the test even
    // if a real key is configured on the developer's machine.
    let saved = std::env::var("DEEPSEEK_API_KEY").ok();
    // SAFETY: tests are single-threaded by default in this crate.
    unsafe {
        std::env::remove_var("DEEPSEEK_API_KEY");
    }
    let err = DeepSeekClient::from_env().expect_err("should fail without key");
    match err {
        ApiError::MissingKey(name) => assert_eq!(name, "DEEPSEEK_API_KEY"),
        other => panic!("unexpected error: {other:?}"),
    }
    if let Some(value) = saved {
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY", value);
        }
    }
}

#[test]
fn provider_lookup_is_case_insensitive_and_covers_builtins() {
    // The five built-in OpenAI-compatible providers must all be
    // reachable by their short id, regardless of case.
    for id in ["deepseek", "OpenAI", "openrouter", "glm", "gemini"] {
        let p = Provider::lookup(id).unwrap_or_else(|| panic!("missing provider {id}"));
        assert!(!p.base_url.is_empty());
        assert!(!p.env_var.is_empty());
        assert!(!p.default_model.is_empty());
    }
    // Anthropic is now a built-in too — make sure it lands here.
    assert!(Provider::lookup("anthropic").is_some());
    assert!(Provider::lookup("nonsense-vendor-xyz").is_none());
}

#[test]
fn anthropic_request_pulls_system_out_and_emits_content_blocks() {
    let request = ChatRequest {
        model: "claude-haiku-4-5-20251001".to_string(),
        messages: vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("hi"),
            ChatMessage {
                role: Role::Assistant,
                content: Some("calling tool".to_string()),
                content_blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "toolu_1".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"a.rs"}"#.to_string(),
                    },
                }],
                tool_call_id: None,
                name: None,
                protected: false,
                reasoning_content: None,
            },
            ChatMessage::tool_result("toolu_1", "read_file", "fn main() {}"),
        ],
        tools: Some(vec![ToolSpec {
            kind: ToolKind::Function,
            function: FunctionSpec {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }]),
        temperature: Some(0.2),
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };

    let body = AnthropicClient::build_messages_body(&request);

    // System lifted to top-level; as of v0.11 it's a content-block
    // array so we can attach a prompt-cache breakpoint to it.
    let sys_blocks = body["system"].as_array().expect("system array");
    assert_eq!(sys_blocks.len(), 1);
    assert_eq!(sys_blocks[0]["type"], "text");
    assert_eq!(sys_blocks[0]["text"], "be terse");
    assert_eq!(sys_blocks[0]["cache_control"]["type"], "ephemeral");
    // max_tokens defaulted in.
    assert_eq!(body["max_tokens"], 4096);
    // Tool spec flattened with `input_schema`, not `parameters`.
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert!(body["tools"][0]["input_schema"].is_object());
    assert!(body["tools"][0].get("parameters").is_none());
    // Last (and here: only) tool carries the ephemeral cache marker
    // so the whole tool block becomes a prompt-cache breakpoint.
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");

    let messages = body["messages"].as_array().expect("messages array");
    // System turn dropped, three remaining: user, assistant, user/tool_result.
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hi");

    assert_eq!(messages[1]["role"], "assistant");
    let asst_blocks = messages[1]["content"].as_array().expect("blocks");
    assert_eq!(asst_blocks[0]["type"], "text");
    assert_eq!(asst_blocks[0]["text"], "calling tool");
    assert_eq!(asst_blocks[1]["type"], "tool_use");
    assert_eq!(asst_blocks[1]["id"], "toolu_1");
    assert_eq!(asst_blocks[1]["name"], "read_file");
    // Crucially: input is a JSON object, not the raw string.
    assert_eq!(asst_blocks[1]["input"]["path"], "a.rs");

    // Tool result became a user turn with a tool_result block.
    assert_eq!(messages[2]["role"], "user");
    let tool_blocks = messages[2]["content"].as_array().expect("tool blocks");
    assert_eq!(tool_blocks[0]["type"], "tool_result");
    assert_eq!(tool_blocks[0]["tool_use_id"], "toolu_1");
    assert_eq!(tool_blocks[0]["content"], "fn main() {}");
}

#[test]
fn anthropic_response_with_text_and_tool_use_round_trips_to_openai_shape() {
    let value = serde_json::json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "thinking..."},
            {
                "type": "tool_use",
                "id": "toolu_42",
                "name": "grep",
                "input": {"pattern": "fn main"}
            }
        ],
        "model": "claude-haiku-4-5-20251001",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 12, "output_tokens": 7}
    });
    let parsed = AnthropicClient::parse_messages_response(&value).expect("parse");
    assert_eq!(parsed.choices.len(), 1);
    let msg = &parsed.choices[0].message;
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.content.as_deref(), Some("thinking..."));
    assert_eq!(msg.tool_calls.len(), 1);
    assert_eq!(msg.tool_calls[0].id, "toolu_42");
    assert_eq!(msg.tool_calls[0].function.name, "grep");
    // Arguments re-serialized to a string the agent loop can parse.
    let args: serde_json::Value =
        serde_json::from_str(&msg.tool_calls[0].function.arguments).unwrap();
    assert_eq!(args["pattern"], "fn main");
    // Stop reason mapped into OpenAI finish_reason vocabulary.
    assert_eq!(
        parsed.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    let usage = parsed.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 12);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.total_tokens, 19);
}

#[test]
fn anthropic_provider_is_routed_through_provider_lookup() {
    let provider = Provider::lookup("anthropic").expect("anthropic provider");
    assert_eq!(provider.wire, WireFormat::Anthropic);
    assert_eq!(provider.env_var, "ANTHROPIC_API_KEY");
    assert!(provider.default_model.starts_with("claude"));
}

#[test]
fn sse_parser_splits_blank_line_separated_frames_and_joins_multiline_data() {
    // Two frames, the second one uses multi-line `data:` per spec —
    // the parser must join the pieces with `\n` and yield a single
    // payload per frame.
    let body = "data: first\n\ndata: line one\ndata: line two\n\ndata: [DONE]\n\n";
    let mut payloads: Vec<String> = Vec::new();
    consume_sse(std::io::Cursor::new(body), |payload| {
        payloads.push(payload.to_string());
        Ok(true)
    })
    .expect("parse sse");
    assert_eq!(payloads, vec!["first", "line one\nline two", "[DONE]"]);
}

#[test]
fn sse_parser_stops_when_handler_returns_false() {
    // Once the callback returns Ok(false) — the convention for
    // OpenAI's `[DONE]` sentinel — the parser must not deliver any
    // further frames even if bytes remain in the stream.
    let body = "data: a\n\ndata: b\n\ndata: c\n\n";
    let mut seen = 0usize;
    consume_sse(std::io::Cursor::new(body), |_payload| {
        seen += 1;
        Ok(seen < 2) // stop after the second frame
    })
    .expect("parse sse");
    assert_eq!(seen, 2);
}

#[test]
fn sse_parser_ignores_event_and_comment_lines() {
    // Comments (`:` prefix) and non-`data:` fields must be dropped
    // without error; only `data:` is meaningful for the two
    // providers we currently speak.
    let body = ": keep-alive\nevent: message\nid: 1\ndata: payload\nretry: 1000\n\n";
    let mut seen: Vec<String> = Vec::new();
    consume_sse(std::io::Cursor::new(body), |payload| {
        seen.push(payload.to_string());
        Ok(true)
    })
    .expect("parse sse");
    assert_eq!(seen, vec!["payload"]);
}

#[test]
fn openai_accumulator_merges_text_deltas_and_tool_call_fragments() {
    // Simulates a realistic streamed tool call: text delta first,
    // then a tool_call whose id/name arrive in the first chunk and
    // whose JSON arguments are sharded across three chunks.
    let mut acc = OpenAiStreamAccumulator::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);

    let chunks = [
        r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"p"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ath\":\""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"a.rs\"}"}}]}}]}"#,
        r#"{"choices":[{"finish_reason":"tool_calls","delta":{}}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":8,"total_tokens":13}}"#,
        "[DONE]",
    ];
    for chunk in &chunks {
        let keep = acc.feed(chunk, &mut sink).expect("feed");
        if !keep {
            break;
        }
    }

    // Text deltas must have streamed through as-is, in order.
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["hel", "lo"]);

    // A single usage event must have been emitted with the final
    // counters.
    let usages: Vec<Usage> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].prompt_tokens, 5);
    assert_eq!(usages[0].completion_tokens, 8);

    let response = acc.into_response();
    let msg = &response.choices[0].message;
    assert_eq!(msg.content.as_deref(), Some("hello"));
    assert_eq!(msg.tool_calls.len(), 1);
    assert_eq!(msg.tool_calls[0].id, "call_1");
    assert_eq!(msg.tool_calls[0].function.name, "read_file");
    // The sharded JSON argument string must be the exact
    // concatenation of the three fragments.
    assert_eq!(msg.tool_calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(
        response.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    let usage = response.usage.expect("usage present");
    assert_eq!(usage.total_tokens, 13);
}

#[test]
fn openai_stream_body_sets_stream_and_usage_opts() {
    // The streaming helper must flip `stream` on and opt into
    // `include_usage` — otherwise OpenAI's final chunk would arrive
    // without token counters and the cost footer would go blank.
    let request = ChatRequest {
        model: "gpt-4o-mini".to_string(),
        messages: vec![ChatMessage::user("hi")],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = OpenAICompatClient::build_stream_body(&request);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["model"], "gpt-4o-mini");
}

#[test]
fn anthropic_accumulator_assembles_text_and_tool_use_blocks() {
    // Exercises the full Anthropic streaming handshake: message_start
    // with input_tokens, a text content block streaming three text
    // deltas, a tool_use block streaming its input as sharded JSON
    // fragments, message_delta with stop_reason and output_tokens,
    // and a final message_stop that flushes usage.
    let mut acc = AnthropicStreamAccumulator::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);

    let frames = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":11}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_7","name":"grep","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pat"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"tern\":\"fn main\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    for frame in &frames {
        let keep = acc.feed(frame, &mut sink).expect("feed");
        if !keep {
            break;
        }
    }

    // Text deltas forwarded verbatim, in order.
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["hi ", "there"]);

    // message_stop must synthesise exactly one usage event with
    // both counters populated from the two separate Anthropic
    // frames (input_tokens from message_start, output_tokens from
    // message_delta).
    let usages: Vec<Usage> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].prompt_tokens, 11);
    assert_eq!(usages[0].completion_tokens, 9);
    assert_eq!(usages[0].total_tokens, 20);

    let response = acc.into_response();
    let msg = &response.choices[0].message;
    assert_eq!(msg.content.as_deref(), Some("hi there"));
    assert_eq!(msg.tool_calls.len(), 1);
    assert_eq!(msg.tool_calls[0].id, "toolu_7");
    assert_eq!(msg.tool_calls[0].function.name, "grep");
    // Sharded JSON arguments re-concatenated exactly.
    assert_eq!(
        msg.tool_calls[0].function.arguments,
        r#"{"pattern":"fn main"}"#
    );
    // Anthropic `tool_use` stop reason maps to OpenAI `tool_calls`.
    assert_eq!(
        response.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
}

#[test]
fn usage_openai_shape_peels_cache_read_from_prompt_tokens() {
    // OpenAI's `prompt_tokens` counts cached reads *inside* the
    // total. The normalised Usage must subtract them so
    // `prompt_tokens` reflects only the fresh-input portion paid
    // at full rate, and `cache_read_tokens` holds the discounted
    // chunk. `cache_write_tokens` stays zero because OpenAI does
    // not bill cache writes separately.
    let raw = r#"{
        "prompt_tokens": 1200,
        "completion_tokens": 80,
        "total_tokens": 1280,
        "prompt_tokens_details": { "cached_tokens": 1000 }
    }"#;
    let usage: Usage = serde_json::from_str(raw).expect("parse usage");
    assert_eq!(usage.prompt_tokens, 200);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.cache_read_tokens, 1000);
    assert_eq!(usage.cache_write_tokens, 0);
    // Local recomputation: 200 fresh + 80 out + 1000 cache read.
    assert_eq!(usage.total_tokens, 1280);
}

#[test]
fn usage_anthropic_shape_carries_cache_creation_and_read() {
    // Anthropic reports the cache counters as flat siblings of
    // `input_tokens`, and `input_tokens` is already the fresh
    // non-cached count. The normalised Usage must mirror that
    // layout and fold *all four* fields into `total_tokens`.
    let raw = r#"{
        "input_tokens": 50,
        "output_tokens": 30,
        "cache_creation_input_tokens": 400,
        "cache_read_input_tokens": 1500
    }"#;
    let usage: Usage = serde_json::from_str(raw).expect("parse usage");
    assert_eq!(usage.prompt_tokens, 50);
    assert_eq!(usage.completion_tokens, 30);
    assert_eq!(usage.cache_write_tokens, 400);
    assert_eq!(usage.cache_read_tokens, 1500);
    assert_eq!(usage.total_tokens, 50 + 30 + 400 + 1500);
}

#[test]
fn anthropic_stream_accumulator_forwards_cache_tokens_through_message_start() {
    // A second-turn claude response: the prior turn's system
    // preamble is served from cache (cache_read > 0), a small
    // delta is newly written (cache_write > 0), only a handful of
    // fresh tokens are billed at the full input rate. The
    // accumulator must fold every one of those counters into the
    // synthesised final Usage event.
    let mut acc = AnthropicStreamAccumulator::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);

    let frames = [
        r#"{"type":"message_start","message":{"usage":{
            "input_tokens": 20,
            "cache_creation_input_tokens": 300,
            "cache_read_input_tokens": 4000
        }}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    for frame in &frames {
        if !acc.feed(frame, &mut sink).expect("feed") {
            break;
        }
    }
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .expect("usage emitted");
    assert_eq!(usage.prompt_tokens, 20);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.cache_write_tokens, 300);
    assert_eq!(usage.cache_read_tokens, 4000);
    assert_eq!(usage.total_tokens, 20 + 7 + 300 + 4000);

    let response = acc.into_response();
    // The finalised response must report the same counters so
    // `final_text` callers see cache activity without subscribing
    // to the stream callback.
    let final_usage = response.usage.expect("final usage");
    assert_eq!(final_usage, usage);
}

#[test]
fn anthropic_build_messages_body_marks_last_tool_with_cache_control() {
    // Two tools — only the *last* tool should carry the ephemeral
    // cache_control marker so the tool block becomes a single
    // prompt-cache breakpoint. Putting it on every tool would
    // either split into too many breakpoints (and Anthropic caps
    // the count) or waste cache slots on identical prefixes.
    let request = ChatRequest {
        model: "claude-haiku-4-5-20251001".to_string(),
        messages: vec![ChatMessage::user("go")],
        thinking: false,
        thinking_budget: 0,
        tools: Some(vec![
            ToolSpec {
                kind: ToolKind::Function,
                function: FunctionSpec {
                    name: "read_file".to_string(),
                    description: "read a file".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            },
            ToolSpec {
                kind: ToolKind::Function,
                function: FunctionSpec {
                    name: "grep".to_string(),
                    description: "search".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            },
        ]),
        temperature: None,
        max_tokens: None,
    };
    let body = AnthropicClient::build_messages_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    // First tool: no breakpoint, it lives inside the cached prefix.
    assert!(tools[0].get("cache_control").is_none());
    // Last tool: breakpoint marker present.
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[tokio::test]
async fn default_chat_stream_synthesises_one_delta_and_usage_for_non_streaming_providers() {
    // A provider that only implements `chat` must inherit the
    // default `chat_stream` that fakes a single text delta + usage
    // event. This is the contract the existing ScriptedProvider
    // tests (and every future test double) rely on.
    struct Fake;
    #[async_trait]
    impl ChatProvider for Fake {
        async fn chat(&self, _req: &ChatRequest) -> ApiResult<ChatResponse> {
            Ok(ChatResponse {
                choices: vec![ChatChoice {
                    message: ChatMessage::assistant_text("hello world"),
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    total_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
            })
        }
    }

    let request = ChatRequest {
        model: "fake".to_string(),
        messages: vec![ChatMessage::user("hi")],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let mut events: Vec<StreamEvent> = Vec::new();
    let response = Fake
        .chat_stream(&request, &mut |e| events.push(e))
        .await
        .expect("default chat_stream");
    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::TextDelta(s) => assert_eq!(s, "hello world"),
        other => panic!("expected TextDelta, got {other:?}"),
    }
    match &events[1] {
        StreamEvent::Usage(u) => assert_eq!(u.total_tokens, 5),
        other => panic!("expected Usage, got {other:?}"),
    }
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("hello world")
    );
}

#[test]
fn provider_client_from_env_reports_missing_key_with_provider_var() {
    // Picking a provider whose env var is unlikely to be set in dev
    // environments. The error must name the provider's variable, not
    // a hard-coded DEEPSEEK_API_KEY.
    let provider = Provider::lookup("openai").expect("openai provider");
    let saved = std::env::var(provider.env_var).ok();
    // SAFETY: tests in this crate are single-threaded by default.
    unsafe {
        std::env::remove_var(provider.env_var);
    }
    // `Box<dyn ChatProvider>` does not implement Debug, so the
    // `expect_err` shortcut would not compile here — match instead.
    match provider.client_from_env() {
        Ok(_) => panic!("expected MissingKey error"),
        Err(ApiError::MissingKey(name)) => assert_eq!(name, provider.env_var),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
    if let Some(value) = saved {
        unsafe {
            std::env::set_var(provider.env_var, value);
        }
    }
}

#[test]
fn anthropic_accumulator_handles_thinking_blocks() {
    // Anthropic extended thinking emits thinking blocks before text.
    // The accumulator must emit ThinkingDelta events and exclude
    // thinking content from the final response text.
    let mut acc = AnthropicStreamAccumulator::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);

    let frames = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think..."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"the answer"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    for frame in &frames {
        if !acc.feed(frame, &mut sink).expect("feed") {
            break;
        }
    }

    // Should have 2 ThinkingDelta events + 1 TextDelta + 1 Usage
    let thinking: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec!["let me ", "think..."]);

    let text: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, vec!["the answer"]);

    // Final response should only contain text, not thinking.
    let response = acc.into_response();
    let msg = &response.choices[0].message;
    assert_eq!(msg.content.as_deref(), Some("the answer"));
}

#[test]
fn anthropic_build_messages_body_includes_thinking_when_enabled() {
    let request = ChatRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![ChatMessage::user("think hard")],
        tools: None,
        temperature: Some(0.5),
        max_tokens: None,
        thinking: true,
        thinking_budget: 10000,
    };
    let body = AnthropicClient::build_messages_body(&request);
    // Must have thinking block
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 10000);
    // Temperature must be removed when thinking is enabled
    assert!(body.get("temperature").is_none());
}

#[test]
fn anthropic_build_messages_body_omits_thinking_when_disabled() {
    let request = ChatRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![ChatMessage::user("just answer")],
        tools: None,
        temperature: Some(0.7),
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = AnthropicClient::build_messages_body(&request);
    assert!(body.get("thinking").is_none());
    // Temperature should be preserved
    assert!(body.get("temperature").is_some());
}

#[test]
fn openai_accumulator_forwards_reasoning_content_as_thinking_delta() {
    // DeepSeek-reasoner emits `reasoning_content` in stream deltas.
    let mut acc = OpenAiStreamAccumulator::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);

    let frames = [
        r#"{"choices":[{"delta":{"reasoning_content":"step 1..."}}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"step 2..."}}]}"#,
        r#"{"choices":[{"delta":{"content":"final answer"}}]}"#,
        r#"{"choices":[{"finish_reason":"stop"}]}"#,
        "[DONE]",
    ];
    for frame in &frames {
        if !acc.feed(frame, &mut sink).expect("feed") {
            break;
        }
    }

    let thinking: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec!["step 1...", "step 2..."]);

    let text: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, vec!["final answer"]);

    // reasoning_content should NOT end up in the final response text
    let response = acc.into_response();
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("final answer")
    );
    // …but it IS captured on the message so DeepSeek V4 thinking models
    // can echo it back on the next turn (API returns 400 otherwise).
    assert_eq!(
        response.choices[0].message.reasoning_content.as_deref(),
        Some("step 1...step 2..."),
    );
}

#[test]
fn chat_message_reasoning_content_serializes_on_wire() {
    // DeepSeek V4 Pro requires `reasoning_content` to round-trip on
    // multi-turn requests. Verify the field lands in the request body
    // when present, and is omitted when `None` so OpenAI/other
    // providers don't see a stray unknown key.
    use crate::types::{ChatMessage, ChatRequest};

    let mut asst = ChatMessage::assistant_text("answer");
    asst.reasoning_content = Some("chain of thought".to_string());
    let req = ChatRequest {
        model: "deepseek-v4-flash".to_string(),
        messages: vec![ChatMessage::user("q"), asst],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = crate::openai::OpenAICompatClient::build_stream_body(&req);
    let msgs = body["messages"].as_array().expect("messages array");
    assert_eq!(msgs[0].get("reasoning_content"), None, "user msg: no field");
    assert_eq!(
        msgs[1]["reasoning_content"].as_str(),
        Some("chain of thought"),
        "assistant msg: field echoed back",
    );
}

// ---------------------------------------------------------------
// Gemini sanitizer tests
// ---------------------------------------------------------------

#[test]
fn gemini_sanitize_converts_all_system_to_user() {
    let msgs = vec![
        ChatMessage::system("you are helpful"),
        ChatMessage::user("do something"),
        ChatMessage::system("compaction summary"),
    ];
    let out = OpenAICompatClient::sanitize_for_gemini(&msgs);
    // All three become user, consecutive users merge:
    // [system→user "you are helpful"] + [user "do something"] + [system→user "compaction"] → merged
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].role, Role::User);
    let c = out[0].content.as_ref().unwrap();
    assert!(c.contains("you are helpful"));
    assert!(c.contains("do something"));
    assert!(c.contains("compaction summary"));
}

#[test]
fn gemini_sanitize_moves_user_out_of_tool_group() {
    let msgs = vec![
        ChatMessage::system("prompt"),
        ChatMessage::user("read my file"),
        ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: ToolCallFunction {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            }],
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        },
        // Hook output injected between tool_call and tool_result
        ChatMessage::system("hook output"),
        ChatMessage::tool_result("c1", "read_file", "file contents"),
    ];
    let out = OpenAICompatClient::sanitize_for_gemini(&msgs);
    // All system→user, consecutive users merged, hook moved before tool group.
    // Expected: user("prompt\nread my file\nhook output"), assistant, tool
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].role, Role::User);
    let c = out[0].content.as_ref().unwrap();
    assert!(c.contains("prompt"));
    assert!(c.contains("read my file"));
    assert!(c.contains("hook output"));
    assert_eq!(out[1].role, Role::Assistant);
    assert!(!out[1].tool_calls.is_empty());
    assert_eq!(out[2].role, Role::Tool);
}

#[test]
fn gemini_sanitize_clean_transcript() {
    let msgs = vec![
        ChatMessage::system("prompt"),
        ChatMessage::user("hello"),
        ChatMessage::assistant_text("hi there"),
    ];
    let out = OpenAICompatClient::sanitize_for_gemini(&msgs);
    // system→user merged with next user → 2 messages
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, Role::User);
    assert!(out[0].content.as_ref().unwrap().contains("prompt"));
    assert!(out[0].content.as_ref().unwrap().contains("hello"));
    assert_eq!(out[1].role, Role::Assistant);
}

// ---------------------------------------------------------------
// OpenAI-compat sanitize_tool_calls tests
// ---------------------------------------------------------------

/// Regression: `/btw` or any direct store append can wedge a user
/// message between an assistant(tool_calls) and its tool reply. The
/// sanitizer must drop the orphaned tool so the provider doesn't 400
/// with "Messages with role 'tool' must be a response to a preceding
/// message with 'tool_calls'". The previous `.rev().find()` check
/// would rescue the tool by pointing at an earlier valid tool group
/// in `out`, leaking the orphan through.
#[test]
fn sanitize_drops_orphan_tool_after_user_despite_earlier_valid_group() {
    let valid_assistant = ChatMessage {
        role: Role::Assistant,
        content: None,
        content_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "c_prev".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let broken_assistant = ChatMessage {
        role: Role::Assistant,
        content: None,
        content_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "c_broken".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let msgs = vec![
        ChatMessage::system("prompt"),
        ChatMessage::user("read a"),
        valid_assistant,
        ChatMessage::tool_result("c_prev", "read_file", "ok"),
        // `/btw` note slipped in between the next assistant turn and
        // its tool reply — this is the shape that produced the 400.
        broken_assistant,
        ChatMessage::user("[btw] heads up"),
        ChatMessage::tool_result("c_broken", "read_file", "ok"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    // The orphan tool(c_broken) must be dropped. The earlier valid
    // pair must survive. The `/btw` user note can stay; it's just a
    // stray user message now.
    let has_orphan = out
        .iter()
        .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("c_broken"));
    assert!(!has_orphan, "orphan tool must be dropped: {out:#?}");
    let has_valid = out
        .iter()
        .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("c_prev"));
    assert!(has_valid, "valid tool must survive: {out:#?}");
    // And the message directly preceding any surviving Tool must be
    // either another Tool or Assistant with tool_calls.
    for (i, m) in out.iter().enumerate() {
        if m.role == Role::Tool {
            let prev = &out[i - 1];
            assert!(
                prev.role == Role::Tool
                    || (prev.role == Role::Assistant && !prev.tool_calls.is_empty()),
                "tool at {i} preceded by invalid {:?}: {out:#?}",
                prev.role
            );
        }
    }
}

#[test]
fn sanitize_keeps_clean_assistant_tool_pair() {
    let assistant = ChatMessage {
        role: Role::Assistant,
        content: None,
        content_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let msgs = vec![
        ChatMessage::user("hi"),
        assistant,
        ChatMessage::tool_result("c1", "read_file", "ok"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].role, Role::User);
    assert_eq!(out[1].role, Role::Assistant);
    assert_eq!(out[2].role, Role::Tool);
}

/// Regression: on `--resume` with a corrupted session file a tool
/// message can appear with a tool_call_id that no preceding
/// assistant advertised. The sanitizer must drop it even though
/// `out.last()` is a valid Assistant+tool_calls.
#[test]
fn sanitize_drops_tool_with_unknown_call_id() {
    let assistant = ChatMessage {
        role: Role::Assistant,
        content: None,
        content_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let msgs = vec![
        ChatMessage::user("hi"),
        assistant,
        // Wrong id — c1 was announced, not c_ghost.
        ChatMessage::tool_result("c_ghost", "read_file", "ok"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    // Assistant with unanswered c1 is an incomplete turn, so it gets
    // dropped together with the mis-addressed tool reply.
    assert!(
        out.iter().all(|m| m.role != Role::Tool),
        "unknown-id tool must be dropped: {out:#?}"
    );
    assert!(
        !out.iter()
            .any(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()),
        "incomplete assistant must be dropped: {out:#?}"
    );
}

/// Ghost assistant: no content, no content_blocks, no tool_calls.
/// Providers reject these with "content or tool_calls must be set" 400.
/// The sanitizer must drop them so they never reach the wire.
#[test]
fn sanitize_drops_ghost_assistant_with_no_content_or_tool_calls() {
    let msgs = vec![
        ChatMessage::user("hello"),
        ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        },
        ChatMessage::user("are you there?"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    // Ghost assistant must be dropped; only the two user messages survive.
    assert_eq!(out.len(), 2, "ghost assistant must be dropped: {out:#?}");
    assert!(out.iter().all(|m| m.role == Role::User));
}

#[test]
fn sanitize_keeps_assistant_with_content_but_no_tool_calls() {
    let msgs = vec![
        ChatMessage::user("hello"),
        ChatMessage::assistant_text("hi there"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    assert_eq!(out.len(), 2, "assistant with text content must survive: {out:#?}");
    assert_eq!(out[1].role, Role::Assistant);
    assert_eq!(out[1].content.as_deref(), Some("hi there"));
}

#[test]
fn sanitize_keeps_assistant_with_tool_calls_but_no_content() {
    // This is the normal tool-call case: content=None, tool_calls non-empty.
    let msgs = vec![
        ChatMessage::user("read file"),
        ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: ToolCallFunction {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            }],
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        },
        ChatMessage::tool_result("c1", "read_file", "ok"),
    ];
    let out = OpenAICompatClient::sanitize_tool_calls(&msgs);
    assert_eq!(out.len(), 3, "normal tool-call turn must survive: {out:#?}");
    assert!(out
        .iter()
        .any(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()));
}

#[test]
fn multimodal_user_message_serializes_to_openai_image_url_format() {
    // Regression: a user message carrying an Image block used to land
    // on the wire as `content: null, content_blocks: [...]`, which
    // OpenAI / DeepSeek / OpenRouter all reject with
    // `missing field content`. The rewrite must turn it into a proper
    // `content` array with `image_url` entries.
    use crate::ContentBlock;
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![
            ChatMessage::system("be terse"),
            ChatMessage::user_multimodal(vec![
                ContentBlock::Text {
                    text: "describe this".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/jpeg".to_string(),
                    data: "AAAA".to_string(),
                },
            ]),
        ],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = OpenAICompatClient::build_stream_body(&req);
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);

    let user = &messages[1];
    assert!(
        user.get("content_blocks").is_none(),
        "content_blocks must be stripped from the wire body"
    );
    let content = user["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2, "two blocks → two content entries");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "describe this");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"], "data:image/jpeg;base64,AAAA",
        "image block must become a data: URL in image_url.url"
    );
}

#[test]
fn text_only_message_still_serializes_as_plain_string() {
    // Don't regress the common case: plain text user messages stay
    // as `content: "…"`, not a one-entry array.
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage::user("hi")],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = OpenAICompatClient::build_stream_body(&req);
    assert_eq!(body["messages"][0]["content"], "hi");
    assert!(body["messages"][0].get("content_blocks").is_none());
}

#[test]
fn tool_result_multimodal_flattens_to_text_for_openai() {
    // OpenAI's `tool` role only accepts string content. A multimodal
    // tool result (e.g. read_file on an image) should have its text
    // blocks concatenated and images dropped, not sent as `null`.
    use crate::ContentBlock;
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![
            ChatMessage::user("look"),
            ChatMessage {
                role: Role::Assistant,
                content: None,
                content_blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "t1".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
                tool_call_id: None,
                name: None,
                protected: false,
                reasoning_content: None,
            },
            ChatMessage::tool_result_multimodal(
                "t1",
                "read_file",
                vec![
                    ContentBlock::Text {
                        text: "[Image: foo.png 123 bytes]".to_string(),
                    },
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "BBBB".to_string(),
                    },
                ],
            ),
        ],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let body = OpenAICompatClient::build_stream_body(&req);
    let tool_msg = &body["messages"][2];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["content"], "[Image: foo.png 123 bytes]");
    assert!(tool_msg.get("content_blocks").is_none());
}

#[test]
fn tool_choice_injected_when_tools_present() {
    use crate::openai::OpenAICompatClient;
    use crate::{FunctionSpec, ToolKind, ToolSpec};
    let req = ChatRequest {
        model: "deepseek-v4-flash".to_string(),
        messages: vec![ChatMessage::user("read the file")],
        tools: Some(vec![ToolSpec {
            kind: ToolKind::Function,
            function: FunctionSpec {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        }]),
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    // build_stream_body itself does not inject tool_choice (no provider context).
    // inject_tool_choice is the helper tested separately.
    let mut body = OpenAICompatClient::build_stream_body(&req);
    OpenAICompatClient::inject_tool_choice(&mut body);
    assert_eq!(body["tool_choice"], "auto", "tool_choice must be auto when tools present");
    assert_eq!(body["parallel_tool_calls"], true, "parallel_tool_calls must be true");
}

#[test]
fn tool_choice_not_injected_when_no_tools() {
    use crate::openai::OpenAICompatClient;
    let req = ChatRequest {
        model: "deepseek-v4-flash".to_string(),
        messages: vec![ChatMessage::user("hello")],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    let mut body = OpenAICompatClient::build_stream_body(&req);
    OpenAICompatClient::inject_tool_choice(&mut body);
    assert!(body.get("tool_choice").is_none(), "tool_choice must be absent when no tools");
    assert!(body.get("parallel_tool_calls").is_none());
}

#[test]
fn deepseek_chat_rejects_images_client_side() {
    // Image + a known text-only model (deepseek-chat, deepseek-reasoner,
    // etc.) must fail fast with a readable error — not go out to the
    // wire and come back as a cryptic provider 400.
    use crate::ContentBlock;
    let req = ChatRequest {
        model: "deepseek-chat".to_string(),
        messages: vec![ChatMessage::user_multimodal(vec![
            ContentBlock::Text {
                text: "describe".to_string(),
            },
            ContentBlock::Image {
                media_type: "image/jpeg".to_string(),
                data: "AAAA".to_string(),
            },
        ])],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    // Smoke-test the predicates that power the fail-fast path.
    assert!(OpenAICompatClient::request_has_images(&req));
    assert!(OpenAICompatClient::last_user_has_images(&req));
    assert!(OpenAICompatClient::model_is_text_only(&req.model));
    assert!(OpenAICompatClient::model_is_text_only("deepseek-reasoner"));
    assert!(!OpenAICompatClient::model_is_text_only("gpt-4o"));
    assert!(!OpenAICompatClient::model_is_text_only("claude-sonnet-4-5"));
}

#[test]
fn history_image_does_not_lock_text_turn() {
    // Repro for the lock-up Atakan hit: image attached on turn 1, then
    // a plain text turn 2. Without history-strip, every later turn
    // sees an Image block in history and refuses the request even
    // though the user only typed text. Predicates must distinguish
    // "current turn carries an image" from "an old image lives in
    // history" — only the former should fail-fast.
    use crate::ContentBlock;
    let req = ChatRequest {
        model: "deepseek-v4-flash".to_string(),
        messages: vec![
            ChatMessage::user_multimodal(vec![
                ContentBlock::Text {
                    text: "describe this".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "AAAA".to_string(),
                },
            ]),
            ChatMessage::assistant_text("[earlier reply]"),
            ChatMessage::user("can you translate the text in that screenshot?"),
        ],
        tools: None,
        temperature: None,
        max_tokens: None,
        thinking: false,
        thinking_budget: 0,
    };
    assert!(OpenAICompatClient::request_has_images(&req));
    assert!(!OpenAICompatClient::last_user_has_images(&req));
    let stripped = OpenAICompatClient::strip_image_blocks_from_history(&req.messages);
    assert_eq!(stripped.len(), 3);
    assert!(
        !stripped[0]
            .content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "history image must be replaced with a text marker"
    );
    let last = stripped.last().expect("last message present");
    assert_eq!(last.content.as_deref(), Some("can you translate the text in that screenshot?"));
}
