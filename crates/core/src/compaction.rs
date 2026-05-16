//! Transcript compaction — keep long-running sessions under the
//! provider's context window.
//!
//! The agent loop consults [`maybe_compact`] before every provider
//! round-trip. If the last observed `prompt_tokens` exceeds
//! `trigger_ratio * context_window`, the compactor replaces a run of
//! older tool chatter with a single synthetic system message that
//! says "here's what got dropped". The newest turn and the original
//! system prompt are always preserved so the model still knows who it
//! is and what it was just asked to do.
//!
//! Why deterministic summarisation and not an LLM call?
//!
//! * **No extra cost.** Compaction runs inside the hot loop; making a
//!   second provider call for a summary would double the spend.
//! * **No extra failure mode.** An LLM-backed summariser introduces
//!   the possibility of losing important context silently. A
//!   placeholder is honest about what it dropped.
//! * **Upgrade path is open.** The signature is `maybe_compact(&mut
//!   transcript, prompt_tokens, cfg)`, so swapping in a smarter
//!   strategy later doesn't change any call site.
//!
//! The one thing we never drop is the most recent assistant message
//! that has `tool_calls`, together with its matching `tool` replies.
//! OpenAI-compatible providers reject a request that has a `tool`
//! message without the assistant turn that requested it, so breaking
//! that pairing would turn compaction into an instant 400 error.

use aegis_api::{ChatMessage, Role};

/// Knobs governing when and how aggressively to compact.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// The provider's context window in tokens. Used purely as a
    /// reference point — the compactor compares the last observed
    /// prompt token count against `trigger_ratio * context_window`.
    pub context_window: u32,
    /// Fraction of the context window at which compaction kicks in.
    /// 0.7 means "compact when the prompt alone is already using 70%
    /// of the window".
    pub trigger_ratio: f32,
    /// Number of most-recent messages to always keep verbatim, on top
    /// of the system prompt. A small tail window preserves the
    /// current task's state while still letting the compactor reclaim
    /// everything older.
    pub keep_tail: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            // 128k matches the real deepseek-chat window. The previous
            // 64k default was leaving the compactor disengaged on
            // sessions that the provider would still happily accept,
            // and then a single fat tool result would tip past 128k
            // and the request would 400. The CLI still exposes this
            // via `--context-window` for providers with smaller windows.
            context_window: 128_000,
            // Trigger earlier so a sudden tool result doesn't blow the
            // window before the next compaction pass. 0.55 leaves
            // ~57k tokens of headroom on a 128k budget — enough to
            // absorb one large grep / read_file without a 400.
            trigger_ratio: 0.55,
            keep_tail: 12,
        }
    }
}

impl CompactionConfig {
    /// The prompt-token threshold at which compaction triggers.
    pub fn trigger_tokens(&self) -> u32 {
        (self.context_window as f32 * self.trigger_ratio) as u32
    }
}

/// Micro-compaction: trim oversized tool outputs in-place before full
/// compaction kicks in. Fires at a lower token threshold than
/// [`maybe_compact`] — typically around 40% of the context window.
///
/// Unlike full compaction (which removes entire messages), microcompact
/// only shrinks tool result content that exceeds `max_tool_chars`. This
/// preserves message count and structure while reclaiming tokens from
/// large grep/read_file outputs.
///
/// Returns `true` if any trimming was performed.
pub fn maybe_micro_compact(
    transcript: &mut [ChatMessage],
    prompt_tokens: u32,
    micro_ratio: f32,
    context_window: u32,
    max_tool_chars: usize,
) -> bool {
    let threshold = (context_window as f32 * micro_ratio) as u32;
    if prompt_tokens < threshold {
        return false;
    }

    let mut changed = false;
    for msg in transcript.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        if let Some(content) = msg.content.as_mut() {
            if content.len() > max_tool_chars {
                head_truncate(content, max_tool_chars);
                changed = true;
            }
        }
    }
    changed
}

/// Blob-aware variant of [`maybe_micro_compact`]. Instead of head-truncating
/// oversize tool messages and discarding the trimmed bytes, the full
/// payload is stored in [`crate::blob_store::BlobStore`] (deduped by
/// content hash) and indexed in [`crate::blob_index::BlobIndex`], and the
/// in-memory message is replaced with a short summary that includes a
/// `ctx://<hex>` reference the model can resolve later via
/// `metis ctx show <hex>`.
///
/// Falls back to head-truncation when the blob layer rejects a write,
/// so a misbehaving filesystem can never make compaction itself fail.
#[cfg(feature = "ctx")]
pub fn maybe_micro_compact_with_blobs(
    transcript: &mut [ChatMessage],
    prompt_tokens: u32,
    micro_ratio: f32,
    context_window: u32,
    max_tool_chars: usize,
    store: &crate::blob_store::BlobStore,
    index: &crate::blob_index::BlobIndex,
) -> bool {
    let threshold = (context_window as f32 * micro_ratio) as u32;
    if prompt_tokens < threshold {
        return false;
    }

    let mut changed = false;
    for msg in transcript.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        let Some(content) = msg.content.as_mut() else {
            continue;
        };
        if content.len() <= max_tool_chars {
            continue;
        }

        // If this content has already been ctx-stashed (sandbox_exec
        // produced a `[stashed: ctx://…]` summary), there's nothing
        // to do.
        if content.starts_with("[stashed: ctx://") {
            continue;
        }

        // Stash the full content. On blob-layer failure, head-truncate
        // so the loop still makes progress.
        let meta = crate::blob_store::BlobMeta::new("compaction");
        let id = match store.store(content.as_bytes(), meta.clone()) {
            Ok(id) => id,
            Err(_) => {
                head_truncate(content, max_tool_chars);
                changed = true;
                continue;
            }
        };
        // Index is best-effort: failures here don't block compaction.
        let _ = index.add_and_commit(&id, &meta, content);

        let original_len = content.len();
        let lines = content.lines().count();
        *content = format!(
            "[micro-compact: stashed {} — {} bytes, {} lines, fetch with `metis ctx show {}`]",
            id.reference(),
            original_len,
            lines,
            &id.0[..id.0.len().min(crate::blob_store::ID_PREFIX_LEN)],
        );
        changed = true;
    }
    changed
}

/// Shared head-truncation helper used both by the legacy path and by
/// [`maybe_micro_compact_with_blobs`] when the blob layer fails.
fn head_truncate(content: &mut String, max_tool_chars: usize) {
    if content.len() <= max_tool_chars {
        return;
    }
    let mut truncated_at = max_tool_chars;
    while truncated_at > 0 && !content.is_char_boundary(truncated_at) {
        truncated_at -= 1;
    }
    let removed = content.len() - truncated_at;
    content.truncate(truncated_at);
    content.push_str(&format!("\n[micro-compact: trimmed {removed} chars]"));
}

/// Optional summarizer that can be plugged into [`maybe_compact`].
/// When `None`, a static placeholder is used. When `Some`, the
/// dropped messages are sent to an LLM for a proper summary.
pub type Summarizer<'a> = Option<&'a dyn Fn(&[ChatMessage]) -> Option<String>>;

/// If `prompt_tokens` has crossed the trigger, rewrite `transcript` in
/// place to be shorter. No-op otherwise.
///
/// The resulting transcript has this shape:
///
/// ```text
/// [system (original)?]
/// [system (synthetic "compacted N messages")]
/// [... keep_tail most-recent messages ...]
/// ```
///
/// Care is taken not to split an assistant-with-`tool_calls` from its
/// tool replies when trimming the tail.
pub fn maybe_compact(
    transcript: &mut Vec<ChatMessage>,
    prompt_tokens: u32,
    cfg: &CompactionConfig,
) {
    maybe_compact_with(transcript, prompt_tokens, cfg, None);
}

/// Like [`maybe_compact`] but accepts an optional LLM-backed
/// summarizer. If the summarizer returns `Some(text)`, that text
/// replaces the static placeholder. If it returns `None` (e.g. due
/// to an API error), the static placeholder is used as fallback.
pub fn maybe_compact_with(
    transcript: &mut Vec<ChatMessage>,
    prompt_tokens: u32,
    cfg: &CompactionConfig,
    summarizer: Summarizer<'_>,
) {
    if prompt_tokens < cfg.trigger_tokens() {
        return;
    }
    if transcript.len() <= cfg.keep_tail + 2 {
        return;
    }

    let total = transcript.len();
    let mut tail_start = total.saturating_sub(cfg.keep_tail);

    while tail_start > 0 {
        let m = &transcript[tail_start];
        if m.role == Role::Tool {
            tail_start -= 1;
            continue;
        }
        if m.role == Role::Assistant && !m.tool_calls.is_empty() {
            break;
        }
        break;
    }

    let has_system = transcript
        .first()
        .map(|m| m.role == Role::System)
        .unwrap_or(false);
    let head_end = if has_system { 1 } else { 0 };

    if tail_start <= head_end {
        return;
    }

    // Collect protected messages from the drop zone so they survive compaction.
    // These are messages explicitly marked `protected: true` (e.g. memory-injection
    // or context-primer system notes that must persist for the whole session).
    let protected: Vec<ChatMessage> = transcript[head_end..tail_start]
        .iter()
        .filter(|m| m.protected)
        .cloned()
        .collect();

    let dropped_messages: Vec<ChatMessage> = transcript[head_end..tail_start]
        .iter()
        .filter(|m| !m.protected)
        .cloned()
        .collect();
    let dropped = dropped_messages.len();

    if dropped == 0 && protected.is_empty() {
        return;
    }

    // Try LLM-backed summarization with quality gating; fall back to static placeholder.
    let summary_text = if dropped == 0 {
        // Nothing non-protected to compact — protected messages will be re-inserted.
        String::new()
    } else {
        summarizer
            .and_then(|f| {
                let first = f(&dropped_messages)?;
                // Quality gate: if summary is too short relative to dropped content, retry.
                let min_length = 50.min(dropped * 10); // at least 50 chars or 10 per message
                if first.len() < min_length && dropped > 3 {
                    // Retry with the same summarizer — the LLM may produce a better result.
                    f(&dropped_messages).or(Some(first))
                } else {
                    Some(first)
                }
            })
            .unwrap_or_else(|| {
                let context = extract_compaction_context(&dropped_messages);
                format!(
                    "[compacted {dropped} earlier message{s} to stay within the context window.\n\
                     {context}\
                     Continue from the most recent user request.]",
                    s = if dropped == 1 { "" } else { "s" }
                )
            })
    };

    let tail: Vec<ChatMessage> = transcript.drain(tail_start..).collect();
    transcript.truncate(head_end);
    // Re-insert protected messages immediately after the system prompt.
    transcript.extend(protected);
    if !summary_text.is_empty() {
        transcript.push(ChatMessage::system(summary_text));
    }
    transcript.extend(tail);
}

/// Build a summarizer closure that calls the given provider to
/// summarize dropped messages. Returns `None` if the LLM call fails.
pub fn llm_summarizer<'a>(
    client: &'a dyn aegis_api::ChatProvider,
    model: &'a str,
) -> impl Fn(&[ChatMessage]) -> Option<String> + 'a {
    move |dropped: &[ChatMessage]| {
        // Extract structured context from dropped messages before
        // summarising: file paths, tool usage stats, and decisions.
        let context = extract_compaction_context(dropped);

        let mut messages = vec![ChatMessage::system(format!(
            "You are summarizing a dropped portion of an AI coding session. \
             The summary will replace these messages in the context window, \
             so it must preserve everything the assistant needs to continue \
             working correctly.\n\n\
             RULES:\n\
             1. List every file that was EDITED or CREATED with its full path.\n\
             2. Preserve the user's key decisions and preferences verbatim.\n\
             3. Note any errors that were encountered and how they were resolved.\n\
             4. Record the current task/goal if one is in progress.\n\
             5. Omit exploratory reads and tool output details — just note what was learned.\n\
             6. Use bullet points, not prose.\n\n\
             STRUCTURED FACTS (must appear in your summary):\n\
             {context}\n\n\
             Keep it under 300 words. Output only the summary, no preamble.",
        ))];

        // Flatten the dropped messages into a single user message
        // to avoid confusing the summarizer with tool_call/tool pairs.
        let mut text = String::new();
        for m in dropped {
            let role = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Tool => "Tool result",
                Role::System => "System",
            };
            let content = m.content.as_deref().unwrap_or("[no text]");
            text.push_str(&format!("{role}: {content}\n"));
        }
        messages.push(ChatMessage::user(text));

        let request = aegis_api::ChatRequest {
            model: model.to_string(),
            messages,
            tools: None,
            temperature: Some(0.0),
            max_tokens: Some(768),
            thinking: false,
            thinking_budget: 0,
        };

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match client.chat(&request).await {
                    Ok(response) => response
                        .choices
                        .first()
                        .and_then(|c| c.message.content.clone()),
                    Err(_) => None,
                }
            })
        })
    }
}

/// Extract structured context from dropped messages: which files were
/// read/edited, which tools were used, key user decisions, error
/// patterns, and shell commands. This gives the LLM summarizer
/// anchors to preserve critical information that would otherwise be
/// lost during compaction.
fn extract_compaction_context(messages: &[ChatMessage]) -> String {
    use std::collections::{HashMap, HashSet};

    let mut files_read: HashSet<String> = HashSet::new();
    let mut files_edited: HashSet<String> = HashSet::new();
    let mut files_written: HashSet<String> = HashSet::new();
    let mut tool_counts: HashMap<String, usize> = HashMap::new();
    let mut user_decisions: Vec<String> = Vec::new();
    let mut errors_resolved: Vec<String> = Vec::new();
    let mut shell_commands: Vec<String> = Vec::new();

    for msg in messages {
        // Extract key decisions from user messages — short messages
        // that confirm, reject, or redirect are high-signal.
        if msg.role == Role::User {
            if let Some(content) = msg.content.as_deref() {
                let trimmed = content.trim();
                let lower = trimmed.to_lowercase();
                // Capture explicit decisions: yes/no, confirmations,
                // "use X instead", "don't do Y", preference statements.
                let is_decision = lower.starts_with("yes")
                    || lower.starts_with("no,")
                    || lower.starts_with("no ")
                    || lower.starts_with("don't")
                    || lower.starts_with("do not")
                    || lower.starts_with("use ")
                    || lower.starts_with("keep ")
                    || lower.starts_with("skip ")
                    || lower.starts_with("instead ")
                    || lower.starts_with("prefer ")
                    || lower.starts_with("actually ")
                    || lower.contains("instead of")
                    || lower.contains("rather than")
                    || lower.contains("let's go with")
                    || lower.contains("sounds good")
                    || lower.contains("go ahead");
                if is_decision && trimmed.len() < 200 && user_decisions.len() < 5 {
                    user_decisions.push(trimmed.to_string());
                }
            }
        }

        // Extract error patterns from tool results — look for
        // error/failure indicators followed by resolution.
        if msg.role == Role::Tool {
            if let Some(content) = msg.content.as_deref() {
                let lower = content.to_lowercase();
                if (lower.contains("error") || lower.contains("failed") || lower.contains("panic"))
                    && errors_resolved.len() < 5
                {
                    // Keep first line as a compact error signature.
                    let first_line = content.lines().next().unwrap_or("").trim();
                    if !first_line.is_empty() && first_line.len() < 200 {
                        errors_resolved.push(first_line.to_string());
                    }
                }
            }
        }

        // Count tool calls from assistant messages
        for call in &msg.tool_calls {
            *tool_counts.entry(call.function.name.clone()).or_default() += 1;

            // Extract file paths from tool arguments
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) {
                // Extract paths from path or file_path fields
                let path_val = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .or_else(|| args.get("file_path").and_then(|v| v.as_str()));

                if let Some(path) = path_val {
                    match call.function.name.as_str() {
                        "read_file" => {
                            files_read.insert(path.to_string());
                        }
                        "edit_file" => {
                            files_edited.insert(path.to_string());
                        }
                        "write_file" => {
                            files_written.insert(path.to_string());
                        }
                        _ => {}
                    }
                }

                // Capture shell commands for bash tool calls
                if call.function.name == "bash" || call.function.name == "run_command" {
                    if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                        // Keep only short commands as context; long
                        // pipe chains are noise.
                        if cmd.len() < 120 && shell_commands.len() < 8 {
                            shell_commands.push(cmd.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut ctx = String::new();

    // Files edited/created are highest priority — they represent
    // actual workspace mutations that future turns may depend on.
    if !files_edited.is_empty() {
        let mut sorted: Vec<_> = files_edited.iter().collect();
        sorted.sort();
        ctx.push_str(&format!(
            "- Files edited: {}\n",
            sorted
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !files_written.is_empty() {
        let mut sorted: Vec<_> = files_written.iter().collect();
        sorted.sort();
        ctx.push_str(&format!(
            "- Files created: {}\n",
            sorted
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !files_read.is_empty() {
        let mut sorted: Vec<_> = files_read.iter().collect();
        sorted.sort();
        ctx.push_str(&format!(
            "- Files read: {}\n",
            sorted
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !user_decisions.is_empty() {
        ctx.push_str("- User decisions:\n");
        for d in &user_decisions {
            ctx.push_str(&format!("  * \"{d}\"\n"));
        }
    }

    if !errors_resolved.is_empty() {
        ctx.push_str("- Errors encountered:\n");
        for e in &errors_resolved {
            ctx.push_str(&format!("  * {e}\n"));
        }
    }

    if !shell_commands.is_empty() {
        ctx.push_str(&format!(
            "- Shell commands run: {}\n",
            shell_commands.join(" ; ")
        ));
    }

    if !tool_counts.is_empty() {
        let mut sorted: Vec<_> = tool_counts.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        let tools_str: Vec<String> = sorted
            .iter()
            .map(|(k, v)| format!("{}(×{})", k, v))
            .collect();
        ctx.push_str(&format!("- Tools used: {}\n", tools_str.join(", ")));
    }

    if ctx.is_empty() {
        "- No structured context extracted from dropped messages".to_string()
    } else {
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_api::{ChatMessage, ToolCall, ToolCallFunction};

    #[test]
    fn extract_context_finds_files_and_tools() {
        let messages = vec![ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".into(),
                    kind: "function".into(),
                    function: ToolCallFunction {
                        name: "read_file".into(),
                        arguments: r#"{"path":"src/main.rs"}"#.into(),
                    },
                },
                ToolCall {
                    id: "c2".into(),
                    kind: "function".into(),
                    function: ToolCallFunction {
                        name: "edit_file".into(),
                        arguments: r#"{"path":"src/lib.rs","old_string":"a","new_string":"b"}"#
                            .into(),
                    },
                },
            ],
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        }];
        let ctx = extract_compaction_context(&messages);
        assert!(ctx.contains("src/main.rs"), "should list read file");
        assert!(ctx.contains("src/lib.rs"), "should list edited file");
        assert!(ctx.contains("read_file"), "should list tool name");
        assert!(ctx.contains("edit_file"), "should list tool name");
    }

    #[test]
    fn extract_context_empty_messages() {
        let ctx = extract_compaction_context(&[]);
        assert!(ctx.contains("No structured context"));
    }

    fn assistant_with_tool_call(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
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
        }
    }

    #[test]
    fn below_trigger_is_noop() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.7,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
        ];
        let before = t.clone();
        maybe_compact(&mut t, 100, &cfg);
        assert_eq!(t.len(), before.len());
    }

    #[test]
    fn compacts_and_preserves_system_and_tail() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
            ChatMessage::user("u3"),
            ChatMessage::assistant_text("a3"),
        ];
        maybe_compact(&mut t, 900, &cfg);
        // Expected: [system, synthetic, u3, a3]
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].role, Role::System);
        assert_eq!(t[0].content.as_deref(), Some("sys"));
        assert_eq!(t[1].role, Role::System);
        assert!(t[1].content.as_ref().unwrap().contains("compacted"));
        assert_eq!(t[2].content.as_deref(), Some("u3"));
        assert_eq!(t[3].content.as_deref(), Some("a3"));
    }

    #[test]
    fn never_splits_tool_pair() {
        // keep_tail=2 would naively keep [tool, final-assistant], but
        // that dangles without its assistant-with-tool_calls. The
        // compactor should pull the assistant turn into the kept tail.
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            assistant_with_tool_call("call_1"),
            ChatMessage::tool_result("call_1", "read_file", "ok"),
            ChatMessage::assistant_text("done"),
        ];
        maybe_compact(&mut t, 900, &cfg);
        // The tool and its assistant turn must both be present.
        let has_assistant_with_call = t.iter().any(|m| !m.tool_calls.is_empty());
        let has_tool_reply = t.iter().any(|m| m.role == Role::Tool);
        assert!(
            has_assistant_with_call,
            "assistant-with-tool_calls dropped: {t:#?}"
        );
        assert!(has_tool_reply, "tool reply dropped: {t:#?}");
    }

    #[test]
    fn trigger_tokens_math() {
        let cfg = CompactionConfig {
            context_window: 10_000,
            trigger_ratio: 0.7,
            keep_tail: 4,
        };
        assert_eq!(cfg.trigger_tokens(), 7_000);
    }

    // ========================================================================
    // Session 16 — boundary and walk-back failure-driven coverage
    // ========================================================================

    /// Boundary: the trigger uses `<` (strictly less). Exactly at the
    /// threshold compaction MUST run; one token below it MUST NOT.
    /// State transitions probed:
    ///   1. transcript at length L, prompt = trigger - 1 → no-op (length L)
    ///   2. same transcript, prompt = trigger → compacted (length < L)
    ///
    /// Catches an off-by-one regression in `< vs <=`.
    #[test]
    fn boundary_at_trigger_minus_one_is_noop_at_trigger_compacts() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let trigger = cfg.trigger_tokens();
        assert_eq!(trigger, 500);

        let make_transcript = || {
            vec![
                ChatMessage::system("sys"),
                ChatMessage::user("u1"),
                ChatMessage::assistant_text("a1"),
                ChatMessage::user("u2"),
                ChatMessage::assistant_text("a2"),
                ChatMessage::user("u3"),
                ChatMessage::assistant_text("a3"),
            ]
        };

        // Just below threshold → no-op.
        let mut t = make_transcript();
        let original_len = t.len();
        maybe_compact(&mut t, trigger - 1, &cfg);
        assert_eq!(
            t.len(),
            original_len,
            "BOUNDARY: trigger-1 must not compact"
        );

        // Exactly at threshold → compact.
        let mut t = make_transcript();
        maybe_compact(&mut t, trigger, &cfg);
        assert!(
            t.len() < original_len,
            "BOUNDARY: exactly at trigger must compact (still len {})",
            t.len()
        );
        // And the synthetic system message must be present.
        let has_synthetic = t.iter().any(|m| {
            m.role == Role::System && m.content.as_deref().unwrap_or("").contains("compacted")
        });
        assert!(
            has_synthetic,
            "synthetic message missing after compact: {t:#?}"
        );
    }

    /// Branch coverage: when there is NO leading system prompt,
    /// `head_end = 0` and the synthetic message must land at index 0.
    /// Probes the `has_system = false` branch which is otherwise
    /// untested. Multi-step assertion: pre-state, post-state, and the
    /// position of the synthetic message.
    #[test]
    fn compacts_with_no_system_prompt_synthetic_lands_at_index_zero() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
            ChatMessage::user("u3"),
            ChatMessage::assistant_text("a3"),
        ];
        let pre_len = t.len();
        maybe_compact(&mut t, 900, &cfg);

        assert!(t.len() < pre_len, "compaction did not run");
        // Synthetic must be at index 0 because head_end = 0.
        assert_eq!(t[0].role, Role::System);
        assert!(
            t[0].content.as_deref().unwrap_or("").contains("compacted"),
            "first message was not the synthetic: {:?}",
            t[0]
        );
        // Tail must include the most recent assistant message.
        let last = t.last().unwrap();
        assert_eq!(last.content.as_deref(), Some("a3"));
    }

    /// Walk-back: when an assistant turn emits MULTIPLE tool_calls
    /// followed by multiple tool replies, the compactor must pull the
    /// assistant turn AND every tool reply belonging to it into the
    /// kept tail — even if the naive `keep_tail` window only covers the
    /// LAST of the tool replies. Otherwise the next request would
    /// contain orphan `tool` messages and the provider would 400.
    ///
    /// State transitions probed:
    ///   1. tail_start initially lands inside a multi-tool batch
    ///   2. walk-back pulls the second tool reply
    ///   3. walk-back pulls the first tool reply
    ///   4. walk-back lands on the assistant_with_tool_calls and stops
    ///
    /// Final assertion: kept tail starts with the assistant_with_calls.
    #[test]
    fn walk_back_pulls_full_multi_tool_batch_when_tail_lands_in_middle() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 1, // Force the naive window to land on the LAST message.
        };
        // Multi-tool batch: one assistant message, two tool replies.
        let assistant_two_calls = ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"a"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "c2".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"b"}"#.to_string(),
                    },
                },
            ],
            tool_call_id: None,
            name: None,
            protected: false,
            reasoning_content: None,
        };
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            assistant_two_calls,
            ChatMessage::tool_result("c1", "read_file", "alpha"),
            ChatMessage::tool_result("c2", "read_file", "beta"),
        ];
        maybe_compact(&mut t, 900, &cfg);

        // Result must contain BOTH tool replies and the assistant turn.
        let assistant_calls_count = t.iter().filter(|m| !m.tool_calls.is_empty()).count();
        let tool_replies_count = t.iter().filter(|m| m.role == Role::Tool).count();
        assert_eq!(
            assistant_calls_count, 1,
            "assistant_with_tool_calls dropped or duplicated: {t:#?}"
        );
        assert_eq!(
            tool_replies_count, 2,
            "expected both tool replies after walk-back, got {tool_replies_count}: {t:#?}"
        );
        // Order check: assistant_calls must come BEFORE both tool replies.
        let assistant_idx = t.iter().position(|m| !m.tool_calls.is_empty()).unwrap();
        let first_tool_idx = t.iter().position(|m| m.role == Role::Tool).unwrap();
        assert!(
            assistant_idx < first_tool_idx,
            "tool reply landed before its assistant turn: {t:#?}"
        );
    }

    // ========================================================================
    // micro_compact tests — failure-driven
    // ========================================================================

    /// Below micro threshold → no-op, nothing touched.
    #[test]
    fn micro_compact_below_threshold_is_noop() {
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("query"),
            ChatMessage::tool_result("c1", "read_file", "x".repeat(5000)),
        ];
        let original_len = t[2].content.as_ref().unwrap().len();
        let changed = maybe_micro_compact(&mut t, 100, 0.4, 1000, 2000);
        assert!(!changed, "must not change below threshold");
        assert_eq!(t[2].content.as_ref().unwrap().len(), original_len);
    }

    /// Above threshold, large tool output → gets trimmed to max_tool_chars + marker.
    #[test]
    fn micro_compact_trims_large_tool_output() {
        let big = "A".repeat(5000);
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("query"),
            ChatMessage::tool_result("c1", "bash", &big),
        ];
        let changed = maybe_micro_compact(&mut t, 900, 0.4, 1000, 2000);
        assert!(changed, "must trim above threshold");
        let content = t[2].content.as_ref().unwrap();
        // Original 5000 chars truncated to 2000 + marker
        assert!(content.len() < big.len(), "content was not shortened");
        assert!(
            content.contains("micro-compact: trimmed"),
            "trim marker missing: {content:.100}"
        );
        assert!(
            content.len() <= 2000 + 60,
            "content too long after trim: {}",
            content.len()
        );
    }

    /// Small tool outputs are never touched, even above threshold.
    #[test]
    fn micro_compact_leaves_small_tool_outputs_alone() {
        let small = "result ok".to_string();
        let mut t = vec![ChatMessage::tool_result("c1", "bash", &small)];
        let changed = maybe_micro_compact(&mut t, 999, 0.4, 1000, 2000);
        assert!(!changed, "small output should not be trimmed");
        assert_eq!(t[0].content.as_deref(), Some(small.as_str()));
    }

    /// Multiple large tool outputs → all get trimmed, changed=true.
    #[test]
    fn micro_compact_trims_multiple_tool_outputs() {
        let big = "B".repeat(3000);
        let mut t = vec![
            ChatMessage::tool_result("c1", "bash", &big),
            ChatMessage::tool_result("c2", "read_file", &big),
            ChatMessage::tool_result("c3", "bash", "tiny"),
        ];
        let changed = maybe_micro_compact(&mut t, 900, 0.4, 1000, 2000);
        assert!(changed);
        // c1 and c2 trimmed, c3 untouched
        assert!(t[0].content.as_ref().unwrap().contains("micro-compact"));
        assert!(t[1].content.as_ref().unwrap().contains("micro-compact"));
        assert!(!t[2].content.as_ref().unwrap().contains("micro-compact"));
    }

    /// Microcompact threshold (0.4) fires before full compaction threshold (0.55).
    /// At 45% usage: micro fires, full does not.
    #[test]
    fn micro_fires_before_full_compaction() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.55,
            keep_tail: 2,
        };
        let big = "C".repeat(3000);
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
            ChatMessage::tool_result("c1", "bash", &big),
        ];
        let pre_len = t.len();
        let tokens_at_45pct = 450u32;

        // Full compaction must NOT fire at 45%
        maybe_compact(&mut t, tokens_at_45pct, &cfg);
        assert_eq!(t.len(), pre_len, "full compaction should not fire at 45%");

        // Microcompact MUST fire at 45% (threshold 0.4 * 1000 = 400)
        let changed = maybe_micro_compact(&mut t, tokens_at_45pct, 0.4, 1000, 2000);
        assert!(changed, "microcompact must fire at 45%");
    }

    /// Already-trimmed message (contains "micro-compact:" marker) is not
    /// double-trimmed in a second pass — the marker adds chars so the
    /// re-check must account for it.
    #[test]
    fn micro_compact_does_not_double_trim_already_short_content() {
        // Content already at exactly max_tool_chars — a second pass
        // must not trim the marker itself (which would loop forever).
        let content_at_limit = "D".repeat(2000);
        let mut t = vec![ChatMessage::tool_result("c1", "bash", &content_at_limit)];
        let changed = maybe_micro_compact(&mut t, 900, 0.4, 1000, 2000);
        // Content is exactly at limit → not over → not trimmed
        assert!(!changed, "content at limit must not be trimmed");
    }

    /// Early-return guard: when transcript length is `keep_tail + 2` or
    /// less, compaction must be a no-op even if the token count is
    /// astronomical. Pins the second guard at `transcript.len() <=
    /// keep_tail + 2`.
    #[test]
    fn early_returns_when_transcript_at_keep_tail_plus_two_even_above_trigger() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        // keep_tail + 2 = 4 messages.
        let mut t = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
        ];
        let before = t.clone();
        maybe_compact(&mut t, 999_999, &cfg);
        assert_eq!(
            t.len(),
            before.len(),
            "guard tripped — compactor ran on too-small transcript"
        );
        for (a, b) in t.iter().zip(before.iter()) {
            assert_eq!(a.content, b.content);
            assert_eq!(a.role, b.role);
        }
    }

    // ---------------------------------------------------------------
    // Protected message preservation
    // ---------------------------------------------------------------

    fn protected_system(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::System,
            content: Some(content.to_string()),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            protected: true,
            reasoning_content: None,
        }
    }

    /// A protected system message inside the drop zone must survive compaction
    /// and be placed immediately after the main system prompt.
    #[test]
    fn protected_messages_survive_compaction() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::system("main sys"),
            protected_system("memory: pinned context"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
            ChatMessage::user("u3"),
            ChatMessage::assistant_text("a3"),
        ];
        maybe_compact(&mut t, 900, &cfg);

        // Protected message must still be present.
        let has_protected = t.iter().any(|m| {
            m.protected && m.content.as_deref() == Some("memory: pinned context")
        });
        assert!(has_protected, "protected message was dropped: {t:#?}");

        // It must appear after the system prompt (index 1) and before the tail.
        let protected_idx = t
            .iter()
            .position(|m| m.protected)
            .expect("protected message not found");
        assert_eq!(protected_idx, 1, "protected message should be at index 1 (after system)");

        // The synthetic compaction marker must also be present.
        let has_synthetic = t.iter().any(|m| {
            !m.protected
                && m.role == Role::System
                && m.content.as_deref().unwrap_or("").contains("compacted")
        });
        assert!(has_synthetic, "synthetic compaction message missing: {t:#?}");
    }

    /// Multiple protected messages all survive and land before the synthetic marker.
    #[test]
    fn multiple_protected_messages_all_survive() {
        let cfg = CompactionConfig {
            context_window: 1000,
            trigger_ratio: 0.5,
            keep_tail: 2,
        };
        let mut t = vec![
            ChatMessage::system("sys"),
            protected_system("pin1"),
            protected_system("pin2"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_text("a1"),
            ChatMessage::user("u2"),
            ChatMessage::assistant_text("a2"),
            ChatMessage::user("u3"),
            ChatMessage::assistant_text("a3"),
        ];
        maybe_compact(&mut t, 900, &cfg);

        let protected_count = t.iter().filter(|m| m.protected).count();
        assert_eq!(protected_count, 2, "both protected messages must survive: {t:#?}");
    }

    // ---------------------------------------------------------------
    // ctx-feature: blob-aware micro-compaction
    // ---------------------------------------------------------------

    #[cfg(feature = "ctx")]
    #[test]
    fn micro_compact_blob_replaces_oversized_tool_with_reference() {
        use crate::blob_index::BlobIndex;
        use crate::blob_store::BlobStore;

        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path()).unwrap();
        let index = BlobIndex::open(tmp.path()).unwrap();

        // Tool message with a body well above the cap.
        let mut t = vec![
            ChatMessage::user("hi"),
            ChatMessage::tool_result("1", "read_file", "X".repeat(8 * 1024)),
        ];

        let changed = super::maybe_micro_compact_with_blobs(
            &mut t, 999_999, 0.30, 10_000, 1024, &store, &index,
        );
        assert!(changed);
        let body = t[1].content.as_deref().unwrap_or("");
        assert!(body.starts_with("[micro-compact: stashed ctx://"));
        assert!(body.contains("8192 bytes"));
        // The blob actually landed.
        assert_eq!(store.iter_ids().unwrap().len(), 1);
        // And the index registered the doc. (We don't search here
        // because the body is just `X` repeated, which tokenizes as a
        // single oversized term — irrelevant for verifying that the
        // indexer was invoked.)
        assert_eq!(index.doc_count().unwrap(), 1);
    }

    #[cfg(feature = "ctx")]
    #[test]
    fn micro_compact_blob_skips_already_stashed_messages() {
        use crate::blob_index::BlobIndex;
        use crate::blob_store::BlobStore;

        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path()).unwrap();
        let index = BlobIndex::open(tmp.path()).unwrap();

        let already =
            "[stashed: ctx://abcdef0123456789 — 8192 bytes, 200 lines]\n--- preview ---\n…";
        let mut t = vec![
            ChatMessage::user("hi"),
            ChatMessage::tool_result("1", "read_file", already.to_string()),
        ];

        let changed = super::maybe_micro_compact_with_blobs(
            &mut t, 999_999, 0.30, 10_000, 8, // tiny threshold so length alone would trip it
            &store, &index,
        );
        // Already stashed → no double-stash, no second blob.
        assert!(!changed);
        assert_eq!(store.iter_ids().unwrap().len(), 0);
        assert_eq!(t[1].content.as_deref(), Some(already));
    }

    #[cfg(feature = "ctx")]
    #[test]
    fn micro_compact_blob_below_threshold_is_noop() {
        use crate::blob_index::BlobIndex;
        use crate::blob_store::BlobStore;

        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path()).unwrap();
        let index = BlobIndex::open(tmp.path()).unwrap();

        let mut t = vec![
            ChatMessage::user("hi"),
            ChatMessage::tool_result("1", "read_file", "X".repeat(8 * 1024)),
        ];

        let changed = super::maybe_micro_compact_with_blobs(
            &mut t, 10, // way below trigger
            0.30, 10_000, 1024, &store, &index,
        );
        assert!(!changed);
        assert_eq!(store.iter_ids().unwrap().len(), 0);
    }
}
