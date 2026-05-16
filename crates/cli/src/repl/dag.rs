//! DAG-style renderers for session transcripts and fork hierarchies.
//!
//! - [`format_dag`] renders the tool-call chain of a transcript as an
//!   ASCII tree (used by the `/dag` slash command).
//! - [`format_session_tree`] renders parent→child session fork lineage
//!   (used by `/sessions --tree`).
//!
//! Both are pure rendering helpers with no I/O; REPL slash handlers own
//! the data fetching and stderr emission.

use std::collections::HashMap;
use std::fmt::Write;

use aegis_api::{ChatMessage, Role};
use aegis_core::SessionSummary;

/// Render the tool-call chain of a session transcript as an ASCII tree.
///
/// Each assistant turn that issued tool_calls becomes a numbered node.
/// Its tool results (Role::Tool messages) are children. Multiple parallel
/// calls are shown as sibling branches under the same turn node.
///
/// ```text
/// turn 1  ── read_file {"path":"src/main.rs"}  ✓
/// turn 2  ┬─ edit_file {"path":"src/lib.rs"...  ✓
///          └─ bash {"command":"cargo build"}     ✓
/// turn 4  ── web_search {"query":"tokio docs"}   ✗ error
/// ```
pub(crate) fn format_dag(messages: &[ChatMessage]) -> String {
    struct CallNode {
        turn: usize,
        name: String,
        args: String,
        ok: bool,
    }

    // First pass: index tool results by tool_call_id.
    let mut results: HashMap<String, bool> = HashMap::new();
    for m in messages {
        if m.role == Role::Tool {
            let id = m.tool_call_id.clone().unwrap_or_default();
            let ok = m
                .content
                .as_deref()
                .map(|c| !c.to_lowercase().starts_with("error"))
                .unwrap_or(true);
            results.insert(id, ok);
        }
    }

    // Second pass: collect assistant turns with tool calls.
    let mut turn = 0usize;
    let mut nodes: Vec<Vec<CallNode>> = Vec::new();

    for m in messages {
        if m.role == Role::User || m.role == Role::Assistant {
            turn += 1;
        }
        if m.role == Role::Assistant && !m.tool_calls.is_empty() {
            let batch: Vec<CallNode> = m
                .tool_calls
                .iter()
                .map(|tc| {
                    let args_preview: String = tc.function.arguments.chars().take(55).collect();
                    let args_str = if tc.function.arguments.len() > 55 {
                        format!("{args_preview}…")
                    } else {
                        args_preview
                    };
                    let ok = results.get(&tc.id).copied().unwrap_or(true);
                    CallNode {
                        turn,
                        name: tc.function.name.clone(),
                        args: args_str,
                        ok,
                    }
                })
                .collect();
            nodes.push(batch);
        }
    }

    if nodes.is_empty() {
        return "  (no tool calls in this session)\n".to_string();
    }

    let mut out = String::new();
    for batch in &nodes {
        let n = batch.len();
        for (i, node) in batch.iter().enumerate() {
            let status = if node.ok { "✓" } else { "✗" };
            if n == 1 {
                out.push_str(&format!(
                    "  turn {:>2}  ── {:<18} {}  {}\n",
                    node.turn, node.name, node.args, status
                ));
            } else if i == 0 {
                out.push_str(&format!(
                    "  turn {:>2}  ┬─ {:<18} {}  {}\n",
                    node.turn, node.name, node.args, status
                ));
            } else if i == n - 1 {
                out.push_str(&format!(
                    "           └─ {:<18} {}  {}\n",
                    node.name, node.args, status
                ));
            } else {
                out.push_str(&format!(
                    "           ├─ {:<18} {}  {}\n",
                    node.name, node.args, status
                ));
            }
        }
    }
    out
}

/// Render a tree of sessions showing parent–child fork relationships.
/// The active session (if any) is highlighted with `*`.
pub(crate) fn format_session_tree(sessions: &[SessionSummary], current_id: Option<&str>) -> String {
    // Build parent → children map
    let mut children_map: HashMap<&str, Vec<&SessionSummary>> = HashMap::new();
    let mut roots: Vec<&SessionSummary> = Vec::new();

    for s in sessions {
        if let Some(ref pid) = s.parent_id {
            children_map.entry(pid.as_str()).or_default().push(s);
        } else {
            roots.push(s);
        }
    }

    // Sort roots by modified time (newest first, matching /sessions)
    roots.sort_by(|a, b| match (b.modified, a.modified) {
        (Some(bm), Some(am)) => bm.cmp(&am),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id),
    });

    let mut out = String::from("[goblin] session tree:\n");

    fn render_node(
        out: &mut String,
        s: &SessionSummary,
        children_map: &HashMap<&str, Vec<&SessionSummary>>,
        current_id: Option<&str>,
        prefix: &str,
        is_last: bool,
    ) {
        let connector = if prefix.is_empty() {
            ""
        } else if is_last {
            "└─ "
        } else {
            "├─ "
        };
        let marker = if current_id == Some(s.id.as_str()) {
            " *"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{prefix}{connector}{} ({} msgs){marker}",
            s.id, s.message_count
        );

        let child_prefix = if prefix.is_empty() {
            String::new()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };

        if let Some(kids) = children_map.get(s.id.as_str()) {
            for (i, kid) in kids.iter().enumerate() {
                let last = i == kids.len() - 1;
                render_node(out, kid, children_map, current_id, &child_prefix, last);
            }
        }
    }

    for (i, root) in roots.iter().enumerate() {
        let is_last = i == roots.len() - 1;
        render_node(&mut out, root, &children_map, current_id, "", is_last);
    }

    // Handle orphaned children (parent deleted but child remains)
    for s in sessions {
        if let Some(ref pid) = s.parent_id {
            if !sessions.iter().any(|x| x.id == *pid) {
                let marker = if current_id == Some(s.id.as_str()) {
                    " *"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "({pid}?) └─ {} ({} msgs){marker}",
                    s.id, s.message_count
                );
            }
        }
    }

    out
}
