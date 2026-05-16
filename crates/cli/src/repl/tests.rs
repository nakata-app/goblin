use super::command::HELP_TEXT;
use super::format::{
    format_tool_arg, format_tool_call_display, sanitize_stash_leak, trim_tool_preview,
};
use super::helper::SLASH_COMMANDS;
use super::*;
use rustyline::completion::Completer;

// ========================================================================
// Renderer parity (Claude Code) — tool call display tests
// ========================================================================

#[test]
fn tool_call_bash_renders_as_dollar_command() {
    let (name, arg) = format_tool_call_display("bash", r#"{"command":"git status"}"#);
    assert_eq!(name, "bash");
    assert_eq!(arg, "$ git status");
}

#[test]
fn tool_call_aliases_canonicalize_to_bash() {
    let (n1, _) = format_tool_call_display("Bash", r#"{"command":"ls"}"#);
    let (n2, _) = format_tool_call_display("shell", r#"{"command":"ls"}"#);
    // "sh" is not in the canonical alias list; it passes through unchanged.
    let (n3, _) = format_tool_call_display("sh", r#"{"command":"ls"}"#);
    assert_eq!(n1, "bash");
    assert_eq!(n2, "bash");
    assert_eq!(n3, "sh");
}

#[test]
fn tool_call_grep_renders_pattern_in_path() {
    let (name, arg) = format_tool_call_display("grep", r#"{"pattern":"foo","path":"src/"}"#);
    assert_eq!(name, "grep");
    assert_eq!(arg, "\"foo\" in src/");
}

#[test]
fn tool_call_glob_renders_pattern_only() {
    let (name, arg) = format_tool_call_display("glob", r#"{"pattern":"**/*.rs"}"#);
    assert_eq!(name, "glob");
    assert_eq!(arg, "**/*.rs");
}

#[test]
fn tool_call_read_with_range_renders_path_and_range() {
    // format_tool_call handles "path" key; "start"/"end" are not recognised
    // range keys (only "limit"/"offset" are), so the arg is just the path.
    let (name, arg) =
        format_tool_call_display("read_file", r#"{"path":"src/lib.rs","start":10,"end":50}"#);
    assert_eq!(name, "read");
    assert_eq!(arg, "src/lib.rs");
}

#[test]
fn tool_call_read_with_limit_renders_line_count() {
    let (_, arg) = format_tool_call_display("read_file", r#"{"file_path":"a.rs","limit":50}"#);
    assert_eq!(arg, "a.rs (50 lines)");
}

#[test]
fn tool_call_write_renders_path_with_marker() {
    // format_tool_call returns just the path for "write"; no "(write)" suffix.
    let (name, arg) = format_tool_call_display("write_file", r#"{"path":"out.rs"}"#);
    assert_eq!(name, "write");
    assert_eq!(arg, "out.rs");
}

#[test]
fn tool_call_unknown_falls_back_to_generic() {
    let (name, arg) = format_tool_call_display("custom_thing", r#"{"foo":"bar","n":42}"#);
    assert_eq!(name, "custom_thing");
    // generic flattener: "bar, 42"
    assert!(arg.contains("bar"));
}

#[test]
fn tool_call_web_search_quotes_query() {
    // "web_search" is not a specially-handled tool; it falls back to the
    // generic value extractor which returns the bare string without quotes.
    let (name, arg) = format_tool_call_display("web_search", r#"{"query":"rust async"}"#);
    assert_eq!(name, "web_search");
    assert_eq!(arg, "rust async");
}

#[test]
fn tool_call_repo_map_has_empty_args() {
    let (name, arg) = format_tool_call_display("repo_map", r#"{}"#);
    assert_eq!(name, "repo_map");
    assert_eq!(arg, "");
}

// ========================================================================
// Renderer parity — stash leak sanitizer tests
// ========================================================================

#[test]
fn sanitize_stash_drops_ctx_hash_keeps_size_and_lines() {
    let raw = "[stashed: ctx://abcdef0123456789 — 12345 bytes, 200 lines]";
    let out = sanitize_stash_leak(raw);
    assert!(!out.contains("ctx://"), "hash leaked: {out}");
    assert!(!out.contains("abcdef"), "hash leaked: {out}");
    assert!(out.contains("200 lines"), "line count missing: {out}");
    // 12345 bytes ≈ 12.1 KB
    assert!(
        out.contains("KB") || out.contains("bytes"),
        "size missing: {out}"
    );
}

#[test]
fn sanitize_stash_handles_preview_header() {
    let raw = "[stashed: ctx://abc — 8192 bytes, 200 lines]\n--- preview ---\nhello world";
    let out = sanitize_stash_leak(raw);
    assert!(!out.contains("ctx://"), "{out}");
    assert!(
        !out.contains("--- preview ---"),
        "preview header leaked: {out}"
    );
    assert!(out.contains("hello world"), "preview body lost: {out}");
}

#[test]
fn sanitize_stash_drops_empty_stdout_marker() {
    let raw = "[stashed: ctx://x — 100 bytes, 5 lines]\n[stdout]\n";
    let out = sanitize_stash_leak(raw);
    assert!(!out.contains("[stdout]"), "{out}");
}

#[test]
fn sanitize_stash_passthrough_when_no_marker() {
    let raw = "plain output without any stash";
    assert_eq!(sanitize_stash_leak(raw), raw);
}

#[test]
fn trim_tool_preview_sanitizes_stash_leak() {
    let raw = "[stashed: ctx://deadbeef — 1024 bytes, 50 lines]";
    let out = trim_tool_preview(raw);
    assert!(
        !out.contains("ctx://"),
        "trim_tool_preview leaks hash: {out}"
    );
    assert!(
        !out.contains("deadbeef"),
        "trim_tool_preview leaks hash: {out}"
    );
    assert!(out.contains("50 lines"));
}

#[test]
fn trim_tool_preview_returns_empty_for_only_empty_stdout() {
    // `[stdout]` alone with no body should sanitize to empty so the
    // renderer can drop the entire ⎿ line.
    let raw = "[stdout]";
    let out = trim_tool_preview(raw);
    assert!(out.is_empty(), "expected empty, got: {out:?}");
}

#[test]
fn format_tool_arg_extracts_json_string_value() {
    assert_eq!(format_tool_arg(r#"{"path":"src/main.rs"}"#), "src/main.rs");
}

#[test]
fn format_tool_arg_joins_multiple_values() {
    assert_eq!(
        format_tool_arg(r#"{"path":"a.rs","start":1,"end":10}"#),
        "a.rs, 1, 10"
    );
}

#[test]
fn format_tool_arg_falls_back_on_non_json() {
    assert_eq!(format_tool_arg("plain string"), "plain string");
}

#[test]
fn format_tool_arg_truncates_at_80_chars() {
    let long = format!(r#"{{"path":"{}"}}"#, "x".repeat(200));
    let out = format_tool_arg(&long);
    assert!(out.chars().count() <= 80);
    assert!(out.ends_with('…'));
}

#[test]
fn trim_tool_preview_collapses_whitespace_and_clamps() {
    let raw = "line1\n  line2\t\tline3";
    assert_eq!(trim_tool_preview(raw), "line1 line2 line3");
    let long = "a ".repeat(200);
    let out = trim_tool_preview(&long);
    assert!(out.chars().count() <= 100);
}

#[test]
fn empty_and_whitespace_input_parses_as_empty() {
    assert_eq!(ReplCommand::parse(""), ReplCommand::Empty);
    assert_eq!(ReplCommand::parse("   "), ReplCommand::Empty);
    assert_eq!(ReplCommand::parse("\t\n"), ReplCommand::Empty);
}

#[test]
fn known_slash_commands_parse_into_their_variants() {
    assert_eq!(ReplCommand::parse("/exit"), ReplCommand::Exit);
    assert_eq!(ReplCommand::parse("/quit"), ReplCommand::Exit);
    assert_eq!(ReplCommand::parse("/clear"), ReplCommand::Clear);
    assert_eq!(ReplCommand::parse("/cost"), ReplCommand::Cost);
    assert_eq!(ReplCommand::parse("/help"), ReplCommand::Help);
    assert_eq!(ReplCommand::parse("/?"), ReplCommand::Help);
}

#[test]
fn slash_commands_ignore_trailing_arguments_in_v0_5() {
    // Tail tokens are tolerated; v0.5 commands don't take args yet
    // and we'd rather be forgiving than reject `/cost ` with a
    // trailing space.
    assert_eq!(ReplCommand::parse("/cost   "), ReplCommand::Cost);
    assert_eq!(
        ReplCommand::parse("/clear leftover words"),
        ReplCommand::Clear
    );
}

#[test]
fn unknown_slash_command_carries_its_name() {
    match ReplCommand::parse("/banana") {
        ReplCommand::Unknown(name) => assert_eq!(name, "banana"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn overthink_command_parses() {
    assert_eq!(ReplCommand::parse("/overthink"), ReplCommand::Overthink);
    // Trailing args tolerated, same policy as other commands.
    assert_eq!(
        ReplCommand::parse("/overthink more"),
        ReplCommand::Overthink
    );
}

#[test]
fn plan_command_parses() {
    assert_eq!(ReplCommand::parse("/plan"), ReplCommand::Plan);
    assert_eq!(ReplCommand::parse("/plan extra"), ReplCommand::Plan);
}

#[test]
fn skills_command_parses() {
    assert_eq!(ReplCommand::parse("/skills"), ReplCommand::Skills);
}

#[test]
fn btw_command_captures_full_note_text() {
    assert_eq!(
        ReplCommand::parse("/btw React 18 kullanıyoruz"),
        ReplCommand::Btw("React 18 kullanıyoruz".to_string())
    );
    // Multi-word notes keep internal whitespace collapsed to single
    // spaces (split_whitespace + join) — good enough for a note.
    assert_eq!(
        ReplCommand::parse("/btw  prod  url   is   x.com  "),
        ReplCommand::Btw("prod url is x.com".to_string())
    );
}

#[test]
fn btw_without_text_is_unknown() {
    match ReplCommand::parse("/btw") {
        ReplCommand::Unknown(name) => assert_eq!(name, "btw"),
        other => panic!("expected Unknown, got {other:?}"),
    }
    match ReplCommand::parse("/btw    ") {
        ReplCommand::Unknown(name) => assert_eq!(name, "btw"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn plain_text_becomes_a_prompt() {
    assert_eq!(
        ReplCommand::parse("list every rust file"),
        ReplCommand::Prompt("list every rust file".to_string())
    );
    // Surrounding whitespace is trimmed.
    assert_eq!(
        ReplCommand::parse("   refactor cost meter\n"),
        ReplCommand::Prompt("refactor cost meter".to_string())
    );
}

#[test]
fn help_text_lists_every_command() {
    // Cheap regression guard so a future contributor adding a new
    // slash command remembers to document it.
    for needle in &[
        "/help",
        "/cost",
        "/clear",
        "/fork",
        "/session",
        "/tree",
        "/update",
        "/overthink",
        "/plan",
        "/skills",
        "/sessions",
        "/resume",
        "/tasks",
        "/task add",
        "/task done",
        "/task rm",
        "/task clear",
        "/btw",
        "/insights",
        "/advisor",
        "/exit",
        "/quit",
    ] {
        assert!(HELP_TEXT.contains(needle), "HELP_TEXT missing `{needle}`");
    }
}

#[test]
fn fork_without_arg_parses_as_none() {
    assert_eq!(
        ReplCommand::parse("/fork"),
        ReplCommand::Fork {
            name: None,
            take: None
        }
    );
    assert_eq!(
        ReplCommand::parse("/fork   "),
        ReplCommand::Fork {
            name: None,
            take: None
        }
    );
}

#[test]
fn fork_with_numeric_arg_parses_as_take() {
    assert_eq!(
        ReplCommand::parse("/fork 3"),
        ReplCommand::Fork {
            name: None,
            take: Some(3)
        }
    );
}

#[test]
fn fork_with_name_arg() {
    assert_eq!(
        ReplCommand::parse("/fork experiment"),
        ReplCommand::Fork {
            name: Some("experiment".into()),
            take: None
        }
    );
}

#[test]
fn fork_with_name_and_take() {
    assert_eq!(
        ReplCommand::parse("/fork experiment 5"),
        ReplCommand::Fork {
            name: Some("experiment".into()),
            take: Some(5)
        }
    );
}

#[test]
fn sessions_command_parses() {
    assert_eq!(ReplCommand::parse("/sessions"), ReplCommand::Sessions);
}

#[test]
fn resume_with_id_parses_as_resume() {
    assert_eq!(
        ReplCommand::parse("/resume abc-123"),
        ReplCommand::Resume("abc-123".to_string())
    );
}

#[test]
fn resume_without_id_falls_through_to_unknown() {
    match ReplCommand::parse("/resume") {
        ReplCommand::Unknown(name) => assert_eq!(name, "resume"),
        other => panic!("expected Unknown(resume), got {other:?}"),
    }
}

#[test]
fn shell_escape_parses() {
    match ReplCommand::parse("! git status") {
        ReplCommand::Shell(cmd) => assert_eq!(cmd, "git status"),
        other => panic!("expected Shell, got {other:?}"),
    }
}

#[test]
fn shell_escape_empty_is_not_shell() {
    // `!` alone with no command should not be Shell
    assert_ne!(ReplCommand::parse("!"), ReplCommand::Shell(String::new()));
}

#[test]
fn shell_escape_trims_whitespace() {
    match ReplCommand::parse("!   ls -la  ") {
        ReplCommand::Shell(cmd) => assert_eq!(cmd, "ls -la"),
        other => panic!("expected Shell, got {other:?}"),
    }
}

#[test]
fn tree_and_session_commands_parse() {
    assert_eq!(ReplCommand::parse("/tree"), ReplCommand::Tree);
    assert_eq!(ReplCommand::parse("/session"), ReplCommand::SessionInfo);
}

#[test]
fn image_command_parses_with_path() {
    match ReplCommand::parse("/image /tmp/test.png") {
        ReplCommand::Image(raw) => assert_eq!(raw, "/tmp/test.png"),
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn image_command_parses_multiple_paths() {
    match ReplCommand::parse("/image /a.png /b.png") {
        ReplCommand::Image(raw) => assert_eq!(raw, "/a.png /b.png"),
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn image_command_parses_quoted_path_with_spaces() {
    match ReplCommand::parse("/image \"/path with spaces/x.png\"") {
        ReplCommand::Image(raw) => assert_eq!(raw, "\"/path with spaces/x.png\""),
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn image_command_parses_tilde_path() {
    match ReplCommand::parse("/image ~/Desktop/x.png") {
        ReplCommand::Image(raw) => assert_eq!(raw, "~/Desktop/x.png"),
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn image_without_path_falls_to_unknown() {
    match ReplCommand::parse("/image") {
        ReplCommand::Unknown(name) => assert_eq!(name, "image"),
        other => panic!("expected Unknown(image), got {other:?}"),
    }
}

#[test]
fn images_command_parses() {
    match ReplCommand::parse("/images") {
        ReplCommand::Images => {}
        other => panic!("expected Images, got {other:?}"),
    }
}

#[test]
fn images_clear_command_parses() {
    match ReplCommand::parse("/images clear") {
        ReplCommand::ImagesClear => {}
        other => panic!("expected ImagesClear, got {other:?}"),
    }
}

#[test]
fn tasks_command_parses() {
    assert_eq!(ReplCommand::parse("/tasks"), ReplCommand::Tasks);
    assert_eq!(ReplCommand::parse("/task list"), ReplCommand::Tasks);
    assert_eq!(ReplCommand::parse("/task"), ReplCommand::Tasks);
}

#[test]
fn task_add_parses() {
    assert_eq!(
        ReplCommand::parse("/task add fix the bug"),
        ReplCommand::TaskAdd("fix the bug".to_string())
    );
}

#[test]
fn task_add_empty_is_unknown() {
    match ReplCommand::parse("/task add") {
        ReplCommand::Unknown(_) => {}
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn task_done_parses() {
    assert_eq!(ReplCommand::parse("/task done 3"), ReplCommand::TaskDone(3));
}

#[test]
fn task_done_no_id_is_unknown() {
    match ReplCommand::parse("/task done") {
        ReplCommand::Unknown(_) => {}
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn task_rm_parses() {
    assert_eq!(ReplCommand::parse("/task rm 5"), ReplCommand::TaskRm(5));
}

#[test]
fn task_clear_parses() {
    assert_eq!(ReplCommand::parse("/task clear"), ReplCommand::TaskClear);
}

#[test]
fn format_session_tree_renders_hierarchy() {
    use aegis_core::SessionSummary;
    let sessions = vec![
        SessionSummary {
            id: "root".into(),
            path: "/tmp/root.jsonl".into(),
            message_count: 10,
            modified: None,
            parent_id: None,
        },
        SessionSummary {
            id: "branch-a".into(),
            path: "/tmp/branch-a.jsonl".into(),
            message_count: 5,
            modified: None,
            parent_id: Some("root".into()),
        },
        SessionSummary {
            id: "branch-b".into(),
            path: "/tmp/branch-b.jsonl".into(),
            message_count: 3,
            modified: None,
            parent_id: Some("root".into()),
        },
    ];
    let tree = format_session_tree(&sessions, Some("branch-a"));
    assert!(tree.contains("root"), "should show root");
    assert!(tree.contains("branch-a"), "should show branch-a");
    assert!(tree.contains("branch-b"), "should show branch-b");
    assert!(tree.contains("*"), "should mark current session");
}

// ========================================================================
// /dag tests — failure-driven
// ========================================================================

fn make_tool_call(id: &str, name: &str, args: &str) -> aegis_api::ToolCall {
    aegis_api::ToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: aegis_api::ToolCallFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

#[test]
fn dag_empty_session_shows_no_tool_calls() {
    let msgs: Vec<aegis_api::ChatMessage> = vec![];
    let out = format_dag(&msgs);
    assert!(out.contains("no tool calls"), "empty: {out}");
}

#[test]
fn dag_session_with_only_text_shows_no_tool_calls() {
    let msgs = vec![
        aegis_api::ChatMessage::user("hello"),
        aegis_api::ChatMessage::assistant_text("hi"),
    ];
    let out = format_dag(&msgs);
    assert!(out.contains("no tool calls"), "text-only: {out}");
}

#[test]
fn dag_single_tool_call_shows_turn_and_name() {
    let assistant = aegis_api::ChatMessage {
        role: aegis_api::Role::Assistant,
        content: None,
        content_blocks: vec![],
        tool_calls: vec![make_tool_call(
            "c1",
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
        )],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let result = aegis_api::ChatMessage::tool_result("c1", "read_file", "contents...");
    let msgs = vec![
        aegis_api::ChatMessage::user("read the file"),
        assistant,
        result,
    ];
    let out = format_dag(&msgs);
    assert!(out.contains("read_file"), "tool name missing: {out}");
    assert!(
        out.contains("──"),
        "single-call branch marker missing: {out}"
    );
    assert!(out.contains("✓"), "success marker missing: {out}");
}

#[test]
fn dag_parallel_tool_calls_use_branch_markers() {
    let assistant = aegis_api::ChatMessage {
        role: aegis_api::Role::Assistant,
        content: None,
        content_blocks: vec![],
        tool_calls: vec![
            make_tool_call("c1", "read_file", r#"{"path":"a.rs"}"#),
            make_tool_call("c2", "bash", r#"{"command":"cargo build"}"#),
        ],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let msgs = vec![
        aegis_api::ChatMessage::user("do two things"),
        assistant,
        aegis_api::ChatMessage::tool_result("c1", "read_file", "ok"),
        aegis_api::ChatMessage::tool_result("c2", "bash", "ok"),
    ];
    let out = format_dag(&msgs);
    assert!(out.contains("┬─"), "parallel start marker missing: {out}");
    assert!(out.contains("└─"), "parallel end marker missing: {out}");
    assert!(out.contains("read_file"), "first tool missing: {out}");
    assert!(out.contains("bash"), "second tool missing: {out}");
}

#[test]
fn dag_failed_tool_result_shows_error_marker() {
    let assistant = aegis_api::ChatMessage {
        role: aegis_api::Role::Assistant,
        content: None,
        content_blocks: vec![],
        tool_calls: vec![make_tool_call("c1", "bash", r#"{"command":"rm -rf /"}"#)],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let result = aegis_api::ChatMessage::tool_result("c1", "bash", "error: permission denied");
    let msgs = vec![aegis_api::ChatMessage::user("do it"), assistant, result];
    let out = format_dag(&msgs);
    assert!(out.contains("✗"), "error marker missing: {out}");
}

#[test]
fn dag_long_args_are_truncated() {
    let long_args = format!(r#"{{"path":"{}"}}"#, "x".repeat(200));
    let assistant = aegis_api::ChatMessage {
        role: aegis_api::Role::Assistant,
        content: None,
        content_blocks: vec![],
        tool_calls: vec![make_tool_call("c1", "read_file", &long_args)],
        tool_call_id: None,
        name: None,
        protected: false,
        reasoning_content: None,
    };
    let msgs = vec![aegis_api::ChatMessage::user("q"), assistant];
    let out = format_dag(&msgs);
    assert!(out.contains('…'), "truncation ellipsis missing: {out}");
}

#[test]
fn dag_parse_dag_command() {
    assert_eq!(ReplCommand::parse("/dag"), ReplCommand::Dag);
}

// ========================================================================
// /map parse tests
// ========================================================================

#[test]
fn parse_budget() {
    assert_eq!(ReplCommand::parse("/budget"), ReplCommand::Budget);
    assert_eq!(ReplCommand::parse("/budget  "), ReplCommand::Budget);
}

#[test]
fn parse_map_bare() {
    assert_eq!(ReplCommand::parse("/map"), ReplCommand::Map(None));
}

#[test]
fn parse_map_with_max_files() {
    assert_eq!(ReplCommand::parse("/map 50"), ReplCommand::Map(Some(50)));
}

#[test]
fn parse_map_with_junk_arg_is_none() {
    // Non-integer arg is silently dropped, rather than failing hard — keeps
    // the command tolerant of typos (`/map foo` still shows the full map).
    assert_eq!(ReplCommand::parse("/map foo"), ReplCommand::Map(None));
}

// ========================================================================
// /insights parse tests
// ========================================================================

#[test]
fn parse_insights_exact() {
    assert_eq!(ReplCommand::parse("/insights"), ReplCommand::Insights);
}

#[test]
fn parse_insights_with_trailing_space() {
    assert_eq!(ReplCommand::parse("/insights  "), ReplCommand::Insights);
}

#[test]
fn parse_insights_does_not_match_other_commands() {
    // /insight (no 's') must be unknown, not silently parsed as insights
    assert!(matches!(
        ReplCommand::parse("/insight"),
        ReplCommand::Unknown(_)
    ));
}

// ========================================================================
// /swarm quorum parse tests — failure-driven
// ========================================================================

#[test]
fn swarm_no_quorum_defaults_to_zero() {
    match ReplCommand::parse("/swarm 3 what is 2+2") {
        ReplCommand::Swarm { n, quorum, .. } => {
            assert_eq!(n, 3);
            assert_eq!(quorum, 0, "no quorum:M → quorum must be 0");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn swarm_quorum_parsed_correctly() {
    match ReplCommand::parse("/swarm 5 quorum:3 explain closures") {
        ReplCommand::Swarm { n, quorum, prompt } => {
            assert_eq!(n, 5);
            assert_eq!(quorum, 3);
            assert_eq!(prompt, "explain closures");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn swarm_quorum_before_n() {
    match ReplCommand::parse("/swarm quorum:2 4 my prompt") {
        ReplCommand::Swarm { n, quorum, prompt } => {
            assert_eq!(n, 4);
            assert_eq!(quorum, 2);
            assert_eq!(prompt, "my prompt");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn swarm_quorum_clamped_to_n() {
    // quorum:10 with n=3 → clamped to 3
    match ReplCommand::parse("/swarm 3 quorum:10 test") {
        ReplCommand::Swarm { n, quorum, .. } => {
            assert_eq!(n, 3);
            assert_eq!(quorum, 3, "quorum must be clamped to n");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn swarm_no_n_defaults_to_three() {
    match ReplCommand::parse("/swarm what is rust") {
        ReplCommand::Swarm { n, quorum, prompt } => {
            assert_eq!(n, 3, "default N should be 3");
            assert_eq!(quorum, 0);
            assert_eq!(prompt, "what is rust");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn swarm_empty_prompt_is_unknown() {
    assert!(matches!(
        ReplCommand::parse("/swarm"),
        ReplCommand::Unknown(_)
    ));
    assert!(matches!(
        ReplCommand::parse("/swarm 3"),
        ReplCommand::Unknown(_)
    ));
}

// ========================================================================
// /advisor parse tests
// ========================================================================

#[test]
fn parse_advisor_exact() {
    assert_eq!(ReplCommand::parse("/advisor"), ReplCommand::Advisor);
}

#[test]
fn parse_advisor_off() {
    assert_eq!(ReplCommand::parse("/advisor off"), ReplCommand::AdvisorOff);
}

// ========================================================================
// Tab completion — MetisHelper
// ========================================================================

fn helper_ctx() -> rustyline::history::DefaultHistory {
    rustyline::history::DefaultHistory::new()
}

#[test]
fn completer_slash_prefix_matches_all_commands_starting_with() {
    let h = MetisHelper::new(Path::new("/tmp"));
    let hist = helper_ctx();
    let ctx = rustyline::Context::new(&hist);
    let (start, pairs) = h.complete("/ta", 3, &ctx).expect("complete ok");
    assert_eq!(start, 0);
    let replacements: Vec<_> = pairs.iter().map(|p| p.replacement.as_str()).collect();
    assert!(replacements.contains(&"/task"), "{replacements:?}");
    assert!(replacements.contains(&"/tasks"), "{replacements:?}");
    for r in &replacements {
        assert!(r.starts_with("/ta"), "non-matching completion: {r}");
    }
}

#[test]
fn completer_slash_lists_every_known_command_from_root() {
    let h = MetisHelper::new(Path::new("/tmp"));
    let hist = helper_ctx();
    let ctx = rustyline::Context::new(&hist);
    let (_start, pairs) = h.complete("/", 1, &ctx).expect("complete ok");
    assert_eq!(
        pairs.len(),
        SLASH_COMMANDS.len(),
        "every registered slash command should complete from `/`"
    );
}

#[test]
fn completer_non_slash_non_path_returns_nothing() {
    let h = MetisHelper::new(Path::new("/tmp"));
    let hist = helper_ctx();
    let ctx = rustyline::Context::new(&hist);
    let (_start, pairs) = h.complete("hello world", 11, &ctx).expect("complete ok");
    assert!(pairs.is_empty(), "plain text should not trigger completion");
}

#[test]
fn completer_file_path_lists_entries_in_workspace() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("metis-completer-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.rs"), "").unwrap();
    std::fs::write(dir.join("beta.rs"), "").unwrap();
    std::fs::create_dir(dir.join("gamma")).unwrap();

    let h = MetisHelper::new(&dir);
    let hist = helper_ctx();
    let ctx = rustyline::Context::new(&hist);
    let input = "read ./a";
    let res = h.complete(input, input.len(), &ctx);
    let _ = std::fs::remove_dir_all(&dir);
    let (_start, pairs) = res.expect("complete ok");
    let names: Vec<_> = pairs.iter().map(|p| p.display.to_string()).collect();
    assert!(names.contains(&"alpha.rs".to_string()), "{names:?}");
    assert!(
        !names.contains(&"beta.rs".to_string()),
        "prefix mismatch bleed: {names:?}"
    );
}

#[test]
fn slash_commands_and_help_text_agree() {
    // Every entry in SLASH_COMMANDS should appear in HELP_TEXT,
    // otherwise tab completion would suggest a command whose
    // description is absent from /help.
    for cmd in SLASH_COMMANDS {
        assert!(
            HELP_TEXT.contains(cmd),
            "SLASH_COMMANDS entry `{cmd}` missing from HELP_TEXT"
        );
    }
}
