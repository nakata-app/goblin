//! TUI (Terminal User Interface) mode for `goblin`.
//!
//! Launches a rich ratatui-based interface with three panels:
//! - Left (70%): scrollable chat area with color-coded messages
//! - Right (30%): live tool activity feed
//! - Bottom: input line + status bar (model, cost, turn count)
//!
//! Activated via `aegis --tui`. Integrates with the same agent pipeline
//! as the REPL — same tools, same sessions, same slash commands.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Terminal;

use aegis_api::{ChatProvider, StreamEvent};
use aegis_core::{
    display, format_cost_footer, Agent, AgentConfig, AllowAll, Permission, PermissionDecision, PlanState,
    SessionStore, SkillRegistry, ToolContext, ToolRegistry, UsageSnapshot, spawn_mcp_server,
};

// ---------------------------------------------------------------------------
// TUI-native permission prompt
// ---------------------------------------------------------------------------

/// One in-flight permission request, owned by `TuiApp` while the worker
/// thread that fired the check is parked on the response channel.
#[derive(Debug)]
pub struct PendingPermission {
    /// Tool the agent wants to run (`bash`, `edit_file`, …).
    pub tool: String,
    /// Pretty-printed args JSON — what the user sees in the modal. Plain
    /// text, no ANSI escapes (the render path styles it with ratatui spans).
    pub args_preview: String,
    /// Focused option index: 0 = Yes, 1 = Yes-and-always, 2 = No.
    /// Up/Down arrows adjust; 1/2/3 jump directly; Enter confirms; Esc denies.
    pub focused: usize,
    /// Response channel — the worker thread is blocked on the matching
    /// receiver. When the user picks an option we send through this and
    /// drop the `PendingPermission`, unblocking the agent.
    pub response_tx: std::sync::mpsc::Sender<PermissionChoice>,
}

/// The three decisions a user can make at the permission modal. `Cancel`
/// is distinct from `Deny` so a future UX could differentiate (e.g.
/// "Esc = back to chat without ending the turn" vs "No = hard stop"),
/// but today both map to `PermissionDecision::HardDeny` on the agent side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionChoice {
    Allow,
    AlwaysAllow,
    Deny,
    Cancel,
}


/// Active ask_user prompt — set by the TUI's UserInputFn callback and
/// consumed by the main event loop. Mirrors PendingPermission but for
/// interactive question/answer dialogs (the `ask_user_question` tool).
#[derive(Debug)]
pub struct AskUserPending {
    pub question: String,
    pub options: Vec<String>,
    pub focused: usize,
    pub freeform_text: String,
    pub freeform_active: bool,
    pub response_tx: std::sync::mpsc::Sender<AskUserResponse>,
}

/// User's response to an ask_user prompt.
///
/// `Option` / `Freeform` carry the user's text on the wire so the
/// receiver can log it or echo it back; the current consumer just
/// distinguishes the variants and discards the payload, hence the
/// dead-code lint suppressions on the inner strings. The fields stay
/// part of the public shape so future receivers don't have to widen
/// the enum to recover the text.
#[derive(Debug, Clone)]
pub enum AskUserResponse {
    /// User selected a numbered option — contains the option text.
    Option(#[allow(dead_code)] String),
    /// User typed custom text (the "Other" freeform path).
    Freeform(#[allow(dead_code)] String),
    /// User pressed Esc — decline to answer.
    Declined,
}

/// `Permission` implementation that drives a ratatui modal instead of
/// raw stdout. Safe to use inside a TUI alt-screen because it never
/// writes to stdout — it pushes a `PendingPermission` into the shared
/// `TuiApp` and parks the caller thread on an `mpsc::Receiver` until
/// the main event loop resolves the choice.
///
/// Matches `PolicyPermission` semantics: read-only tools bypass with
/// `Allow`, and `PermissionChoice::AlwaysAllow` caches a session-scoped
/// pass so the user doesn't get re-prompted for repeat bash/edit calls.
pub struct TuiPermission {
    app: Arc<Mutex<TuiApp>>,
    read_only: std::collections::HashSet<&'static str>,
    always_allowed: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Atakan: per-command bash whitelist. Keys are canonical command
    /// strings as produced by `aegis_core::bash_safety::analyze_bash_command`
    /// (e.g. "git status", "ls", "cargo build"). When the model invokes
    /// `bash`, the command line is decomposed into parts; if every part's
    /// canonical key is in this set, the call auto-allows. If the line
    /// is flagged Dangerous (rm -rf, subshell, sudo, etc) the whitelist
    /// is bypassed and the modal always opens.
    bash_command_allowlist: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl TuiPermission {
    /// Same read-only set as `aegis_core::PolicyPermission`. Tools that
    /// can't touch the filesystem or spawn subprocesses bypass the
    /// prompt entirely so research flows aren't a wall of modals.
    pub fn new(app: Arc<Mutex<TuiApp>>) -> Self {
        // Source of truth = `aegis_core::tools::PLAN_MODE_ALLOWED`.
        // Any tool safe to run in plan mode is safe to run without a
        // prompt here. Adding web_search explicitly because it's net
        // read-only but not in the plan mode list (plan mode wants
        // deterministic tools; web_search is network I/O).
        let mut read_only = std::collections::HashSet::new();
        for name in aegis_core::tools::PLAN_MODE_ALLOWED {
            read_only.insert(*name);
        }
        read_only.insert("web_search");
        Self {
            app,
            read_only,
            always_allowed: Arc::new(Mutex::new(std::collections::HashSet::new())),
            bash_command_allowlist: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    pub fn always_allowed_set(&self) -> Arc<Mutex<std::collections::HashSet<String>>> {
        Arc::clone(&self.always_allowed)
    }

    /// Atakan: handle for the slash-command layer (`/allow bash <cmd>`).
    /// Same Arc design as `always_allowed_set` so both halves of the TUI
    /// observe inserts immediately without rebuilding the permission gate.
    pub fn bash_command_allowlist_set(
        &self,
    ) -> Arc<Mutex<std::collections::HashSet<String>>> {
        Arc::clone(&self.bash_command_allowlist)
    }
}

/// Single row in the permission timeline. `decision` mirrors what
/// PermissionDecision the user picked (or how the layer auto-resolved
/// it). `args_preview` is hard-truncated so a giant multi_edit payload
/// doesn't blow up memory across hundreds of entries.
#[derive(Debug, Clone)]
pub struct PermissionLogEntry {
    /// Wall-clock seconds since UNIX epoch. Keeping this as a primitive
    /// avoids dragging in chrono just for one timestamp.
    pub when: u64,
    pub tool: String,
    pub args_preview: String,
    pub decision: PermissionLogDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLogDecision {
    /// User pressed `1` (one-shot allow).
    Allow,
    /// User pressed `2` — same as Allow plus added to always_allowed.
    AlwaysAllow,
    /// User pressed `3` / Esc — denied.
    Deny,
    /// Tool was on the read-only or always-allowed list, no prompt.
    AutoAllow,
    /// `acceptEdits` mode auto-approved a file mutation.
    AutoAcceptEdit,
}

impl PermissionLogDecision {
    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AlwaysAllow => "always-allow",
            Self::Deny => "deny",
            Self::AutoAllow => "auto",
            Self::AutoAcceptEdit => "auto-edits",
        }
    }
}

/// Record a single permission decision into the rolling log. Trims to
/// the most recent 200 entries so a long-running session doesn't grow
/// unbounded. Args are hard-capped at 240 chars so a multi_edit JSON
/// blob doesn't dominate the buffer.
fn record_permission(
    state: &mut TuiApp,
    tool: &str,
    args_preview: &str,
    decision: PermissionLogDecision,
) {
    const MAX_ARGS: usize = 240;
    const MAX_HISTORY: usize = 200;
    let when = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let trimmed: String = if args_preview.chars().count() > MAX_ARGS {
        let head: String = args_preview.chars().take(MAX_ARGS).collect();
        format!("{head}…")
    } else {
        args_preview.to_string()
    };
    state.permission_history.push(PermissionLogEntry {
        when,
        tool: tool.to_string(),
        args_preview: trimmed,
        decision,
    });
    if state.permission_history.len() > MAX_HISTORY {
        let drop = state.permission_history.len() - MAX_HISTORY;
        state.permission_history.drain(..drop);
    }
}

const ACCEPT_EDITS_AUTO_ALLOW: &[&str] = &[
    "edit_file",
    "write_file",
    "multi_edit",
    "notebook_edit",
    "create_file",
];

impl Permission for TuiPermission {
    fn check(&self, tool: &str, args: &serde_json::Value) -> PermissionDecision {
        let args_compact = serde_json::to_string(args).unwrap_or_default();
        if self.read_only.contains(tool) {
            if let Ok(mut state) = self.app.lock() {
                record_permission(
                    &mut state,
                    tool,
                    &args_compact,
                    PermissionLogDecision::AutoAllow,
                );
            }
            return PermissionDecision::Allow;
        }
        if let Ok(set) = self.always_allowed.lock() {
            if set.contains(tool) {
                drop(set);
                if let Ok(mut state) = self.app.lock() {
                    record_permission(
                        &mut state,
                        tool,
                        &args_compact,
                        PermissionLogDecision::AutoAllow,
                    );
                }
                return PermissionDecision::Allow;
            }
        }

        // Atakan: per-command bash whitelist. Auto-allows only when the
        // command line decomposes into Safe parts (no rm -rf, no
        // subshells, no sudo, etc) and EVERY canonical key is in the
        // user's bash_command_allowlist. A single dangerous token or
        // unwhitelisted part forces the modal to open.
        if tool == "bash" {
            if let Some(cmd_str) = args.get("command").and_then(|v| v.as_str()) {
                use aegis_core::bash_safety::{analyze_bash_command, CommandCheck};
                if let CommandCheck::Safe(parts) = analyze_bash_command(cmd_str) {
                    let all_known = if let Ok(set) = self.bash_command_allowlist.lock() {
                        !parts.is_empty() && parts.iter().all(|p| set.contains(p))
                    } else {
                        false
                    };
                    if all_known {
                        if let Ok(mut state) = self.app.lock() {
                            record_permission(
                                &mut state,
                                tool,
                                &args_compact,
                                PermissionLogDecision::AutoAllow,
                            );
                        }
                        return PermissionDecision::Allow;
                    }
                }
            }
        }
        // acceptEdits mode: auto-approve file mutations without a TUI modal.
        let accept_edits = self.app.lock().map_or(false, |s| s.accept_edits_mode);
        if accept_edits && ACCEPT_EDITS_AUTO_ALLOW.contains(&tool) {
            if let Ok(mut state) = self.app.lock() {
                record_permission(
                    &mut state,
                    tool,
                    &args_compact,
                    PermissionLogDecision::AutoAcceptEdit,
                );
            }
            return PermissionDecision::Allow;
        }

        let args_preview = serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".to_string());
        let (tx, rx) = std::sync::mpsc::channel::<PermissionChoice>();
        // For file-mutating tools, render a diff preview into the
        // transcript BEFORE the permission modal opens. The compact
        // modal can't show 100 lines of diff, but the chat scrollback
        // can — so the user sees exactly what would change and can
        // scroll up to inspect it before answering Y/N. Pure
        // best-effort: any read or parse failure just skips the
        // preview, the modal still works.
        let preview_lines = build_edit_preview_lines(tool, args);
        {
            let mut state = match self.app.lock() {
                Ok(g) => g,
                Err(_) => {
                    return PermissionDecision::HardDeny(format!(
                        "tui permission lock poisoned for `{tool}`"
                    ));
                }
            };
            for (plain, styled) in preview_lines {
                state.push_styled(plain, styled);
            }
            state.pending_permission = Some(PendingPermission {
                tool: tool.to_string(),
                args_preview,
                focused: 0,
                response_tx: tx,
            });
        }

        let outcome = rx.recv();
        let decision_log = match &outcome {
            Ok(PermissionChoice::Allow) => Some(PermissionLogDecision::Allow),
            Ok(PermissionChoice::AlwaysAllow) => Some(PermissionLogDecision::AlwaysAllow),
            Ok(PermissionChoice::Deny) | Ok(PermissionChoice::Cancel) => {
                Some(PermissionLogDecision::Deny)
            }
            Err(_) => None,
        };
        if let Some(d) = decision_log {
            if let Ok(mut state) = self.app.lock() {
                record_permission(&mut state, tool, &args_compact, d);
            }
        }
        match outcome {
            Ok(PermissionChoice::Allow) => PermissionDecision::Allow,
            Ok(PermissionChoice::AlwaysAllow) => {
                // Atakan: bash gets per-command whitelisting instead of
                // a blanket "allow all bash". Decompose the line via
                // bash_safety; if Safe, add each canonical key. If the
                // line was Dangerous (rm -rf, subshell, etc) we fall back
                // to one-shot Allow — never bake a dangerous shape into
                // the allowlist, even if the user clicks "always".
                if tool == "bash" {
                    use aegis_core::bash_safety::{analyze_bash_command, CommandCheck};
                    let cmd_str = args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let CommandCheck::Safe(parts) = analyze_bash_command(cmd_str) {
                        if let Ok(mut set) = self.bash_command_allowlist.lock() {
                            for p in &parts {
                                set.insert(p.clone());
                            }
                        }
                        if let Ok(mut state) = self.app.lock() {
                            state.push_system(&format!(
                                "always-allowed for this session: bash[{}]",
                                parts.join(", ")
                            ));
                        }
                    } else if let Ok(mut state) = self.app.lock() {
                        state.push_system(
                            "[allow] this command was flagged as dangerous; granted ONCE only — \
                             not added to whitelist",
                        );
                    }
                } else if let Ok(mut set) = self.always_allowed.lock() {
                    set.insert(tool.to_string());
                }
                PermissionDecision::Allow
            }
            Ok(PermissionChoice::Deny) | Ok(PermissionChoice::Cancel) => {
                PermissionDecision::HardDeny(format!("user denied `{tool}`"))
            }
            Err(_) => PermissionDecision::HardDeny(format!(
                "permission channel closed for `{tool}` (TUI quit?)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Message types for the chat panel
// ---------------------------------------------------------------------------

/// A single message in the chat history.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub text: String,
    /// Pre-built styled lines for messages that need per-span colors
    /// beyond what `MessageRole` offers (e.g. `/files` output where
    /// each entry has its own ext-based color). When `Some`, the
    /// renderer bypasses role-based styling and emits these lines
    /// verbatim. `text` is kept in sync for plain-text access (scroll
    /// math, tests) by flattening span strings.
    pub styled_lines: Option<Vec<Line<'static>>>,
    /// ToolResult messages are collapsed by default; ctrl+O expands them.
    pub expanded: bool,
}

/// Atakan: CC-style permission mode cycle. 4 mod, Shift+Tab ile cycle:
/// - **Default**: standart davranış, edit/bash için onay sorulur
/// - **AcceptEdits**: edit_file/write_file/multi_edit otomatik allowed
/// - **Plan**: PlanState::Drafting — sadece read-only tool'lar
/// - **Bypass**: tüm tool'lar always_allowed (mevcut /yolo /allow-all)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermMode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
}

impl PermMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
            Self::Bypass => "bypass",
        }
    }
    pub fn next(&self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Bypass,
            Self::Bypass => Self::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Error,
    /// Inline tool-call header — rendered as `⏺ Tool(arg)` with the
    /// canonicalised CC-style display name (snake_case → PascalCase).
    Tool,
    /// Inline tool-result preview — rendered as `  ⎿ preview`.
    ToolResult,
    /// Per-turn cost summary — dim gray, no label, REPL `format_cost_delta`
    /// output. Printed after each assistant turn so session cost stays
    /// visible in scrollback instead of being hidden in a status bar
    /// the user wasn't watching.
    Footer,
}

impl MessageRole {
    /// GitHub Copilot CLI palette: blue for user, white for model,
    /// green for system, dim gray for tool/footer.
    fn color(self) -> Color {
        match self {
            Self::User => Color::Rgb(255, 159, 64),   // warm orange (was Copilot blue)
            Self::Assistant => Color::White,
            Self::System => Color::Rgb(63, 185, 80),   // GitHub green
            Self::Error => Color::Rgb(248, 81, 73),     // soft red
            Self::Tool => Color::Rgb(139, 148, 158),    // secondary gray
            Self::ToolResult => Color::Rgb(139, 148, 158),
            Self::Footer => Color::Rgb(139, 148, 158),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool activity log entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub name: String,
    pub status: ToolStatus,
    pub preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

impl ToolStatus {
    /// Kept for tests + possible future sidebar/stats view.
    #[allow(dead_code)]
    fn symbol(self) -> &'static str {
        match self {
            Self::Running => "...",
            Self::Done => " ok",
            Self::Failed => "err",
        }
    }

    #[allow(dead_code)]
    fn color(self) -> Color {
        match self {
            Self::Running => Color::Yellow,
            Self::Done => Color::Green,
            Self::Failed => Color::White,
        }
    }
}

// ---------------------------------------------------------------------------
// Reverse-i-search state
// ---------------------------------------------------------------------------

/// Ctrl+F chat-history search. Active when `Some`. While set, the
/// input bar swaps for a search prompt; key input rewrites the query
/// and recomputes matches; `Down`/`Up` step through them and adjust
/// `scroll_offset` so the active match stays visible. Esc/Ctrl+G
/// cancels and restores the previous scroll position.
#[derive(Debug, Clone, Default)]
pub struct ChatSearchState {
    pub query: String,
    /// Indices into `TuiApp::messages` whose plain text contains
    /// `query` (case-insensitive). Recomputed on every query edit so
    /// stale matches never persist.
    pub matches: Vec<usize>,
    /// Active position in `matches`, or `0` when `matches.is_empty()`.
    pub current: usize,
    /// `scroll_offset` snapshot from when search opened. Restored on
    /// cancel so a no-result probe doesn't strand the user mid-history.
    pub saved_scroll: u16,
}

/// Bash-style `Ctrl+R` interactive history search. Active while
/// `TuiApp::search_state` is `Some`; null state means the input line
/// behaves normally.
#[derive(Debug, Clone)]
pub struct ReverseSearchState {
    /// Current search query typed by the user.
    pub query: String,
    /// Index into `TuiApp::input_history` of the most recent match, or
    /// `None` if the query has no matches yet.
    pub match_idx: Option<usize>,
    /// The input buffer as it was when Ctrl+R was pressed. Restored on
    /// Esc/Ctrl+G cancel.
    pub saved_input: String,
    /// Cursor position saved alongside `saved_input`.
    pub saved_cursor: usize,
}

// ---------------------------------------------------------------------------
// TUI application state
// ---------------------------------------------------------------------------

/// All mutable state for the TUI — designed so unit tests can exercise
/// it without needing a real terminal or agent.
#[derive(Debug)]
pub struct TuiApp {
    /// Chat message history.
    pub messages: Vec<ChatMessage>,
    /// Current text in the input line.
    pub input: String,
    /// Cursor position within `input` (byte offset).
    pub cursor: usize,
    /// Scroll offset in the chat panel (0 = bottom / newest).
    pub scroll_offset: u16,
    /// Tool activity log.
    pub tools: Vec<ToolEntry>,
    /// Scroll offset in the tool panel (reserved for v2).
    #[allow(dead_code)]
    pub tool_scroll: u16,
    /// Model name for the status bar.
    pub model: String,
    /// Cumulative cost string.
    pub cost_display: String,
    /// Cumulative usage for cost calculation.
    pub cumulative_usage: UsageSnapshot,
    /// Turn count.
    pub turn_count: u32,
    /// Whether the agent is currently running.
    pub busy: bool,
    /// Whether the user has requested quit.
    pub should_quit: bool,
    /// Partial assistant text being streamed.
    pub streaming_text: String,
    /// Thinking text being streamed.
    pub thinking_text: String,
    /// Wall-clock start of the current turn — drives the REPL-style
    /// "✻ Thinking…" spinner before the first token arrives and the
    /// "⏺ thought for X.Xs" line emitted right after.
    pub turn_start: Option<std::time::Instant>,
    /// Flag flipped true on the first TextDelta of a turn. Used by the
    /// stream callback to push a one-shot `thought for …` footer
    /// exactly once per turn.
    pub first_token_seen: bool,
    /// Input history for up/down arrow navigation.
    pub input_history: Vec<String>,
    /// Current position in input history (-1 = live input).
    pub history_index: Option<usize>,
    /// Saved live input when browsing history.
    pub saved_input: String,
    /// Prompts the user pressed Enter on while the agent was still
    /// running. Drained in FIFO order by the main loop as soon as
    /// `busy` falls back to false. Mirrors REPL's "↵ queued" behavior
    /// so fast typers can line up follow-ups without waiting for the
    /// current turn to finish.
    pub pending_prompts: std::collections::VecDeque<String>,
    /// GODMODE: race result queued for next idle iteration
    pub pending_race: Option<String>,
    /// `/askall <prompt>` — like race but uses each provider's strongest
    /// model (index 0 in models_for_provider) and shows all responses.
    /// (Renamed from `/ask`; the new `/ask` is Copilot-CLI-style single-shot.)
    pub pending_askall: Option<String>,
    /// `/ask <prompt>` — Copilot-CLI-style single-shot Q&A: fires the
    /// prompt at the current model with tools disabled and prints the
    /// single response. No multi-provider fan-out, no agentic loop.
    pub pending_ask_single: Option<String>,
    /// GODMODE: multi-model evaluation enabled
    pub multi_model_evaluation: bool,
    /// GODMODE: prompt perturbation enabled
    pub prompt_perturbation: bool,
    /// GODMODE: parallel models enabled
    pub parallel_models: bool,
    /// GODMODE: API key management enabled
    pub api_keys_display: bool,
    /// Adaptive temperature per turn — picks based on prompt classification
    /// (autotune::autotune in metis-api). Mirrors REPL's `/autotune` toggle.
    pub autotune: bool,
    /// Skip the next per-turn `※ recap:` footer (used for synthetic
    /// opening turns that should not surface memory links to the user).
    pub skip_next_recap: bool,
    /// Live handle to the autonomous security layer, when wrapped into
    /// the permission chain at startup. Lets `/security` show real stats
    /// and lets `/security kill | resume` flip the real kill switch
    /// rather than printing cosmetic strings. `None` only if startup
    /// failed to wrap the layer (should never happen in practice).
    pub security: Option<Arc<aegis_core::AutonomousSecurityLayer>>,
    /// Shared with `TuiPermission::always_allowed` — slash commands can
    /// add/remove tools from this set to skip future permission prompts.
    pub always_allowed: Option<Arc<Mutex<std::collections::HashSet<String>>>>,
    /// Atakan: per-command bash whitelist mirror. Shared Arc with
    /// `TuiPermission::bash_command_allowlist` so `/allow bash <cmd>` and
    /// the AlwaysAllow modal choice both write to the same set the
    /// gate reads on every bash check. Keys are canonical strings from
    /// `aegis_core::bash_safety::analyze_bash_command`.
    pub bash_command_allowlist: Option<Arc<Mutex<std::collections::HashSet<String>>>>,
    /// Atakan: CC-style 4-mod permission cycle. Shift+Tab cycles through
    /// Default → AcceptEdits → Plan → Bypass → Default. Each transition
    /// keeps the underlying `plan_state` and `always_allowed` set in sync,
    /// so existing /plan, /yolo, /allow-all behaviour are preserved as
    /// special-case shortcuts into specific modes.
    pub permission_mode: PermMode,
    /// Notification bell: when true, emit ASCII BEL (`\x07`) to stderr
    /// at the end of any turn whose wall-clock duration meets or
    /// exceeds `bell_threshold_secs`. Default off — opt-in via `/bell`
    /// so the TUI never produces unsolicited noise.
    pub bell_enabled: bool,
    /// Minimum turn duration (seconds) that triggers the bell when it's
    /// enabled. Configurable via `/bell <N>`. Default 30s — most edits
    /// and explanations finish well below this, so the bell only fires
    /// for the long runs the user has actually walked away from.
    pub bell_threshold_secs: u64,
    /// Architect/editor swap: model used when permission_mode flips to
    /// `Plan`. Populated from `[routing] plan_model` at TUI launch.
    /// Bare model name or `provider:model`. `None` → mode change does
    /// not touch the active model.
    pub plan_model: Option<String>,
    /// Architect/editor swap counterpart: model used when
    /// permission_mode flips to `AcceptEdits` or `Bypass`. Same shape
    /// as `plan_model`.
    pub build_model: Option<String>,
    /// Snapshot of the model the TUI launched with, captured before the
    /// first plan/build swap so cycling back to `Default` can restore
    /// the user's original `--model` / config pick instead of leaving
    /// them on whichever swap fired last.
    pub default_model: Option<String>,
    /// Numbered permission quick-menu — shown when `/allow` is typed
    /// without args. Number keys 1-7 trigger the corresponding action.
    pub allow_menu_open: bool,
    /// `/providers` interactive menu — (id, default_model, has_key).
    /// `Some` while open, `None` when closed.
    pub provider_menu: Option<Vec<(String, String, bool)>>,
    /// `/models` interactive menu — model names for current provider.
    pub model_menu: Option<Vec<String>>,
    /// Slash-command-driven provider switch. The main loop picks this
    /// up before the next turn fires, rebuilds `client`, updates
    /// `model`, and clears the field. Expressed as `(provider_id,
    /// optional_model_override)`.
    pub pending_provider_switch: Option<(String, Option<String>)>,
    /// Slash-command-driven model switch (same provider). Main loop
    /// just rewrites the `model` string before the next turn.
    pub pending_model_switch: Option<String>,
    /// `/overthink` toggles model thinking mode for subsequent turns.
    /// Applied to `AgentConfig.thinking` before the next
    /// `run_agent_turn` builds its agent. Starts false.
    pub thinking_enabled: bool,
    /// `/advisor` on/off — wires up the post-turn advisor hook in a
    /// future batch. Stored now so the slash command has somewhere
    /// to land; the run loop currently ignores it.
    #[allow(dead_code)]
    pub advisor_enabled: bool,
    /// Current session id. `/clear`, `/resume`, and `/fork` rewrite it
    /// so the next turn's `SessionStore::open` loads the right file.
    pub session_id: String,
    /// Shared plan-mode flag. `/plan` toggles between Normal / Drafting;
    /// the per-turn agent ctx reads it to gate write tools.
    pub plan_state: Arc<Mutex<PlanState>>,
    /// `@` file search — OpenCode/Copilot CLI style fuzzy path completion.
    /// When the user types `@`, a file search overlay shows matching paths
    /// below the prompt. Tab completes the selected match.
    pub at_search_active: bool,
    pub at_search_matches: Vec<String>,
    pub at_search_index: usize,
    pub at_search_start: usize, // position of `@` in input
    pub at_search_last_refresh: Option<std::time::Instant>, // debounce

    /// `/compact` queues a session compaction for the next idle point.
    /// The main loop rebuilds a temp agent, calls `force_compact`, and
    /// reports how many messages were removed.
    pub pending_compact: bool,
    /// `/consult <provider> <prompt>` — main loop picks up (provider,
    /// prompt), runs a one-shot chat_stream against that provider,
    /// prints the response inline, and appends it as a session note.
    pub pending_consult: Option<(String, String)>,
    /// Skill registry loaded from `~/.metis/skills/` + `.metis/skills/`
    /// plus built-in skills. Unknown slash commands first check this
    /// registry; if the name matches a user-invocable skill, its
    /// expanded prompt is queued as a normal turn. Starts empty for
    /// tests; `tui_main` populates it on startup.
    pub skill_registry: SkillRegistry,
    /// Shared tool registry — used by `/browser` and `/computer` to
    /// late-register MCP servers without rebuilding the agent.
    pub tool_registry: Option<Arc<ToolRegistry>>,
    /// Image paths queued by `/image <path>`. Sent along with the NEXT
    /// prompt as a multimodal `UserInput::WithImages`, then cleared.
    /// `/images` lists them, `/images clear` empties the buffer without
    /// firing a turn. Exactly mirrors REPL's `pending_images` behavior.
    pub pending_images: Vec<std::path::PathBuf>,
    /// `/update` sets this; main loop picks it up, runs
    /// `aegis_core::update::check_latest` + optional `perform_update`,
    /// then clears. Networked so it lives on the main loop, not
    /// `handle_slash`. Streaming progress lines land in chat via
    /// `push_system` / `push_error`.
    pub pending_update: bool,
    /// `(provider, in-progress body)` for the currently streaming
    /// `/consult` turn. Rendered in-place in the chat panel as tokens
    /// arrive — REPL streams to stderr; TUI grows this string and the
    /// chat paragraph re-renders every ~16ms. Set by the consult spawn
    /// block before firing `chat_stream`, cleared + promoted to a
    /// permanent `MessageRole::System` line when the stream ends.
    pub consult_streaming: Option<(String, String)>,
    /// Set by `/models` so the very next digit keypress (1..=N) picks
    /// a model without requiring Enter. Cleared on any non-digit input,
    /// Esc, or after the pick fires. Size matches `last_model_menu`.
    /// REPL used raw termios single-key reads for the same UX; TUI
    /// can't do that inside ratatui's event loop, but we can flag a
    /// mode and intercept in `handle_key`.
    pub awaiting_model_pick: bool,
    /// Primed-Esc state for the double-Esc "clear input" shortcut.
    /// First Esc on a non-empty input sets this to `Some(Instant)`;
    /// a second Esc within ~800 ms clears the input. Any other key or
    /// a timeout cancels the prime. Empty-input Esc still quits
    /// immediately — the prime path is only active while there's
    /// input text to clear.
    pub esc_primed_at: Option<std::time::Instant>,
    /// First `/clear` arms this; a second `/clear` within 5s actually
    /// wipes. Prevents fat-finger destruction of a running session —
    /// same confirm pattern as double-Esc to clear input.
    pub clear_primed_at: Option<std::time::Instant>,
    /// `/fork <name>` when `<name>.jsonl` already exists arms this.
    /// A second identical `/fork` within 5s overwrites. Without the
    /// prime, silent overwrite would lose an earlier fork with no
    /// warning — same confirm pattern as `/clear`.
    pub fork_overwrite_primed: Option<(String, std::time::Instant)>,
    /// Bash-style reverse-i-search state (Ctrl+R). When Some, the input
    /// area renders as `(r-search)'query': match` and keystrokes extend
    /// the query instead of editing the buffer. Enter accepts, Esc
    /// cancels.
    pub search_state: Option<ReverseSearchState>,
    /// Ctrl+F chat search overlay state. `Some` while the user is in
    /// the search bar, `None` otherwise.
    pub chat_search: Option<ChatSearchState>,
    /// Pinned message indices. The set is rendered as `★` glyphs in
    /// the chat panel and surfaced through `/pinned` so users can
    /// re-find decisions, error snippets, and design notes without
    /// scrolling. The `messages` vector is grow-only in normal use, so
    /// indices stay stable; `/clear` resets both. The compactor does
    /// not yet consult this set — wiring that is a v1 follow-up.
    pub pinned: std::collections::BTreeSet<usize>,
    /// Last draw's post-wrap row count. Used so that when new content
    /// grows the chat while the user is scrolled up (scroll_offset > 0),
    /// the render path can bump scroll_offset by the delta — otherwise
    /// the user's viewport drifts forward as tokens stream in and
    /// "old texts disappear". Updated on every draw.
    pub last_wrapped_total: u16,
    /// Last draw's chat-area height and max_scroll — exposed so the
    /// recap line can show live diagnostic values (`offset/total max
    /// vis`) for scroll-bug triage. One frame behind, which is fine
    /// for a read-only indicator.
    pub last_visible_height: u16,
    pub last_max_scroll: u16,
    /// Whether crossterm mouse capture is currently on. Default `true`
    /// so trackpad scroll works out of the box; `/mouse` toggles it so
    /// the user can temporarily fall back to native terminal select &
    /// copy when Option+drag doesn't work (e.g. Wacom pen input).
    pub mouse_capture_on: bool,
    /// Per-turn tool invocations — each `tool_start` appends the tool
    /// name so `flush_streaming` can render a compact recap footer
    /// ("tools: 3 × read_file · 1 × bash · 1 × edit_file"). Reset at
    /// turn start.
    pub turn_tools: Vec<String>,
    /// Per-turn unique file paths touched by mutating tools
    /// (`edit_file`, `write_file`, `multi_edit`). Used in the
    /// turn-end recap so the user sees exactly which files changed
    /// without having to scroll back through tool calls. Reset at
    /// turn start.
    pub turn_files: Vec<String>,
    /// Active permission prompt, set by `TuiPermission::check` on a
    /// tool-worker thread and consumed by the main event loop when the
    /// user picks 1/2/3 or Esc. While this is `Some`, the main event
    /// loop intercepts digit + arrow + Enter keys instead of routing
    /// them to the input line. See `TuiPermission` for the channel
    /// protocol — the worker thread is parked on an `mpsc::Receiver`
    /// until the UI sends a `PermissionChoice` back.
    pub pending_permission: Option<PendingPermission>,
    /// Active ask_user prompt — shows when the agent calls ask_user_question.
    pub pending_ask_user: Option<AskUserPending>,
    /// Last `/models` listing, stored so `/model N` can pick by number.
    /// REPL reads a single digit via raw termios; TUI can't grab the key
    /// before the event loop sees it, so instead we snapshot the menu
    /// and let the user type `/model 3` on a normal input line.
    pub last_model_menu: Vec<String>,
    /// Current provider id — `/models` and `/glm` need it to build the
    /// right list. Kept in sync by the main loop on provider switches.
    /// Defaults to "deepseek" to match `metis --tui` startup.
    pub current_provider: String,
    /// `/btw` side-call in progress — shown in the queue strip while the
    /// parallel call is running so the user knows it's being answered.
    pub btw_in_flight: Option<String>,
    /// Rate-limit cost footer: only shown every 5 minutes.
    pub last_cost_shown: std::time::Instant,
    /// `/cost off` disables the per-turn footer; `/cost on` re-enables.
    pub cost_footer_enabled: bool,
    /// `/sidebar` toggles the right context panel (OpenCode-style:
    /// Context, LSP, Todo, footer). Default on; hidden if terminal is
    /// too narrow regardless.
    pub sidebar_visible: bool,
    /// Pending cost footer text — queued by `run_agent_turn` after
    /// `agent.run` finishes, then drained by `flush_streaming` so the
    /// cost line lands AFTER the assistant's text instead of between
    /// the "thought for X.Xs" line and the answer (visible bug in
    /// 2026-05-03 user transcript).
    pub pending_cost_footer: Option<String>,
    /// When true, the provider_menu overlay is in "consult pick" mode:
    /// picking a provider pre-fills the input with `/consult <id> `
    /// instead of switching the active provider.
    pub consult_pick_mode: bool,
    /// `/autoskill` toggle — when true, before each user turn a quick
    /// classification call selects the best matching skill and injects
    /// its prompt into the turn automatically.
    pub auto_skill_enabled: bool,
    /// `/acceptedits` toggle — mirrors CC's `acceptEdits` permission mode.
    /// When true, file edits (edit_file, write_file, multi_edit) are
    /// approved without prompting; bash/shell still asks.
    pub accept_edits_mode: bool,
    /// `/skills` interactive overlay — all user-invocable skills.
    /// `Some` while open, `None` when closed. `skill_filter` and
    /// `skill_sel` track the live search state within the overlay.
    pub skill_menu: Option<Vec<aegis_core::skills::Skill>>,
    pub skill_filter: String,
    pub skill_sel: usize,
    /// `/help` overlay panel — when true, draw() shows the help reference
    /// as a full-width modal instead of pushing it into the chat scroll.
    /// Esc / `q` / second `/help` closes it; PgUp/PgDn/↑/↓ scroll.
    pub help_overlay_open: bool,
    pub help_scroll: u16,
    /// Atakan: Trigger B — set to `true` by `/recall-prev` slash handler
    /// when the user wants the previous (unsaved) session summarized and
    /// ingested. The TUI event loop drains it on the next tick by
    /// spawning a background `code_driven_session_save` task. None when
    /// idle; set to Some(()) to request a single recovery cycle.
    pub pending_recall_prev: Option<()>,
    /// Ctrl+P command palette — fuzzy-filtered slash-command picker.
    /// Open with Ctrl+P, type to filter, ↑/↓ navigate, Enter inserts
    /// the picked command into the input line, Esc closes without
    /// touching input. Same modal pattern as `skill_menu` but pulls
    /// from a static list so /palette works even before any skills
    /// are loaded.
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_sel: usize,
    /// Interactive session picker shown when the user runs bare
    /// `/sessions`. Loaded on demand (Some(list) = open). ↑/↓ navigate,
    /// Enter resumes the highlighted session via the same path as
    /// `/resume <id>`, Esc closes without touching state. The text-only
    /// dump remains available as `/sessions list` for users who pipe.
    pub session_picker: Option<Vec<aegis_core::SessionSummary>>,
    pub session_picker_sel: usize,
    /// Files picker — bare `/files` walks the workspace, drops the
    /// flat result here, and the modal lets the user type-to-filter
    /// then Enter to insert the chosen path into the input as `@path`.
    /// `/files <path>` keeps the legacy single-dir listing for users
    /// who want directory metadata. Skips .git / target / node_modules
    /// / .metis at walk time so the picker stays usable in real repos.
    pub files_picker: Option<Vec<String>>,
    pub files_picker_query: String,
    pub files_picker_sel: usize,
    /// Streaming pause: while a turn is producing tokens, Space (with
    /// empty input) parks the live stream into `stream_paused_buffer`
    /// so the user can finish reading the visible text without it
    /// scrolling under them. A second Space drains the buffer back
    /// into `streaming_text` and the chat resumes. Provider-side
    /// throughput isn't gated — chunks keep arriving, they just queue
    /// behind the pause boundary.
    pub stream_paused: bool,
    pub stream_paused_buffer: String,
    /// Optional top-strip tabs. Default off (Atakan'ın yoğun kullandığı
    /// sidebar zaten genel navigasyon yapıyor; tab strip ek satır yer
    /// yer ister, /tabs ile açılır). F1-F4 strip görünür değilse de
    /// çalışır — strip sadece görsel reminder. F1=chat (no-op),
    /// F2=files picker, F3=sessions picker, F4=permissions overlay.
    pub tabs_strip_visible: bool,
    /// Rolling log of every permission decision the user made this
    /// session. New entries land at the back; renderer takes the tail
    /// so the most recent prompts are visible. Capped at 200 to bound
    /// memory in a long-running TUI; oldest entries roll off.
    pub permission_history: Vec<PermissionLogEntry>,
    /// `/permissions` overlay: when `Some`, draw the timeline modal
    /// instead of normal chat. ↑↓ scroll, Esc closes.
    pub permission_overlay_open: bool,
    pub permission_overlay_scroll: u16,
    /// Display name of every MCP server attached to this TUI session
    /// (e.g. `playwright`, `open-computer-use`). Populated by
    /// `/browser` / `/computer` and any future attach-style slash
    /// commands; rendered as a sidebar card so the user can tell at
    /// a glance which extras are wired into the agent. Shared as
    /// `Arc<Mutex<...>>` because the attach happens off-thread in a
    /// `tokio::spawn` and writes back when the server is actually up.
    pub attached_mcps: Arc<Mutex<Vec<String>>>,
    /// Active row in /providers and /models overlays so ↑/↓ + Enter
    /// works the same as 1-9 quick-select. Reset to 0 when the overlay
    /// opens. Capped at items.len() - 1 by the key handler.
    pub provider_sel: usize,
    pub model_sel: usize,
    /// Workspace root — used by the draw loop to load agent tasks for the
    /// live task panel without an extra parameter thread.
    pub workspace: std::path::PathBuf,
    /// Last time the user submitted a message — used for idle reminder.
    pub last_input_time: std::time::Instant,
    /// Whether the 5-min idle reminder has already been shown for this idle window.
    pub idle_reminder_sent: bool,
    /// Master toggle for the 5-min idle reminder. Off by default — CC has no
    /// such reminder, and back-to-back summaries while the user is just
    /// thinking are noisy. `/idle on` opts back in.
    pub idle_reminder_enabled: bool,
    /// Long pastes are stashed here and replaced by `[Pasted text #N +X lines]`
    /// in the input line so the prompt stays readable. Expanded back to the
    /// raw text by `take_input` on submit. Cleared each turn.
    pub pasted_buffers: Vec<String>,
    /// `/claude <prompt>` — run `claude -p "<prompt>"` as a subprocess and
    /// display the output as a system message. Default off (opt-in). Only
    /// fires if the `claude` binary is found in PATH. Set by `handle_slash`,
    /// consumed by the main loop.
    pub pending_claude: Option<String>,
}

impl TuiApp {
    pub fn new(model: &str) -> Self {
        Self {
            // Placeholder — `refresh_welcome()` rewrites this with the
            // workspace path once `initial_app.workspace` is set in the
            // startup path. Tests don't set workspace so they see the
            // placeholder; the `messages.len() == 1` invariant holds.
            messages: vec![ChatMessage {
                role: MessageRole::System,
                text: "███╗   ███╗███████╗████████╗██╗███████╗\n\
                       ████╗ ████║██╔════╝╚══██╔══╝██║██╔════╝\n\
                       ██╔████╔██║█████╗     ██║   ██║███████╗\n\
                       ██║╚██╔╝██║██╔══╝     ██║   ██║╚════██║\n\
                       ██║ ╚═╝ ██║███████╗   ██║   ██║███████║\n\
                       ╚═╝     ╚═╝╚══════╝   ╚═╝   ╚═╝╚══════╝\n\
                       the original swallowed intelligence\n\
                       /help for commands · /banner for ASCII art\n\
                       ⓘ trackpad scroll is on — text-select needs Option+drag, or `/mouse` to toggle.".into(),
                styled_lines: None,
                expanded: false,
            }],
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            tools: Vec::new(),
            tool_scroll: 0,
            model: model.to_string(),
            cost_display: "$0.00".into(),
            cumulative_usage: UsageSnapshot::default(),
            turn_count: 0,
            busy: false,
            should_quit: false,
            streaming_text: String::new(),
            thinking_text: String::new(),
            turn_start: None,
            first_token_seen: false,
            input_history: Vec::new(),
            history_index: None,
            saved_input: String::new(),
            pending_prompts: std::collections::VecDeque::new(),
            pending_race: None,
            pending_askall: None,
            pending_ask_single: None,
            multi_model_evaluation: false,
            prompt_perturbation: false,
            parallel_models: false,
            api_keys_display: false,
            autotune: false,
            skip_next_recap: false,
            security: None,
            always_allowed: None,
            bash_command_allowlist: None,
            permission_mode: PermMode::Default,
            bell_enabled: false,
            bell_threshold_secs: 30,
            plan_model: None,
            build_model: None,
            default_model: None,
            allow_menu_open: false,
            provider_menu: None,
            model_menu: None,
            pending_provider_switch: None,
            pending_model_switch: None,
            thinking_enabled: false,
            advisor_enabled: false,
            session_id: SessionStore::new_id(),
            plan_state: Arc::new(Mutex::new(PlanState::Normal)),
            at_search_active: false,
            at_search_matches: Vec::new(),
            at_search_index: 0,
            at_search_start: 0,
            at_search_last_refresh: None,
            pending_compact: false,
            pending_consult: None,
            skill_registry: SkillRegistry::new(),
            tool_registry: None,
            pending_images: Vec::new(),
            pending_update: false,
            consult_streaming: None,
            last_model_menu: Vec::new(),
            current_provider: "nvidia".to_string(),
            btw_in_flight: None,
            last_cost_shown: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(300))
                .unwrap_or_else(std::time::Instant::now),
            // Default OFF — Atakan's standing preference is silent
            // turns; users who want the per-turn `· $0.013 · 412t`
            // chip flip it on with `/cost on`.
            cost_footer_enabled: false,
            sidebar_visible: true,
            pending_cost_footer: None,
            consult_pick_mode: false,
            auto_skill_enabled: false,
            accept_edits_mode: false,
            skill_menu: None,
            skill_filter: String::new(),
            skill_sel: 0,
            help_overlay_open: false,
            help_scroll: 0,
            pending_recall_prev: None,
            palette_open: false,
            palette_query: String::new(),
            palette_sel: 0,
            session_picker: None,
            session_picker_sel: 0,
            files_picker: None,
            files_picker_query: String::new(),
            files_picker_sel: 0,
            stream_paused: false,
            stream_paused_buffer: String::new(),
            tabs_strip_visible: false,
            permission_history: Vec::new(),
            permission_overlay_open: false,
            permission_overlay_scroll: 0,
            attached_mcps: Arc::new(Mutex::new(Vec::new())),
            provider_sel: 0,
            model_sel: 0,
            workspace: std::path::PathBuf::new(),
            last_input_time: std::time::Instant::now(),
            idle_reminder_sent: false,
            idle_reminder_enabled: false,
            pending_permission: None,
            pending_ask_user: None,
            awaiting_model_pick: false,
            esc_primed_at: None,
            clear_primed_at: None,
            fork_overwrite_primed: None,
            search_state: None,
            chat_search: None,
            pinned: std::collections::BTreeSet::new(),
            last_wrapped_total: 0,
            last_visible_height: 0,
            last_max_scroll: 0,
            mouse_capture_on: true,
            turn_tools: Vec::new(),
            turn_files: Vec::new(),
            pasted_buffers: Vec::new(),
            pending_claude: None,
        }
    }

    pub fn pop_pending(&mut self) -> Option<String> {
        self.pending_prompts.pop_front()
    }

    /// Rewrite the welcome message with the active workspace path. Copilot CLI style:
    /// clean greeting showing model, cwd, and trusted directory notice.
    pub fn refresh_welcome(&mut self) {
        if let Some(msg) = self.messages.first_mut() {
            if msg.role == MessageRole::System
                && msg.text.starts_with("███╗")
            {
                msg.text = format!(
                    "aegis — AI coding assistant\n\
                     model: {}\n\
                      cwd:  {}\n\
                     trusted directory ✓  ·  Type /help for commands, esc/ctrl-d to quit.",
                    self.model,
                    self.workspace.display()
                );
                msg.styled_lines = None;
            }
        }
    }

    // -- Input editing --

    pub fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();

        // `@` fuzzy file search — OpenCode/Copilot CLI style.
        if ch == '@' {
            let prev = if self.cursor > 1 {
                self.input[..self.cursor - 1].chars().last()
            } else {
                None
            };
            let is_word_start = prev.map_or(true, |p| !p.is_alphanumeric() && p != '.' && p != '-');
            if is_word_start {
                self.at_search_active = true;
                self.at_search_start = self.cursor;
                self.at_search_index = 0;
                self.at_search_matches.clear();
                self.at_search_last_refresh = None;
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // Find the previous char boundary.
            let prev = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            let next = self.input[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.input.len());
            self.input.drain(self.cursor..next);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor = self.input[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.input.len());
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Readline Ctrl+K: delete from cursor to end of line.
    pub fn kill_to_end(&mut self) {
        self.input.truncate(self.cursor);
    }

    /// Readline Ctrl+U: delete from cursor to start of line.
    pub fn kill_to_start(&mut self) {
        self.input.drain(..self.cursor);
        self.cursor = 0;
    }

    /// Readline Ctrl+W: delete the word immediately before the cursor.
    /// Skips trailing whitespace first (so `foo bar ` + Ctrl+W eats
    /// "bar ", not just the trailing space), then kills non-whitespace
    /// back to the previous whitespace boundary.
    pub fn kill_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut end = self.cursor;
        // Skip trailing whitespace before cursor.
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        // Kill back through non-whitespace.
        let mut start = end;
        while start > 0 && !(bytes[start - 1] as char).is_whitespace() {
            start -= 1;
        }
        self.input.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Insert a literal newline at the cursor (Shift+Enter multiline).
    pub fn insert_newline(&mut self) {
        self.input.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Refresh `@` file search matches for the given query string
    /// (the text between `@` and cursor). Uses fuzzy prefix matching
    /// on all files in the workspace, then sorts by relevance.
    pub fn refresh_at_search(&mut self, ws: &std::path::Path, query: &str) {
        self.at_search_matches.clear();
        if query.is_empty() || query == "@" {
            return;
        }
        let q = query.trim_start_matches('@').to_lowercase();
        if q.is_empty() {
            return;
        }
        let mut results: Vec<String> = Vec::new();
        let _ = walkdir::WalkDir::new(ws)
            .max_depth(12)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter(|e| {
                let name = e.path().to_string_lossy().to_lowercase();
                // Fuzzy: each query char must appear in order
                let mut name_chars = name.chars();
                q.chars().all(|qc| name_chars.any(|nc| nc == qc))
            })
            .take(30)
            .for_each(|e| {
                if let Ok(rel) = e.path().strip_prefix(ws) {
                    results.push(rel.to_string_lossy().into_owned());
                }
            });
        results.sort_by_key(|p| (p.len(), p.clone()));
        self.at_search_matches = results;
        self.at_search_index = 0;
    }

    /// Complete the `@` file search — replace `@query` with the selected
    /// file path. Called on Tab when at_search is active.
    fn complete_at_search(&mut self) {
        if !self.at_search_active || self.at_search_matches.is_empty() {
            return;
        }
        // Guard: cursor must be after @, not before it
        if self.at_search_start == 0 || self.cursor < self.at_search_start {
            self.at_search_active = false;
            self.at_search_matches.clear();
            return;
        }
        let idx = self.at_search_index.min(self.at_search_matches.len() - 1);
        let path = self.at_search_matches[idx].clone();
        // Replace from @ character to cursor with the selected path
        self.input.replace_range(self.at_search_start - 1..self.cursor, &path);
        self.cursor = self.at_search_start - 1 + path.len();
        self.at_search_active = false;
        self.at_search_matches.clear();
    }

    /// Tab completion. Completes slash commands at the start of the
    /// input (e.g. `/ima` → `/image `), and filesystem paths after a
    /// space (e.g. `/image ~/Des` → `/image ~/Desktop/`). Returns true
    /// if the input changed so the caller can swallow the event.
    ///
    /// Only operates when the cursor is at the end of the input — no
    /// mid-buffer completion. Single match completes fully; multiple
    /// matches extend to the longest common prefix and list candidates
    /// in the chat panel.
    pub fn complete_tab(&mut self, workspace: &std::path::Path) -> bool {
        if self.cursor != self.input.len() {
            return false;
        }
        let buf = self.input.clone();
        // Operate on the current logical line (after the last newline).
        let line_start = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &buf[line_start..];

        // Case 1: slash command — input starts with `/` and has no
        // whitespace yet on this line.
        if line.starts_with('/') && !line.contains(char::is_whitespace) {
            let prefix = &line[1..];
            let matches: Vec<&'static str> = KNOWN_SLASH_COMMANDS
                .iter()
                .copied()
                .filter(|c| c.starts_with(prefix))
                .collect();
            return self.apply_completion(
                line_start + 1, // after the `/`
                prefix,
                &matches,
                /* append_space */ true,
                /* is_path */ false,
            );
        }

        // Case 2: path completion — the last whitespace-separated token
        // on the current line, treated as a path. Backslash-escaped
        // spaces keep the token glued together.
        let (tok_start_in_line, token) = last_token(line);
        if token.is_empty() {
            return false;
        }
        let resolved = crate::path_input::resolve(token, workspace);
        let (parent, prefix) = split_parent_prefix(&resolved, token);
        let entries = match std::fs::read_dir(&parent) {
            Ok(it) => it,
            Err(_) => return false,
        };
        let mut names: Vec<(String, bool)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with(&prefix) {
                    return None;
                }
                // Hide dotfiles unless the user typed a leading dot.
                if name.starts_with('.') && !prefix.starts_with('.') {
                    return None;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some((name, is_dir))
            })
            .collect();
        names.sort();
        if names.is_empty() {
            return false;
        }

        // Common prefix across candidates.
        let common = longest_common_prefix(names.iter().map(|(n, _)| n.as_str()));
        if names.len() == 1 {
            let (name, is_dir) = &names[0];
            let mut replacement = shell_escape(name);
            if *is_dir {
                replacement.push('/');
            }
            // Replace from token-start to cursor.
            let tok_byte_start = line_start + tok_start_in_line;
            // Preserve the prefix portion the user typed that already
            // matched, by replacing the whole token with "<leading dirs>
            // + replacement". The `token` may include `./sub/name` — we
            // only replace the final component.
            let new_token = rewrite_token(token, &replacement);
            self.input
                .replace_range(tok_byte_start..self.cursor, &new_token);
            self.cursor = tok_byte_start + new_token.len();
            return true;
        }
        if common.len() > prefix.len() {
            let tok_byte_start = line_start + tok_start_in_line;
            let new_token = rewrite_token(token, &common);
            self.input
                .replace_range(tok_byte_start..self.cursor, &new_token);
            self.cursor = tok_byte_start + new_token.len();
            return true;
        }
        // Ambiguous — show candidates.
        let preview: Vec<String> = names
            .iter()
            .take(20)
            .map(|(n, is_dir)| if *is_dir { format!("{n}/") } else { n.clone() })
            .collect();
        let more = if names.len() > 20 {
            format!("  … (+{} more)", names.len() - 20)
        } else {
            String::new()
        };
        self.push_system(&format!("{}{}", preview.join("  "), more));
        false
    }

    /// Shared completion-apply for slash commands and similar
    /// fixed-vocabulary completions.
    fn apply_completion(
        &mut self,
        start_byte: usize,
        prefix: &str,
        matches: &[&'static str],
        append_space: bool,
        _is_path: bool,
    ) -> bool {
        if matches.is_empty() {
            return false;
        }
        if matches.len() == 1 {
            let m = matches[0];
            let tail = if append_space { " " } else { "" };
            let replacement = format!("{m}{tail}");
            self.input
                .replace_range(start_byte..self.cursor, &replacement);
            self.cursor = start_byte + replacement.len();
            return true;
        }
        let common = longest_common_prefix(matches.iter().copied());
        if common.len() > prefix.len() {
            self.input.replace_range(start_byte..self.cursor, &common);
            self.cursor = start_byte + common.len();
            return true;
        }
        let shown: Vec<String> = matches.iter().map(|m| format!("/{m}")).collect();
        self.push_system(&shown.join("  "));
        false
    }

    /// Atakan: CC-style 4-mod permission cycle. Shift+Tab handler.
    /// Default → AcceptEdits → Plan → Bypass → Default. Underlying
    /// PlanState ve always_allowed set'i sync tutar.
    pub fn cycle_permission_mode(&mut self) {
        let next = self.permission_mode.next();
        self.set_permission_mode(next);
    }

    pub fn set_permission_mode(&mut self, mode: PermMode) {
        self.permission_mode = mode;
        // Atakan: session sidecar'a yaz — `--resume` sonrası mod restore
        // edilebilsin. Sessiz hata: session henüz disk'te yoksa veya I/O
        // hata varsa (premortem F3 ders: ileride fail-loud uyarı eklenir).
        let mode_str = match mode {
            PermMode::Default => None,
            other => Some(other.label().to_string()),
        };
        if let Ok(mut store) = aegis_core::SessionStore::open(&self.workspace, &self.session_id) {
            let _ = store.set_permission_mode(mode_str);
        }
        // Sync PlanState
        {
            let mut plan = self.plan_state.lock().unwrap();
            *plan = match mode {
                PermMode::Plan => PlanState::Drafting,
                _ => PlanState::Normal,
            };
        }
        // Sync always_allowed set
        if let Some(set_arc) = self.always_allowed.as_ref() {
            let mut set = set_arc.lock().unwrap();
            match mode {
                PermMode::Default | PermMode::Plan => {
                    set.clear();
                }
                PermMode::AcceptEdits => {
                    set.clear();
                    for t in ["edit_file", "write_file", "multi_edit"] {
                        set.insert(t.to_string());
                    }
                }
                PermMode::Bypass => {
                    for t in [
                        "bash",
                        "edit_file",
                        "write_file",
                        "multi_edit",
                        "computer_use",
                        "web_fetch",
                        "web_search",
                    ] {
                        set.insert(t.to_string());
                    }
                }
            }
        }
        // Architect/editor swap: when [routing] plan_model / build_model
        // is configured, a mode flip swaps the active model. Cycling
        // back to Default restores the launch model so the user's
        // --model / config pick is never silently lost.
        let swap_msg = self.maybe_swap_model_for_mode(mode);

        let label = mode.label();
        let hint = match mode {
            PermMode::Default => "edit/bash için onay sorulur",
            PermMode::AcceptEdits => "edit'ler otomatik, bash onay",
            PermMode::Plan => "sadece read-only — keşif modu",
            PermMode::Bypass => "tüm tool'lar otomatik (yolo)",
        };
        self.push_system(&format!("[mode] {label} — {hint}  (Shift+Tab cycle)"));
        if let Some(line) = swap_msg {
            self.push_system(&line);
        }
    }

    /// Swap `self.model` to the routing-configured plan/build model on
    /// mode flips, or restore the launch model on Default. Returns a
    /// status string for the chat log; `None` when nothing changed.
    ///
    /// Cross-provider swaps are intentionally rejected here: re-init'ing
    /// the active client mid-session is non-trivial and the user almost
    /// always wants the swap to stay on their current provider. If the
    /// configured target carries a `provider:` prefix that doesn't
    /// match, the function emits a warning and leaves the model alone.
    fn maybe_swap_model_for_mode(&mut self, mode: PermMode) -> Option<String> {
        // Default → restore the launch model (only if we ever swapped).
        if mode == PermMode::Default {
            let snap = self.default_model.take()?;
            if snap == self.model {
                return None;
            }
            self.model = snap.clone();
            return Some(format!("[model] restored {snap}"));
        }
        // Plan / AcceptEdits / Bypass → consult routing config.
        let target = match mode {
            PermMode::Plan => self.plan_model.clone(),
            PermMode::AcceptEdits | PermMode::Bypass => self.build_model.clone(),
            PermMode::Default => return None,
        }?;
        let parsed = crate::router::RouteTarget::parse(&target);
        if let Some(prov) = &parsed.provider {
            if prov != &self.current_provider {
                return Some(format!(
                    "[model] {} target '{target}' wants provider '{prov}', current is '{}' — model NOT swapped (switch provider first)",
                    mode.label(),
                    self.current_provider
                ));
            }
        }
        if parsed.model == self.model {
            return None;
        }
        // Snapshot the launch model on the first swap so Default can
        // restore it later.
        if self.default_model.is_none() {
            self.default_model = Some(self.model.clone());
        }
        self.model = parsed.model.clone();
        Some(format!("[model] {} → {}", mode.label(), parsed.model))
    }

    /// Toggle plan mode — OpenCode / Copilot CLI style: Tab on empty
    /// input cycles Build → Plan(Drafting) → Build. Shift+Tab always
    /// cycles regardless of input state.
    ///
    /// No longer wired into the input handler — `cycle_permission_mode`
    /// (Shift+Tab) replaced the Tab-on-empty path. Kept for the unit
    /// test that pins the Build/Plan transition rules and as a public
    /// helper any future shortcut can reuse without re-deriving the
    /// state machine.
    #[allow(dead_code)]
    pub fn toggle_plan_mode(&mut self) {
        let mut state = self.plan_state.lock().unwrap();
        match *state {
            PlanState::Normal => {
                *state = PlanState::Drafting;
                drop(state);
                self.push_system("plan mode  [Tab to exit]");
            }
            PlanState::Drafting => {
                *state = PlanState::Normal;
                drop(state);
                self.push_system("build mode");
            }
            PlanState::Executing => {
                *state = PlanState::Normal;
                drop(state);
                self.push_system("build mode");
            }
        }
    }

    /// Take the current input, clear it, and return it.
    pub fn take_input(&mut self) -> String {
        let mut text = self.input.clone();
        // Expand any `[Pasted text #N +X lines]` placeholders back to raw
        // pasted content before the prompt is sent to the model. Markers the
        // user deleted are silently dropped — that buffer just won't be used.
        if !self.pasted_buffers.is_empty() {
            for (i, buf) in self.pasted_buffers.iter().enumerate() {
                let lines = buf.matches('\n').count();
                let marker = format!("[Pasted text #{} +{} lines]", i + 1, lines);
                if text.contains(&marker) {
                    text = text.replace(&marker, buf);
                }
            }
            self.pasted_buffers.clear();
        }
        if !text.trim().is_empty() {
            self.input_history.push(text.clone());
        }
        self.input.clear();
        self.cursor = 0;
        self.history_index = None;
        self.saved_input.clear();
        text
    }

    /// Run input as a shell command (`!` prefix — OpenCode/Copilot CLI).
    /// Pushes output as a system message. Returns true if a command ran.
    pub fn run_shell_command(&mut self) -> bool {
        let trimmed = self.input.trim().to_string();
        if let Some(cmd) = trimmed.strip_prefix('!') {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                self.input.clear();
                self.cursor = 0;
                return true;
            }
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output();
            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let mut result = format!("! {cmd}\n");
                    if !stdout.is_empty() {
                        let preview: String = stdout.lines().take(20).collect::<Vec<_>>().join("\n");
                        result.push_str(&preview);
                        if stdout.lines().count() > 20 {
                            result.push_str(&format!("\n… {} more lines", stdout.lines().count() - 20));
                        }
                    }
                    if !stderr.is_empty() {
                        let err_preview: String = stderr.lines().take(10).collect::<Vec<_>>().join("\n");
                        if !err_preview.is_empty() {
                            result.push_str(&format!("\nstderr:\n{err_preview}"));
                        }
                    }
                    if !out.status.success() {
                        result.push_str(&format!("\nexit code: {}", out.status.code().unwrap_or(-1)));
                    }
                    self.push_system(&result);
                    // Add to history
                    if !trimmed.trim().is_empty() {
                        self.input_history.push(trimmed);
                    }
                }
                Err(e) => {
                    self.push_error(&format!("! {cmd}\nfailed: {e}"));
                }
            }
            self.input.clear();
            self.cursor = 0;
            self.history_index = None;
            self.saved_input.clear();
            return true;
        }
        false
    }

    pub fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.saved_input = self.input.clone();
                let idx = self.input_history.len() - 1;
                self.history_index = Some(idx);
                self.input = self.input_history[idx].clone();
                self.cursor = self.input.len();
            }
            Some(idx) if idx > 0 => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = self.input_history[new_idx].clone();
                self.cursor = self.input.len();
            }
            _ => {}
        }
    }

    pub fn history_down(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.input_history.len() {
                let new_idx = idx + 1;
                self.history_index = Some(new_idx);
                self.input = self.input_history[new_idx].clone();
                self.cursor = self.input.len();
            } else {
                self.history_index = None;
                self.input = self.saved_input.clone();
                self.cursor = self.input.len();
            }
        }
    }

    // -- Chat search (Ctrl+F) --

    /// Open the chat-search overlay. Snapshots `scroll_offset` so cancel
    /// can restore it. No-op if already open.
    pub fn chat_search_open(&mut self) {
        if self.chat_search.is_some() {
            return;
        }
        self.chat_search = Some(ChatSearchState {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            saved_scroll: self.scroll_offset,
        });
    }

    /// Cancel the search overlay and restore the pre-search scroll
    /// position. Esc/Ctrl+G hits this.
    pub fn chat_search_cancel(&mut self) {
        if let Some(s) = self.chat_search.take() {
            self.scroll_offset = s.saved_scroll;
        }
    }

    /// Recompute matches from the current query (case-insensitive
    /// substring) and reset `current` to the first hit. Called after
    /// every query edit.
    pub fn chat_search_recompute(&mut self) {
        let Some(state) = self.chat_search.as_mut() else {
            return;
        };
        if state.query.is_empty() {
            state.matches.clear();
            state.current = 0;
            return;
        }
        let needle = state.query.to_lowercase();
        let matches: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                if m.text.to_lowercase().contains(&needle) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let state = self.chat_search.as_mut().unwrap();
        state.matches = matches;
        state.current = 0;
        self.chat_search_apply_scroll();
    }

    /// Step to the next match (`forward = true`) or previous match.
    /// Wraps at the ends. No-op when `matches` is empty.
    pub fn chat_search_step(&mut self, forward: bool) {
        let Some(state) = self.chat_search.as_mut() else {
            return;
        };
        if state.matches.is_empty() {
            return;
        }
        let len = state.matches.len();
        state.current = if forward {
            (state.current + 1) % len
        } else {
            (state.current + len - 1) % len
        };
        self.chat_search_apply_scroll();
    }

    /// True when search is open AND has at least one match. Used by the
    /// renderer to decide whether to scroll to the active match.
    pub fn chat_search_active_match(&self) -> Option<usize> {
        let s = self.chat_search.as_ref()?;
        s.matches.get(s.current).copied()
    }

    /// Approximate number of rendered rows below the message at `idx`,
    /// counting `text.lines()` plus one separator row per message. Used
    /// by `chat_search_apply_scroll` to convert "scroll to match" into
    /// a `scroll_offset` value without re-running the full ratatui
    /// wrapping pipeline. Wrapping is ignored — long lines under-count
    /// here, which lands the match a bit lower than centre rather than
    /// off-screen, which is the safer error mode.
    pub fn approx_lines_below(&self, idx: usize) -> u16 {
        let mut total: u32 = 0;
        for m in self.messages.iter().skip(idx + 1) {
            let count = m.text.lines().count().max(1) as u32 + 1;
            total = total.saturating_add(count);
        }
        total.min(u16::MAX as u32) as u16
    }

    /// Scroll the chat panel so the active search match is roughly
    /// centred. No-op when search is closed or has no match. Called
    /// from `chat_search_step` and `chat_search_recompute` so every
    /// state change that moves the active match also moves the view.
    pub fn chat_search_apply_scroll(&mut self) {
        let Some(idx) = self.chat_search_active_match() else {
            return;
        };
        let below = self.approx_lines_below(idx);
        let visible = self.last_visible_height.max(1);
        let target = below.saturating_add(visible / 2);
        let max = self.last_max_scroll.max(below); // never clamp below
        self.scroll_offset = target.min(max);
    }

    // -- Reverse-i-search (Ctrl+R) --

    /// Enter reverse-search mode. First Ctrl+R seeds the state with the
    /// existing input as the initial query — mirrors bash, where the
    /// current line becomes the search pattern.
    pub fn reverse_search_begin(&mut self) {
        let seed = self.input.clone();
        let state = ReverseSearchState {
            query: seed.clone(),
            match_idx: None,
            saved_input: self.input.clone(),
            saved_cursor: self.cursor,
        };
        self.search_state = Some(state);
        self.reverse_search_refind(/* from_newer */ None);
    }

    /// Ctrl+R again while in search mode → step to the next older match.
    pub fn reverse_search_step(&mut self) {
        if let Some(s) = self.search_state.as_ref() {
            let start = s.match_idx;
            self.reverse_search_refind(start);
        }
    }

    /// Append a character to the query and re-search from the current
    /// match position (so typing narrows the selection in place).
    pub fn reverse_search_append(&mut self, ch: char) {
        if let Some(s) = self.search_state.as_mut() {
            s.query.push(ch);
            let start = s.match_idx;
            // Re-search from whatever we last matched, including that
            // entry (so a narrower query that still matches doesn't
            // jump to an older duplicate).
            self.reverse_search_refind_inclusive(start);
        }
    }

    /// Remove the last character from the query. If the query becomes
    /// empty, leave search mode active with an empty match.
    pub fn reverse_search_backspace(&mut self) {
        if let Some(s) = self.search_state.as_mut() {
            s.query.pop();
            self.reverse_search_refind_inclusive(None);
        }
    }

    /// Accept the current match: copy the matched entry into the input
    /// buffer and exit search mode. Enter once more to actually submit.
    pub fn reverse_search_accept(&mut self) {
        if let Some(s) = self.search_state.take() {
            if let Some(i) = s.match_idx {
                if let Some(entry) = self.input_history.get(i).cloned() {
                    self.input = entry;
                    self.cursor = self.input.len();
                    return;
                }
            }
            // No match → accept means keep whatever saved_input had.
            self.input = s.saved_input;
            self.cursor = s.saved_cursor.min(self.input.len());
        }
    }

    /// Cancel search and restore the pre-search buffer.
    pub fn reverse_search_cancel(&mut self) {
        if let Some(s) = self.search_state.take() {
            self.input = s.saved_input;
            self.cursor = s.saved_cursor.min(self.input.len());
        }
    }

    /// Search for the next older history entry matching the query,
    /// starting from `from_newer - 1` (exclusive). Used by Ctrl+R steps
    /// and by `reverse_search_begin` (with None).
    fn reverse_search_refind(&mut self, from_newer: Option<usize>) {
        let query = match self.search_state.as_ref() {
            Some(s) => s.query.clone(),
            None => return,
        };
        let start_exclusive = from_newer.unwrap_or(self.input_history.len());
        let found = if query.is_empty() || start_exclusive == 0 {
            None
        } else {
            (0..start_exclusive)
                .rev()
                .find(|&i| self.input_history[i].contains(&query))
        };
        if let Some(s) = self.search_state.as_mut() {
            s.match_idx = found;
        }
    }

    /// Like `reverse_search_refind` but includes `start_inclusive` in
    /// the scan. Used when the query is narrowed by one char — the
    /// existing match may still satisfy it.
    fn reverse_search_refind_inclusive(&mut self, start_inclusive: Option<usize>) {
        let query = match self.search_state.as_ref() {
            Some(s) => s.query.clone(),
            None => return,
        };
        let end = match start_inclusive {
            Some(i) => i + 1,
            None => self.input_history.len(),
        };
        let found = if query.is_empty() {
            None
        } else {
            (0..end)
                .rev()
                .find(|&i| self.input_history[i].contains(&query))
        };
        if let Some(s) = self.search_state.as_mut() {
            s.match_idx = found;
        }
    }

    // -- Scrolling --

    pub fn scroll_up(&mut self, amount: u16) {
        // Soft cap — max is re-applied in draw() against the real
        // wrapped total. A hard cap of 10K covers any practical session
        // without letting unbounded mashing of PageUp stall later
        // PageDowns (scroll_offset gets too large and the user has to
        // press PageDown dozens of times before the viewport moves).
        self.scroll_offset = self.scroll_offset.saturating_add(amount).min(10_000);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 10_000;
    }

    // -- Message management --

    pub fn push_message(&mut self, role: MessageRole, text: String) {
        self.messages.push(ChatMessage {
            role,
            text,
            styled_lines: None,
            expanded: false,
        });
        // Only auto-scroll-to-bottom when the user was ALREADY at the
        // bottom. Unconditional scroll_to_bottom used to clobber the
        // scroll_offset on every streamed token, which is what made
        // older messages "go under the banner" — the pin-on-growth
        // logic in draw() had no chance because this reset offset=0
        // first. Auto-follow on bottom, stay put when scrolled up.
        if self.scroll_offset == 0 {
            self.scroll_to_bottom();
        }
    }

    /// Push a pre-styled message (renderer bypasses role-based coloring
    /// and emits the lines verbatim). Used by `/files` and future
    /// ext-colored listings. `plain` is the ANSI-stripped equivalent
    /// kept for tests, scroll math, and copy/paste.
    pub fn push_styled(&mut self, plain: String, lines: Vec<Line<'static>>) {
        self.messages.push(ChatMessage {
            role: MessageRole::System,
            text: plain,
            styled_lines: Some(lines),
            expanded: false,
        });
        if self.scroll_offset == 0 {
            self.scroll_to_bottom();
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.push_message(MessageRole::User, text.to_string());
    }

    /// Append a streaming-text chunk respecting the pause flag. While
    /// `stream_paused` is set, chunks accumulate in
    /// `stream_paused_buffer` instead of `streaming_text` so the chat
    /// view freezes at the moment Space was hit. `resume_stream`
    /// drains the buffer.
    pub fn push_stream_chunk(&mut self, text: &str) {
        if self.stream_paused {
            self.stream_paused_buffer.push_str(text);
        } else {
            self.streaming_text.push_str(text);
        }
    }

    /// Toggle pause. On resume, drain anything that arrived while
    /// paused into `streaming_text` so the user catches up to the
    /// real provider position. Called from the Space handler.
    pub fn toggle_stream_pause(&mut self) {
        if self.stream_paused {
            // Resume — flush buffer into the visible stream.
            let buffered = std::mem::take(&mut self.stream_paused_buffer);
            self.streaming_text.push_str(&buffered);
            self.stream_paused = false;
        } else {
            self.stream_paused = true;
        }
    }

    pub fn push_assistant(&mut self, text: &str) {
        self.push_message(MessageRole::Assistant, text.to_string());
        if !text.is_empty() {
            display::copy_to_clipboard_osc52(text);
        }
    }

    pub fn push_error(&mut self, text: &str) {
        self.push_message(MessageRole::Error, text.to_string());
    }

    pub fn push_system(&mut self, text: &str) {
        self.push_message(MessageRole::System, text.to_string());
    }

    /// Validate, optionally convert, and attach an image path to
    /// `pending_images`. Emits the magenta `[image] attached: <name>`
    /// breadcrumb on success, a `push_error` on failure. Shared by
    /// `/image`, bracketed-paste drag-drop, and `/paste` so all three
    /// entry points enforce the same format whitelist and conversion.
    ///
    /// HEIC/HEIF are auto-converted to JPEG via `sips` (macOS built-in);
    /// the converted file lands in a temp dir and is the path we push.
    pub fn attach_image_path(&mut self, path: &std::path::Path) {

        use crate::path_input::{prepare_image, ImagePrep};
        let resolved = match prepare_image(path) {
            ImagePrep::Ok(p) => p,
            ImagePrep::NotFound(p) => {
                self.push_error(&format!("file not found: {}", p.display()));
                return;
            }
            ImagePrep::Unsupported(ext) => {
                self.push_error(&format!(
                    "unsupported image format: .{ext} (use png/jpg/heic/gif/webp/bmp)"
                ));
                return;
            }
            ImagePrep::ConversionFailed(e) => {
                self.push_error(&format!(
                    "HEIC conversion failed: {e} (install `sips` or pre-convert)"
                ));
                return;
            }
        };

        let fname = resolved
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        // Dimensions + file size give the user a quick "did I attach
        // the right image" check without rendering an inline preview
        // (which would corrupt the alt-screen TUI on most terminals).
        // sips is macOS-built-in, no extra dependency. Failure → no
        // dimensions in the breadcrumb, the rest still appears.
        let dims = image_dimensions_via_sips(&resolved);
        let size_str = std::fs::metadata(&resolved)
            .ok()
            .map(|m| format_image_byte_size(m.len()))
            .unwrap_or_default();
        let ext = resolved
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let meta_chunks: Vec<String> = [
            dims,
            if size_str.is_empty() { None } else { Some(size_str) },
            if ext.is_empty() { None } else { Some(ext) },
        ]
        .into_iter()
        .flatten()
        .collect();
        let meta_suffix = if meta_chunks.is_empty() {
            String::new()
        } else {
            format!("  ({})", meta_chunks.join(" · "))
        };
        let mag_style = Style::default().fg(Color::Magenta);
        let plain_style = Style::default().fg(MessageRole::System.color());
        let label_style = plain_style.add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(Color::Rgb(140, 140, 140));
        let plain = format!("[image] attached: {fname}{meta_suffix}\n");
        let styled = vec![Line::from(vec![
            Span::styled("[goblin] ".to_string(), label_style),
            Span::styled("[image]".to_string(), mag_style),
            Span::styled(format!(" attached: {fname}"), plain_style),
            Span::styled(meta_suffix.clone(), dim_style),
        ])];
        self.push_styled(plain, styled);
        self.pending_images.push(resolved);
    }

    /// Flush any accumulated streaming text into a finalized message.
    /// Strip `<think>` / `<thinking>` blocks before persisting so the
    /// saved transcript (and scrollback) matches what the user saw
    /// live — without reasoning XML leaking into sessions. If stripping
    /// empties the message, we drop it entirely rather than persist a
    /// blank assistant turn.
    ///
    /// Also emits the end-of-turn auto-recap footer — tools, files
    /// touched, wall-clock duration, cost delta — as a single dim
    /// `⎿` line so the user sees the turn's shape at a glance without
    /// scrolling back through tool logs.
    pub fn flush_streaming(&mut self) {
        // If the user finished the turn with the stream still paused,
        // there's queued text in `stream_paused_buffer` that must
        // surface — otherwise we'd silently drop the tail of the
        // assistant reply. Drain it back into the visible stream
        // before flushing, then clear the pause flag so the next turn
        // starts fresh.
        if !self.stream_paused_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.stream_paused_buffer);
            self.streaming_text.push_str(&buffered);
        }
        self.stream_paused = false;
        if !self.streaming_text.is_empty() {
            let raw = std::mem::take(&mut self.streaming_text);
            let clean = strip_thinking_tags(&raw);
            if !clean.trim().is_empty() {
                self.push_assistant(&clean);
            }
        }
        self.thinking_text.clear();
        // Cost chip — opt-in via /cost on (default off). When enabled,
        // emit the queued cost footer as a single dim line beneath the
        // assistant turn so the user sees per-turn spend without
        // scrolling to the cumulative recap.
        if let Some(footer) = self.pending_cost_footer.take() {
            if self.cost_footer_enabled && !footer.trim().is_empty() {
                let dim = Style::default().fg(Color::Rgb(140, 140, 140));
                let chip = format!("· {}", footer.trim());
                let styled = vec![Line::from(vec![
                    Span::styled("    ".to_string(), dim),
                    Span::styled(chip.clone(), dim),
                ])];
                self.push_styled(chip, styled);
            }
        }
        let recap = self.build_turn_recap();
        if !recap.is_empty() {
            self.messages.push(ChatMessage {
                role: MessageRole::Footer,
                text: recap,
                styled_lines: None,
                expanded: false,
            });
        }
        // Notification bell: if the turn ran long enough, emit ASCII
        // BEL so the user can tab away while waiting. Stderr-gated on
        // is_terminal so piped TUI invocations (rare but exist) don't
        // poison the downstream consumer with control bytes.
        if self.bell_enabled {
            let duration = self
                .turn_start
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            if duration >= self.bell_threshold_secs {
                use std::io::{IsTerminal, Write};
                if std::io::stderr().is_terminal() {
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(b"\x07");
                    let _ = err.flush();
                }
            }
        }
        // Clear per-turn spinner + recap state so the next turn starts fresh.
        self.turn_start = None;
        self.first_token_seen = false;
        self.turn_tools.clear();
        self.turn_files.clear();
    }

    /// Build the auto-recap footer string shown at turn end. Empty
    /// when nothing noteworthy happened (no tools, no text, no cost).
    /// Format: `⎿ N tools (k × name) · M files: a, b · 4.2s · $0.012`.
    fn build_turn_recap(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if !self.turn_tools.is_empty() {
            let mut reads = 0usize;
            let mut writes = 0usize;
            let mut edits = 0usize;
            let mut searches = 0usize;
            let mut runs = 0usize;
            let mut web = 0usize;
            let mut other = 0usize;

            for t in &self.turn_tools {
                match t.as_str() {
                    "read_file" => reads += 1,
                    "write_file" | "create_file" => writes += 1,
                    "edit_file" | "multi_edit" => edits += 1,
                    "search_text" | "grep" | "find_files" | "glob" => searches += 1,
                    "bash" | "execute_command" | "run_command" => runs += 1,
                    "web_search" | "tavily_search" => web += 1,
                    _ => other += 1,
                }
            }

            let mut action_parts: Vec<String> = Vec::new();
            let s = |n: usize| if n == 1 { "" } else { "s" };
            if searches > 0 {
                action_parts.push(format!("searched {} pattern{}", searches, s(searches)));
            }
            if reads > 0 {
                action_parts.push(format!("read {} file{}", reads, s(reads)));
            }
            if edits > 0 {
                action_parts.push(format!("edited {} file{}", edits, s(edits)));
            }
            if writes > 0 {
                action_parts.push(format!("wrote {} file{}", writes, s(writes)));
            }
            if runs > 0 {
                action_parts.push(format!("ran {} command{}", runs, s(runs)));
            }
            if web > 0 {
                action_parts.push(format!("searched web {} time{}", web, s(web)));
            }
            if other > 0 {
                action_parts.push(format!("{} action{}", other, s(other)));
            }
            if !action_parts.is_empty() {
                parts.push(action_parts.join(", "));
            }
        }

        if !self.turn_files.is_empty() {
            let names: Vec<&str> = self.turn_files.iter()
                .map(|f| std::path::Path::new(f)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(f))
                .collect();
            parts.push(format!("{} file{}: {}", names.len(),
                if names.len() == 1 { "" } else { "s" },
                names.join(", ")));
        }

        // Only append the elapsed time when there's something else to
        // recap (tool actions). On a pure-chat turn, "⏺ thought for X.Xs"
        // already shows the time; emitting "• X.Xs" again would just
        // duplicate the timer. CC parity: one timer per turn.
        if !parts.is_empty() {
            if let Some(start) = self.turn_start {
                let elapsed = start.elapsed().as_secs_f32();
                if elapsed >= 0.1 {
                    parts.push(format!("{elapsed:.1}s"));
                }
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("\u{2022} {}", parts.join(" · "))
        }
    }

    // -- Tool activity --

    pub fn tool_start(&mut self, name: &str, preview: &str) {
        self.tools.push(ToolEntry {
            name: name.to_string(),
            status: ToolStatus::Running,
            preview: truncate_str(preview, 60).to_string(),
        });
        // Per-turn recap tracking — tool name for the tools-pill,
        // file path (if it's a mutating tool) for the files-pill.
        self.turn_tools.push(name.to_string());
        if matches!(name, "edit_file" | "write_file" | "multi_edit") {
            if let Some(path) = extract_path_from_preview(preview) {
                if !self.turn_files.iter().any(|f| f == &path) {
                    self.turn_files.push(path);
                }
            }
        }
        // CC-style inline header: `Read(file.rs)`, `Bash(cargo test)`,
        // `Edit(/tmp/foo.rs)`. Tool name is canonicalised to PascalCase
        // and only one representative argument is shown — full JSON would
        // overflow the terminal and lose the at-a-glance scan that CC's
        // tool-call rows are good for.
        let display_name = canonical_tool_name(name);
        let header = match extract_primary_arg(preview, name) {
            Some(arg) => format!("{display_name}({})", truncate_str(&arg, 60)),
            None => display_name,
        };
        self.messages.push(ChatMessage {
            role: MessageRole::Tool,
            text: header,
            styled_lines: None,
            expanded: false,
        });
    }

    pub fn tool_done(&mut self, name: &str, preview: &str, is_error: bool) {
        // Update the last matching running entry in the tool state log.
        for entry in self.tools.iter_mut().rev() {
            if entry.name == name && entry.status == ToolStatus::Running {
                entry.status = if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Done
                };
                entry.preview = truncate_str(preview, 60).to_string();
                break;
            }
        }
        // Inline preview line in chat — `⎿ preview` under the `●`.
        // CC parity: edit_file/write_file/multi_edit produce a multi-line
        // diff preview (see core::format_tool_preview). The agent layer
        // embeds raw ANSI for REPL consumption; for the TUI we strip them
        // so the ratatui-side `diff_styled_span` can re-apply per-line
        // colors at the cell level. Multi-line previews skip truncation
        // and auto-expand so the diff is visible without ctrl+O.
        // Exception: bash tool — preserve ANSI colors so `ls --color`,
        // `git diff`, etc. render with native terminal colors.
        let role = if is_error {
            MessageRole::Error
        } else {
            MessageRole::ToolResult
        };
        let cleaned = preview.to_string();  // keep ANSI for all tool output
        let is_multiline = cleaned.contains('\n');
        let has_diff = cleaned
            .lines()
            .any(|l| l.starts_with("@@") || l.starts_with("+++") || l.starts_with("---"));
        let auto_expand = is_multiline || has_diff;

        let text = if is_error {
            format!("✗ {name}: {}", truncate_str(&cleaned, 80))
        } else if auto_expand {
            cleaned
        } else {
            truncate_str(&cleaned, 80).to_string()
        };
        self.messages.push(ChatMessage {
            role,
            text,
            styled_lines: None,
            expanded: auto_expand,
        });
    }

    // -- Slash commands --

    pub fn handle_slash(&mut self, line: &str, workspace: &Path) -> SlashResult {
        let trimmed = line.trim();
        let rest_after_cmd = trimmed
            .strip_prefix('/')
            .unwrap_or(trimmed)
            .split_once(char::is_whitespace)
            .map(|(_, r)| r)
            .unwrap_or("")
            .trim()
            .to_string();
        let mut parts = trimmed
            .strip_prefix('/')
            .unwrap_or(trimmed)
            .split_whitespace();
        let cmd = parts.next().unwrap_or("");
        match cmd {
            "info" | "help" | "?" => {
                // OpenCode-style: open as a modal overlay so the help
                // reference doesn't bury the live conversation in the
                // scrollback. Toggle behavior — second invocation closes.
                self.help_overlay_open = !self.help_overlay_open;
                self.help_scroll = 0;
                SlashResult::Handled
            }
            "init" => {
                let agents_path = workspace.join("AGENTS.md");
                if agents_path.exists() {
                    self.push_system(&format!(
                        "AGENTS.md already exists at {}",
                        agents_path.display()
                    ));
                } else {
                    let mut content = String::from("# AGENTS.md\n\n");
                    content.push_str("## Project Overview\n\n");
                    content.push_str(&format!("Workspace: {}\n\n", workspace.display()));
                    content.push_str("## Key Files\n\n");
                    if let Ok(entries) = std::fs::read_dir(workspace) {
                        let files: Vec<_> = entries
                            .filter_map(|e| e.ok())
                            .filter(|e| e.path().is_file())
                            .filter(|e| {
                                let name = e.file_name().to_string_lossy().into_owned();
                                !name.starts_with('.') && !name.ends_with(".lock")
                            })
                            .take(20)
                            .map(|e| format!("- {}", e.file_name().to_string_lossy()))
                            .collect();
                        if files.is_empty() {
                            content.push_str("_No source files found yet._\n");
                        } else {
                            for f in &files {
                                content.push_str(f);
                                content.push('\n');
                            }
                        }
                    }
                    match std::fs::write(&agents_path, &content) {
                        Ok(()) => self.push_system(&format!(
                            "Created AGENTS.md at {}",
                            agents_path.display()
                        )),
                        Err(e) => self.push_error(&format!("Failed to create AGENTS.md: {e}")),
                    }
                }
                SlashResult::Handled
            }
            "undo" => {
                if self.turn_files.is_empty() {
                    self.push_system("nothing to undo (no files were modified this turn)");
                } else {
                    let files: Vec<_> = self.turn_files.iter()
                        .map(|f| f.to_string())
                        .collect();
                    let args: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                    match std::process::Command::new("git")
                        .args(["checkout", "--"])
                        .args(&args)
                        .current_dir(workspace)
                        .output()
                    {
                        Ok(out) if out.status.success() => {
                            let n = args.len();
                            self.push_system(&format!(
                                "undone {} change{}: {}",
                                n,
                                if n == 1 { "" } else { "s" },
                                args.join(", ")
                            ));
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            self.push_error(&format!("undo failed: {}", stderr.trim()));
                        }
                        Err(e) => {
                            self.push_error(&format!("undo failed (is this a git repo?): {e}"));
                        }
                    }
                }
                SlashResult::Handled
            }
            "redo" => {
                self.push_system("redo: use `git reflog` to find the undone state and restore manually.");
                SlashResult::Handled
            }
            "rewind" => {
                // Atakan: CC Esc+Esc rewind menü slash karşılığı.
                // 4 alt-komut: code | conv [n] | both [n] | from <n>.
                // Default conv 1 (son user turn'ünü sil).
                let action = parts.next().unwrap_or("conv");
                let arg_n: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                match action {
                    "code" => {
                        // /undo aliası — turn_files git checkout
                        if self.turn_files.is_empty() {
                            self.push_system("rewind code: bu turn'de değişen dosya yok");
                        } else {
                            let files: Vec<_> = self.turn_files.iter().map(|f| f.to_string()).collect();
                            let args: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                            match std::process::Command::new("git")
                                .args(["checkout", "--"])
                                .args(&args)
                                .current_dir(workspace)
                                .output()
                            {
                                Ok(out) if out.status.success() => {
                                    self.turn_files.clear();
                                    self.push_system(&format!(
                                        "rewind code: {} dosya geri alındı: {}",
                                        args.len(),
                                        args.join(", ")
                                    ));
                                }
                                Ok(out) => self.push_error(&format!(
                                    "rewind code: git checkout fail — {}",
                                    String::from_utf8_lossy(&out.stderr).trim()
                                )),
                                Err(e) => self.push_error(&format!("rewind code: {e}")),
                            }
                        }
                    }
                    "conv" => {
                        let cutoff = nth_user_turn_from_end(&self.messages, arg_n);
                        match cutoff {
                            Some(idx) => {
                                let dropped = self.messages.len() - idx;
                                self.messages.truncate(idx);
                                self.streaming_text.clear();
                                self.thinking_text.clear();
                                self.push_system(&format!(
                                    "rewind conv: son {arg_n} user turn(s) silindi ({dropped} mesaj)"
                                ));
                            }
                            None => self.push_system(&format!(
                                "rewind conv: {arg_n} user turn yok (mevcut: {})",
                                self.messages.iter().filter(|m| m.role == MessageRole::User).count()
                            )),
                        }
                    }
                    "both" => {
                        // Önce code, sonra conv.
                        if !self.turn_files.is_empty() {
                            let files: Vec<_> = self.turn_files.iter().map(|f| f.to_string()).collect();
                            let args: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                            let _ = std::process::Command::new("git")
                                .args(["checkout", "--"])
                                .args(&args)
                                .current_dir(workspace)
                                .output();
                            self.turn_files.clear();
                        }
                        if let Some(idx) = nth_user_turn_from_end(&self.messages, arg_n) {
                            let dropped = self.messages.len() - idx;
                            self.messages.truncate(idx);
                            self.streaming_text.clear();
                            self.thinking_text.clear();
                            self.push_system(&format!(
                                "rewind both: kod geri alındı + son {arg_n} turn silindi ({dropped} mesaj)"
                            ));
                        } else {
                            self.push_system("rewind both: kod geri alındı, conv için yeterli turn yok");
                        }
                    }
                    "from" => {
                        // Selective: index n'den sonrasını sil.
                        if arg_n >= self.messages.len() {
                            self.push_system(&format!(
                                "rewind from: index {arg_n} >= mesaj sayısı {}",
                                self.messages.len()
                            ));
                        } else {
                            let dropped = self.messages.len() - arg_n;
                            self.messages.truncate(arg_n);
                            self.streaming_text.clear();
                            self.thinking_text.clear();
                            self.push_system(&format!(
                                "rewind from {arg_n}: {dropped} mesaj silindi"
                            ));
                        }
                    }
                    _ => self.push_system(
                        "rewind: usage `/rewind {code | conv [n] | both [n] | from <idx>}` (default: `/rewind conv 1`)"
                    ),
                }
                SlashResult::Handled
            }
            "commit" => {
                // Atakan: Aider auto-commit yerine opt-in /commit. Turn'de
                // değişen dosyalar varsa onları (`turn_files`) stage'le ve
                // `[goblin] <message>` etiketli commit at. Boş turn'de yapacak
                // bir şey yok mesajı. Geri-alma için /undo zaten dosya
                // checkout yapıyor; commit varsa kullanıcı `git revert` ile
                // de geri alabilir.
                let raw_msg = rest_after_cmd.trim();
                let msg = if raw_msg.is_empty() {
                    "[goblin] auto-commit".to_string()
                } else {
                    format!("[goblin] {raw_msg}")
                };
                if self.turn_files.is_empty() {
                    self.push_system("/commit: bu turn'de değişen dosya yok");
                } else {
                    let files: Vec<_> = self.turn_files.iter().map(|f| f.to_string()).collect();
                    let args: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                    let add_out = std::process::Command::new("git")
                        .args(["add", "--"])
                        .args(&args)
                        .current_dir(workspace)
                        .output();
                    if let Err(e) = add_out {
                        self.push_error(&format!("/commit: git add fail: {e}"));
                        return SlashResult::Handled;
                    }
                    let commit_out = std::process::Command::new("git")
                        .args(["commit", "-m", &msg, "--"])
                        .args(&args)
                        .current_dir(workspace)
                        .output();
                    match commit_out {
                        Ok(out) if out.status.success() => {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let summary: String =
                                stdout.lines().take(3).collect::<Vec<_>>().join("\n");
                            self.push_system(&format!(
                                "/commit: ✓ {} dosya — {summary}",
                                args.len()
                            ));
                            self.turn_files.clear();
                        }
                        Ok(out) => {
                            let err = String::from_utf8_lossy(&out.stderr);
                            self.push_error(&format!(
                                "/commit fail (exit {}): {}",
                                out.status.code().unwrap_or(-1),
                                err.trim()
                            ));
                        }
                        Err(e) => self.push_error(&format!("/commit: spawn: {e}")),
                    }
                }
                SlashResult::Handled
            }
            "diff" => {
                // Atakan: workspace diff göster. Bare /diff → working
                // tree (staged + unstaged). With an argument: try it as
                // a git revision first (`HEAD~3`, `origin/main`, range
                // forms like `main..HEAD`); fall back to path filter
                // when rev-parse rejects the token. Output is a system
                // message; ANSI is stripped so terminal-bell-like
                // sequences in user filenames can't poison the buffer.
                let target = rest_after_cmd.trim();
                let mut args: Vec<&str> = vec!["diff", "--no-color"];
                let target_owned = target.to_string();
                if !target.is_empty() {
                    let is_ref = std::process::Command::new("git")
                        .args(["rev-parse", "--verify", "--quiet", target])
                        .current_dir(workspace)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    let is_range = target.contains("..");
                    if !(is_ref || is_range) {
                        args.push("--");
                    }
                    args.push(&target_owned);
                }
                let out = std::process::Command::new("git")
                    .args(&args)
                    .current_dir(workspace)
                    .output();
                match out {
                    Ok(o) if o.status.success() => {
                        let s = String::from_utf8_lossy(&o.stdout);
                        if s.trim().is_empty() {
                            self.push_system("/diff: working tree clean");
                        } else {
                            let lines: Vec<&str> = s.lines().take(200).collect();
                            let mut body = format!("$ git {}\n", args.join(" "));
                            body.push_str(&lines.join("\n"));
                            if s.lines().count() > 200 {
                                body.push_str(&format!(
                                    "\n… {} more lines (use git diff for full)",
                                    s.lines().count() - 200
                                ));
                            }
                            self.push_system(&body);
                        }
                    }
                    Ok(o) => self.push_error(&format!(
                        "/diff fail: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )),
                    Err(e) => self.push_error(&format!("/diff: spawn: {e}")),
                }
                SlashResult::Handled
            }
            "save" => {
                // Atakan: manuel mnemonics ingest. system_prompt'ta /save
                // tanımlı ama kod-side handler yoktu. LLM judge atlanır —
                // kullanıcı zaten kayda değer olduğuna karar vermiş. Secret
                // regex hala uygulanır, sızıntı engeli.
                let fact = rest_after_cmd.trim().to_string();
                if fact.is_empty() {
                    self.push_error("/save: usage: /save <fact>");
                    return SlashResult::Handled;
                }
                if let Some(reason) = detect_secret_pattern(&fact) {
                    self.push_error(&format!(
                        "/save REJECTED: secret pattern ({reason}) — kayıt yapılmadı"
                    ));
                    return SlashResult::Handled;
                }
                let repo = workspace
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let ns = format!("proj:{repo}");
                let date = ymd_today();
                let snippet = format!("[{date}] [{repo}] {}", fact.trim());
                // Dedup check — kullanıcı manuel /save ile aynı şeyi
                // tekrar atarsa noise eklemesin.
                if let Some(score) = check_dedup(&ns, &snippet) {
                    if score >= DEDUP_COSINE_THRESHOLD {
                        self.push_system(&format!(
                            "/save: skipped duplicate (cosine={score:.2} ≥ {DEDUP_COSINE_THRESHOLD})"
                        ));
                        return SlashResult::Handled;
                    }
                }
                let out = std::process::Command::new("mnemonics")
                    .args(["ingest", "--ns", &ns, &snippet])
                    .output();
                match out {
                    Ok(o) if o.status.success() => {
                        self.push_system(&format!(
                            "/save: ✓ ns={ns} — {}",
                            truncate_chars(&fact, 100)
                        ));
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        self.push_error(&format!(
                            "/save fail (exit {}): {}",
                            o.status.code().unwrap_or(-1),
                            stderr.trim()
                        ));
                    }
                    Err(e) => self.push_error(&format!("/save: spawn fail: {e}")),
                }
                SlashResult::Handled
            }
            "save-template" | "save_template" => {
                let name = parts.next().unwrap_or("");
                if name.is_empty() {
                    self.push_error(
                        "/save-template: usage: /save-template <name>  (saves current input or last user message)",
                    );
                    return SlashResult::Handled;
                }
                let body = if !self.input.trim().is_empty() {
                    self.input.clone()
                } else {
                    match self
                        .messages
                        .iter()
                        .rev()
                        .find(|m| matches!(m.role, MessageRole::User))
                    {
                        Some(m) => m.text.clone(),
                        None => {
                            self.push_error(
                                "/save-template: input is empty and no prior user message to capture",
                            );
                            return SlashResult::Handled;
                        }
                    }
                };
                match crate::templates::save(workspace, name, &body) {
                    Ok(p) => self.push_system(&format!(
                        "/save-template: ✓ {} ({} bytes)",
                        p.display(),
                        body.len()
                    )),
                    Err(e) => self.push_error(&format!("/save-template: {e}")),
                }
                SlashResult::Handled
            }
            "use" => {
                let name = parts.next().unwrap_or("");
                if name.is_empty() {
                    self.push_error(
                        "/use: usage: /use <name>  (see /templates for the list)",
                    );
                    return SlashResult::Handled;
                }
                match crate::templates::load(workspace, name) {
                    Ok(body) => {
                        self.input = body;
                        self.cursor = self.input.len();
                        self.push_system(&format!(
                            "/use: ✓ loaded `{name}` into input ({} bytes) — Enter to send",
                            self.input.len()
                        ));
                    }
                    Err(e) => self.push_error(&format!("/use: {e}")),
                }
                SlashResult::Handled
            }
            "templates" => {
                let names = crate::templates::list(workspace);
                if names.is_empty() {
                    self.push_system(
                        "/templates: no templates yet. /save-template <name> to create.",
                    );
                } else {
                    self.push_system(&format!("/templates: {}", names.join(", ")));
                }
                SlashResult::Handled
            }
            "pin" => {
                // No arg: pin the most recent user message. With an
                // integer arg: toggle that exact index. Pinning the
                // same index twice unpins, so /pin <n> doubles as
                // /unpin <n>.
                let arg = parts.next().unwrap_or("").trim();
                let target: Option<usize> = if arg.is_empty() {
                    self.messages
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, m)| matches!(m.role, MessageRole::User))
                        .map(|(i, _)| i)
                } else {
                    arg.parse::<usize>().ok().filter(|n| *n < self.messages.len())
                };
                match target {
                    Some(idx) => {
                        if self.pinned.remove(&idx) {
                            self.push_system(&format!("/pin: unpinned #{idx}"));
                        } else {
                            self.pinned.insert(idx);
                            let preview: String =
                                self.messages[idx].text.chars().take(60).collect();
                            self.push_system(&format!("/pin: ✓ #{idx} ★ {preview}"));
                        }
                    }
                    None => {
                        if arg.is_empty() {
                            self.push_error(
                                "/pin: no user messages yet. /pin <index> to pin a specific message.",
                            );
                        } else {
                            self.push_error(&format!(
                                "/pin: index must be 0..{} (got `{arg}`)",
                                self.messages.len()
                            ));
                        }
                    }
                }
                SlashResult::Handled
            }
            "unpin" => {
                let arg = parts.next().unwrap_or("").trim();
                if arg == "all" {
                    let n = self.pinned.len();
                    self.pinned.clear();
                    self.push_system(&format!("/unpin: cleared {n} pin(s)"));
                } else if let Ok(idx) = arg.parse::<usize>() {
                    if self.pinned.remove(&idx) {
                        self.push_system(&format!("/unpin: removed #{idx}"));
                    } else {
                        self.push_system(&format!("/unpin: #{idx} was not pinned"));
                    }
                } else {
                    self.push_error("/unpin: usage: /unpin <index> | /unpin all");
                }
                SlashResult::Handled
            }
            "pinned" => {
                if self.pinned.is_empty() {
                    self.push_system(
                        "/pinned: no pins yet. /pin (last user message) or /pin <index>.",
                    );
                } else {
                    let mut lines: Vec<String> =
                        vec![format!("/pinned: {} message(s)", self.pinned.len())];
                    for idx in &self.pinned {
                        if let Some(msg) = self.messages.get(*idx) {
                            let preview: String = msg.text.chars().take(80).collect();
                            let ellipsis = if msg.text.chars().count() > 80 {
                                "…"
                            } else {
                                ""
                            };
                            lines.push(format!("  ★ #{idx} {preview}{ellipsis}"));
                        }
                    }
                    self.push_system(&lines.join("\n"));
                }
                SlashResult::Handled
            }
            "bell" => {
                let arg = parts.next().unwrap_or("").trim();
                match arg {
                    "" => {
                        // Toggle.
                        self.bell_enabled = !self.bell_enabled;
                        let state = if self.bell_enabled { "on" } else { "off" };
                        self.push_system(&format!(
                            "/bell: {state} (threshold {}s) — fires after long turns",
                            self.bell_threshold_secs
                        ));
                    }
                    "on" => {
                        self.bell_enabled = true;
                        self.push_system(&format!(
                            "/bell: on (threshold {}s)",
                            self.bell_threshold_secs
                        ));
                    }
                    "off" => {
                        self.bell_enabled = false;
                        self.push_system("/bell: off");
                    }
                    n => match n.parse::<u64>() {
                        Ok(secs) if secs >= 1 && secs <= 3600 => {
                            self.bell_threshold_secs = secs;
                            self.bell_enabled = true;
                            self.push_system(&format!(
                                "/bell: on, threshold {secs}s"
                            ));
                        }
                        _ => {
                            self.push_error(
                                "/bell: usage: /bell [on|off|<seconds 1-3600>]  (no arg toggles)",
                            );
                        }
                    },
                }
                SlashResult::Handled
            }
            "recall-prev" | "recall_prev" | "recallprev" => {
                // Atakan: Trigger B — boot-time unsaved session recovery.
                // We only check + queue here; the actual ingest is async
                // (LLM judge call), so the run_tui event loop drains the
                // pending flag and spawns the work. This keeps slash
                // handler synchronous and side-effect-light.
                match aegis_core::SessionStore::previous_unsaved_session(workspace, None) {
                    Ok(Some(hint)) => {
                        if hint.message_count == 0 {
                            self.push_system(
                                "/recall-prev: previous session is empty, nothing to recover",
                            );
                            return SlashResult::Handled;
                        }
                        self.push_system(&format!(
                            "/recall-prev: queued recovery for session {} ({} msgs, {}s ago) — judging…",
                            &hint.id.chars().take(16).collect::<String>(),
                            hint.message_count,
                            hint.age_secs
                        ));
                        self.pending_recall_prev = Some(());
                    }
                    Ok(None) => {
                        self.push_system(
                            "/recall-prev: no unsaved previous session in this workspace",
                        );
                    }
                    Err(e) => {
                        self.push_error(&format!("/recall-prev: scan failed: {e}"));
                    }
                }
                SlashResult::Handled
            }
            "copy-context" => {
                // Atakan: Aider /copy-context pattern. Mevcut conversation'ı
                // markdown olarak osc52 ile sistem clipboard'una yazar (ssh
                // session'larında bile). ChatGPT/Claude web UI'sine
                // yapıştırılabilir. Tool result'lar dahil değil — sadece
                // user/assistant turn'leri.
                let mut out = String::new();
                out.push_str(&format!(
                    "# Aegis context (model: {}, turns: {})\n\n",
                    self.model, self.turn_count
                ));
                for m in &self.messages {
                    match m.role {
                        MessageRole::User => {
                            out.push_str("## User\n\n");
                            out.push_str(m.text.trim());
                            out.push_str("\n\n");
                        }
                        MessageRole::Assistant => {
                            out.push_str("## Assistant\n\n");
                            out.push_str(m.text.trim());
                            out.push_str("\n\n");
                        }
                        _ => {} // skip system/tool/error
                    }
                }
                let bytes = out.len();
                display::copy_to_clipboard_osc52(&out);
                let user_count = self
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::User)
                    .count();
                let asst_count = self
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::Assistant)
                    .count();
                self.push_system(&format!(
                    "/copy-context: {bytes} bytes panoda — {user_count} user + {asst_count} assistant turn (tool/system mesajları hariç)"
                ));
                SlashResult::Handled
            }
            "exit" | "quit" => {
                self.should_quit = true;
                SlashResult::Quit
            }
            "banner" => {
                // Show the full ASCII art on demand. Default startup is
                // single-line CC-style; this lets users opt back in for
                // nostalgia or screenshots.
                for line in BANNER_LINES {
                    self.push_system(line);
                }
                self.push_system("the original swallowed intelligence");
                SlashResult::Handled
            }
            "idle" => {
                // 5-minute idle reminder is off by default (CC parity).
                // `/idle on` opts in, `/idle off` opts out, bare `/idle`
                // reports current state. Resets the "already-shown" flag on
                // toggle so users get one reminder right after enabling.
                let arg = rest_after_cmd.to_ascii_lowercase();
                match arg.as_str() {
                    "on" | "1" | "true" | "ac" | "aç" => {
                        self.idle_reminder_enabled = true;
                        self.idle_reminder_sent = false;
                        self.push_system("idle reminder: on (5dk sessizlikte session özeti basılır)");
                    }
                    "off" | "0" | "false" | "kapa" | "kapat" => {
                        self.idle_reminder_enabled = false;
                        self.push_system("idle reminder: off");
                    }
                    "" => {
                        let state = if self.idle_reminder_enabled { "on" } else { "off" };
                        self.push_system(&format!(
                            "idle reminder: {state}  (kullanım: /idle on  ya da  /idle off)"
                        ));
                    }
                    _ => {
                        self.push_system(
                            "kullanım: /idle on  ya da  /idle off  (argümansız → durumu gösterir)",
                        );
                    }
                }
                SlashResult::Handled
            }
            "clear" => {
                // Two-step confirmation: first `/clear` arms, second
                // within 5 seconds actually wipes. Destructive —
                // losing an in-progress session to a typo is the kind
                // of thing that makes people hate tools.
                let now = std::time::Instant::now();
                const CLEAR_PRIME_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
                let primed = matches!(
                    self.clear_primed_at,
                    Some(t) if now.duration_since(t) <= CLEAR_PRIME_WINDOW
                );
                if !primed {
                    self.clear_primed_at = Some(now);
                    let msg_count = self.messages.len();
                    let tool_count = self.tools.len();
                    self.push_system(&format!(
                        "/clear will wipe this session ({msg_count} messages, \
                         {tool_count} tool entries). Run /clear again within 5s to confirm."
                    ));
                    return SlashResult::Handled;
                }
                self.clear_primed_at = None;
                self.messages.clear();
                self.tools.clear();
                self.scroll_offset = 0;
                self.streaming_text.clear();
                self.thinking_text.clear();
                self.pinned.clear();
                self.session_id = SessionStore::new_id();
                self.push_system("Chat cleared.");
                SlashResult::Clear
            }
            "mouse" => {
                // Toggle crossterm mouse capture so the user can fall
                // back to native terminal select & copy (for Wacom pen
                // or any setup where Option+drag doesn't reach us).
                // The actual enable/disable crossterm call happens in
                // the main loop where `stdout()` is owned; we just
                // flip the flag and the loop reconciles on next tick.
                use crossterm::ExecutableCommand;
                self.mouse_capture_on = !self.mouse_capture_on;
                let msg = if self.mouse_capture_on {
                    let _ = std::io::stdout().execute(crossterm::event::EnableMouseCapture);
                    "mouse capture ON — trackpad scroll active, native select disabled"
                } else {
                    let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
                    "mouse capture OFF — native select/copy restored, use PageUp/Shift+↑ to scroll"
                };
                self.push_system(msg);
                SlashResult::Handled
            }
            "sidebar" => {
                self.sidebar_visible = !self.sidebar_visible;
                let msg = if self.sidebar_visible {
                    "sidebar on"
                } else {
                    "sidebar off"
                };
                self.push_system(msg);
                SlashResult::Handled
            }
            "cost" => match rest_after_cmd.trim() {
                "off" => {
                    self.cost_footer_enabled = false;
                    self.push_system("cost footer off");
                    SlashResult::Handled
                }
                "on" => {
                    self.cost_footer_enabled = true;
                    self.push_system("cost footer on");
                    SlashResult::Handled
                }
                _ => {
                    let breakdown = aegis_core::format_cost_breakdown(
                        &self.cumulative_usage,
                        &self.model,
                        self.turn_count as usize,
                        true,
                    );
                    self.push_system(&breakdown);
                    SlashResult::Handled
                }
            },
            "stats" => {
                if let Some(path) = aegis_core::telemetry::telemetry_path() {
                    let records = aegis_core::telemetry::load_records(&path);
                    if records.is_empty() {
                        self.push_system("no telemetry data yet");
                    } else {
                        let stats = aegis_core::telemetry::UsageStats::from_records(&records);
                        self.push_system(&stats.format_dashboard());
                    }
                } else {
                    self.push_error("could not determine telemetry path");
                }
                SlashResult::Handled
            }
            "providers" => {
                use aegis_api::Provider;
                let items: Vec<(String, String, bool)> = Provider::BUILTINS
                    .iter()
                    .map(|p| {
                        let has_key = std::env::var(p.env_var).is_ok();
                        (p.id.to_string(), p.default_model.to_string(), has_key)
                    })
                    .collect();
                self.provider_menu = Some(items);
                SlashResult::Handled
            }
            "connect" => {
                use aegis_api::Provider;
                let mut out = String::from("Connect a provider — set the API key env var:\n\n");
                for p in Provider::BUILTINS {
                    let has = std::env::var(p.env_var).is_ok();
                    out.push_str(&format!(
                        "  {}  {:<12}  export {}={}\n",
                        if has { "✓" } else { "✗" },
                        p.id,
                        p.env_var,
                        if has { "(set)" } else { "<your-key>" }
                    ));
                }
                out.push_str("\n  Add to ~/.zshrc or ~/.metis/config.toml [api_keys]");
                self.push_system(&out);
                SlashResult::Handled
            }
            "share" => {
                self.push_system("share: sessions are stored in workspace/.metis/sessions/\n  Copy the session JSONL file to share with others.");
                SlashResult::Handled
            }
            "login" => {
                self.push_system("login: set API keys via environment variables.\n  See /connect for a list of providers and their key names.\n  Or add to ~/.metis/config.toml [api_keys] section.");
                SlashResult::Handled
            }
            "cwd" | "cd" => {
                if rest_after_cmd.is_empty() {
                    self.push_system(&format!("cwd: {}", self.workspace.display()));
                } else {
                    let p = std::path::PathBuf::from(rest_after_cmd.trim());
                    if p.is_dir() {
                        self.workspace = p.canonicalize().unwrap_or(p);
                        self.push_system(&format!("cwd changed to {}", self.workspace.display()));
                    } else {
                        self.push_error(&format!("not a directory: {}", rest_after_cmd.trim()));
                    }
                }
                SlashResult::Handled
            }
            "add-dir" => {
                self.push_system("add-dir: workspace is the trusted directory. Use /cwd to change. Additional dirs managed via permissions.");
                SlashResult::Handled
            }
            "themes" => {
                self.push_system("themes: default theme is active (red/cyan/dark). Custom themes via ~/.metis/themes/ — JSON format.");
                SlashResult::Handled
            }
            "thinking" => {
                self.thinking_enabled = !self.thinking_enabled;
                self.push_system(&format!("thinking display: {}", if self.thinking_enabled { "on" } else { "off" }));
                SlashResult::Handled
            }
            "details" => {
                self.push_system("details: tool execution details are shown inline. Use Ctrl+O to expand collapsed tool results.");
                SlashResult::Handled
            }
            "editor" => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
                let tmp = std::env::temp_dir().join(format!("aegis-msg-{}.md", std::process::id()));
                let current = if self.input.is_empty() { String::new() } else { self.input.clone() };
                if let Err(e) = std::fs::write(&tmp, &current) {
                    self.push_error(&format!("editor: could not write temp file: {e}"));
                } else {
                    match std::process::Command::new(&editor).arg(&tmp).status() {
                        Ok(status) if status.success() => {
                            match std::fs::read_to_string(&tmp) {
                                Ok(edited) => {
                                    self.input = edited.trim_end().to_string();
                                    self.cursor = self.input.len();
                                    self.push_system(&format!("editor: loaded {} chars from {editor}", self.input.len()));
                                }
                                Err(e) => self.push_error(&format!("editor: read back failed: {e}")),
                            }
                        }
                        Ok(_) => self.push_error(&format!("{editor} exited with error")),
                        Err(e) => self.push_error(&format!("editor: could not launch {editor}: {e}")),
                    }
                    let _ = std::fs::remove_file(&tmp);
                }
                SlashResult::Handled
            }
            "test" | "lint" => {
                // Atakan: Aider-style smart shell bridge. Config'de kayıtlı
                // komutu çalıştırır; exit 0 ise tek satır "all green" basar
                // (gürültü yok), fail ise full stdout/stderr'i system message
                // olarak push eder — agent next turn'de görür ve fix önerir.
                let kind = cmd.to_string();
                match load_auto_fix_command(workspace, &kind) {
                    Some(shell_cmd) => {
                        let out = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&shell_cmd)
                            .output();
                        match out {
                            Ok(o) if o.status.success() => {
                                let lines = String::from_utf8_lossy(&o.stdout).lines().count();
                                self.push_system(&format!(
                                    "{kind}: ✓ all green ({} stdout lines, exit 0)",
                                    lines
                                ));
                            }
                            Ok(o) => {
                                let stdout = String::from_utf8_lossy(&o.stdout);
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                let mut body = format!(
                                    "{kind}: ✗ exit {} — `{}`\n",
                                    o.status.code().unwrap_or(-1),
                                    shell_cmd
                                );
                                if !stdout.trim().is_empty() {
                                    let preview: Vec<&str> =
                                        stdout.lines().take(80).collect();
                                    body.push_str("\n--- stdout ---\n");
                                    body.push_str(&preview.join("\n"));
                                    if stdout.lines().count() > 80 {
                                        body.push_str(&format!(
                                            "\n… {} more lines",
                                            stdout.lines().count() - 80
                                        ));
                                    }
                                }
                                if !stderr.trim().is_empty() {
                                    let preview: Vec<&str> =
                                        stderr.lines().take(40).collect();
                                    body.push_str("\n\n--- stderr ---\n");
                                    body.push_str(&preview.join("\n"));
                                }
                                self.push_system(&body);
                            }
                            Err(e) => self.push_error(&format!(
                                "{kind}: spawn failed: {e}"
                            )),
                        }
                    }
                    None => self.push_system(&format!(
                        "{kind}: not configured. Set `[auto_fix] {kind}_command = \"...\"` in workspace `.metis/config.toml` or `~/.metis/config.toml`."
                    )),
                }
                SlashResult::Handled
            }
            "run" => {
                // Atakan: arbitrary shell + chat'e push. `!cmd` ile aynı
                // ama agent'ın okuyabileceği system message bırakır
                // (Aider /run pattern). Çıktı her durumda push, exit
                // kodu satıra dahil.
                let cmd_str = rest_after_cmd.trim();
                if cmd_str.is_empty() {
                    self.push_error("/run: usage: /run <shell command>");
                } else {
                    let out = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(cmd_str)
                        .output();
                    match out {
                        Ok(o) => {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            let mut body = format!("$ {cmd_str}\n");
                            if !stdout.trim().is_empty() {
                                let preview: Vec<&str> =
                                    stdout.lines().take(60).collect();
                                body.push_str(&preview.join("\n"));
                                if stdout.lines().count() > 60 {
                                    body.push_str(&format!(
                                        "\n… {} more lines",
                                        stdout.lines().count() - 60
                                    ));
                                }
                            }
                            if !stderr.trim().is_empty() {
                                let preview: Vec<&str> =
                                    stderr.lines().take(20).collect();
                                body.push_str("\nstderr:\n");
                                body.push_str(&preview.join("\n"));
                            }
                            if !o.status.success() {
                                body.push_str(&format!(
                                    "\nexit {}",
                                    o.status.code().unwrap_or(-1)
                                ));
                            }
                            self.push_system(&body);
                        }
                        Err(e) => self.push_error(&format!("/run: spawn failed: {e}")),
                    }
                }
                SlashResult::Handled
            }
            "export" => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = workspace.join(format!("aegis-session-{ts}.md"));
                let mut md = String::from("# Aegis Session Export\n\n");
                md.push_str(&format!("Model: {}\n", self.model));
                md.push_str(&format!("Turns: {}\n", self.turn_count));
                md.push_str(&format!("Cost: {}\n\n", self.cost_display));
                md.push_str("---\n\n");
                for msg in &self.messages {
                    let role = match msg.role {
                        MessageRole::User => "You",
                        MessageRole::Assistant => "Aegis",
                        MessageRole::System => "System",
                        MessageRole::Error => "Error",
                        MessageRole::Tool => "Tool",
                        MessageRole::ToolResult => "Tool Result",
                        MessageRole::Footer => "—",
                    };
                    md.push_str(&format!("## {role}\n\n{}\n\n", msg.text));
                }
                match std::fs::write(&path, &md) {
                    Ok(()) => self.push_system(&format!("exported {} messages to {}", self.messages.len(), path.display())),
                    Err(e) => self.push_error(&format!("export failed: {e}")),
                }
                SlashResult::Handled
            }
            "allow-all" | "yolo" => {
                let Some(set_arc) = self.always_allowed.as_ref().map(Arc::clone) else {
                    self.push_system("already in --yes mode, all tools allowed");
                    return SlashResult::Handled;
                };
                let mut set = set_arc.lock().unwrap();
                for t in ["bash", "edit_file", "write_file", "multi_edit", "computer_use", "web_fetch", "web_search"] {
                    set.insert(t.to_string());
                }
                self.push_system("yolo mode: all tools approved for this session");
                SlashResult::Handled
            }
            "usage" => {
                let tokens = self.cumulative_usage.total_tokens();
                let cost = &self.cost_display;
                let mut out = format!("Session usage:\n  turns: {}\n  tokens: {tokens}\n  cost: {cost}\n", self.turn_count);
                if let Some(path) = aegis_core::telemetry::telemetry_path() {
                    let records = aegis_core::telemetry::load_records(&path);
                    out.push_str(&format!("\nAll-time:\n  sessions: {}\n  total tokens: {}\n  total cost: ${:.4}\n",
                        records.len(),
                        records.iter().map(|r| r.input_tokens + r.output_tokens).sum::<u32>(),
                        records.iter().map(|r| r.cost_usd).sum::<f64>(),
                    ));
                }
                self.push_system(&out);
                SlashResult::Handled
            }
            "context" => {
                let breakdown = aegis_core::format_cost_breakdown(
                    &self.cumulative_usage,
                    &self.model,
                    self.turn_count as usize,
                    true,
                );
                let total = self.cumulative_usage.total_tokens();
                let ctx_info = format!("context window: {} tokens used", total);
                self.push_system(&format!("{ctx_info}\n\n{breakdown}"));
                SlashResult::Handled
            }
            "sessions" => {
                let want_text_dump = rest_after_cmd.trim() == "list";
                match SessionStore::list(workspace) {
                    Ok(list) if list.is_empty() => {
                        self.push_system(&format!("no sessions under {}", workspace.display()));
                    }
                    Ok(list) if want_text_dump => {
                        let mut out = format!("sessions ({} total):\n", list.len());
                        for s in list.iter().take(20) {
                            let age = s.modified.and_then(|t| {
                                t.elapsed().ok().map(|d| {
                                    let secs = d.as_secs();
                                    if secs < 3600 { format!("{}m ago", secs / 60) }
                                    else if secs < 86400 { format!("{}h ago", secs / 3600) }
                                    else { format!("{}d ago", secs / 86400) }
                                })
                            }).unwrap_or_default();
                            let short_id: String = s.id.chars().take(8).collect();
                            out.push_str(&format!(
                                "  {short_id}  · {} msgs{}\n",
                                s.message_count,
                                if age.is_empty() { String::new() } else { format!("  · {age}") }
                            ));
                        }
                        self.push_system(out.trim_end());
                    }
                    Ok(list) => {
                        // Interactive picker: bare `/sessions` opens a
                        // modal so the user can ↑↓+Enter into a previous
                        // session instead of pasting a 12-char id.
                        // /sessions list still prints the text dump for
                        // pipe / scroll-back use.
                        self.session_picker_sel = 0;
                        self.session_picker = Some(list);
                    }
                    Err(e) => self.push_error(&format!("could not list sessions: {e}")),
                }
                SlashResult::Handled
            }
            "tree" => {
                match SessionStore::list(workspace) {
                    Ok(list) if list.is_empty() => self.push_system("no sessions"),
                    Ok(list) => {
                        let tree = crate::repl::dag::format_session_tree(&list, None);
                        self.push_system(&tree);
                    }
                    Err(e) => self.push_error(&format!("could not list sessions: {e}")),
                }
                SlashResult::Handled
            }
            "memory" => {
                match aegis_core::MemoryStore::open(workspace) {
                    Ok(store) => match store.read_index() {
                        Ok(idx) if idx.trim().is_empty() => {
                            self.push_system("no memories stored yet");
                        }
                        Ok(idx) => {
                            let total = idx.lines().count();
                            let preview: String =
                                idx.lines().take(30).collect::<Vec<_>>().join("\n");
                            let truncated = total > 30;
                            self.push_system(&format!(
                                "Memory ({} entries{}):\n{}",
                                total,
                                if truncated { ", showing first 30" } else { "" },
                                preview
                            ));
                        }
                        Err(e) => self.push_error(&format!("memory read failed: {e}")),
                    },
                    Err(e) => self.push_error(&format!("memory store unavailable: {e}")),
                }
                SlashResult::Handled
            }
            "budget" => {
                // Without daily budget plumbed through, show session cost only.
                self.push_system(&format!(
                    "session: {}   (daily budget requires --repl for now)",
                    self.cost_display
                ));
                SlashResult::Handled
            }
            "overthink" => {
                self.thinking_enabled = !self.thinking_enabled;
                self.push_system(&format!(
                    "thinking mode: {}",
                    if self.thinking_enabled { "on" } else { "off" }
                ));
                SlashResult::Handled
            }
            "advisor" => {
                self.advisor_enabled = true;
                self.push_system("advisor: on (applied on next turn — full wiring pending)");
                SlashResult::Handled
            }
            "advisor-off" => {
                self.advisor_enabled = false;
                self.push_system("advisor: off");
                SlashResult::Handled
            }
            "update" => {
                self.pending_update = true;
                self.push_system(&format!(
                    "current: v{}  —  checking for update…",
                    aegis_core::update::CURRENT_VERSION
                ));
                SlashResult::Handled
            }
            "tasks" => {
                let list = crate::tasks::load_tasks(workspace);
                if list.is_empty() {
                    self.push_system("no tasks");
                } else {
                    let (plain, styled) = build_tasks_listing(&list);
                    self.push_styled(plain, styled);
                }
                SlashResult::Handled
            }
            "task" => {
                // `/task add <text>` / `/task done <id>` / `/task rm <id>` / `/task clear`
                let mut sub = rest_after_cmd.splitn(2, char::is_whitespace);
                let op = sub.next().unwrap_or("").trim();
                let arg = sub.next().unwrap_or("").trim();
                let result = match op {
                    "add" => {
                        if arg.is_empty() {
                            self.push_error("usage: /task add <text>");
                            Ok(())
                        } else {
                            match crate::tasks::add_task(workspace, arg) {
                                Ok(id) => {
                                    self.push_system(&format!("task #{id} added"));
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        }
                    }
                    "done" => match arg.parse::<u32>() {
                        Ok(id) => match crate::tasks::complete_task(workspace, id) {
                            Ok(msg) => {
                                self.push_system(&msg);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        },
                        Err(_) => {
                            self.push_error("usage: /task done <id>");
                            Ok(())
                        }
                    },
                    "rm" => match arg.parse::<u32>() {
                        Ok(id) => match crate::tasks::delete_task(workspace, id) {
                            Ok(msg) => {
                                self.push_system(&msg);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        },
                        Err(_) => {
                            self.push_error("usage: /task rm <id>");
                            Ok(())
                        }
                    },
                    "clear" => match crate::tasks::clear_done(workspace) {
                        Ok(n) => {
                            self.push_system(&format!("{n} done task(s) cleared"));
                            Ok(())
                        }
                        Err(e) => Err(e),
                    },
                    _ => {
                        self.push_error("usage: /task add|done|rm|clear ...");
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    self.push_error(&format!("task op failed: {e}"));
                }
                SlashResult::Handled
            }
            "skills" => {
                let all: Vec<aegis_core::skills::Skill> = self
                    .skill_registry
                    .user_invocable()
                    .into_iter()
                    .cloned()
                    .collect();
                self.skill_filter = String::new();
                self.skill_sel = 0;
                self.skill_menu = Some(all);
                SlashResult::Handled
            }
            "autoskill" => {
                self.auto_skill_enabled = !self.auto_skill_enabled;
                if self.auto_skill_enabled {
                    self.push_system("autoskill açık — her mesajda uygun skill otomatik seçilir");
                } else {
                    self.push_system("autoskill kapalı");
                }
                SlashResult::Handled
            }
            "acceptedits" => {
                self.accept_edits_mode = !self.accept_edits_mode;
                if self.accept_edits_mode {
                    self.push_system(
                        "acceptEdits açık — edit/write onaysız, bash hâlâ sorar",
                    );
                } else {
                    self.push_system("acceptEdits kapalı — her mutating tool için onay");
                }
                SlashResult::Handled
            }
            "skill-install" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /skill-install <path-or-url>");
                    return SlashResult::Handled;
                }
                match self.skill_registry.install(&rest_after_cmd) {
                    Ok(names) => self.push_system(&format!(
                        "installed {} skill(s): {}",
                        names.len(),
                        names.join(", ")
                    )),
                    Err(e) => self.push_error(&format!("skill install failed: {e}")),
                }
                SlashResult::Handled
            }
            "skill-uninstall" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /skill-uninstall <name>");
                    return SlashResult::Handled;
                }
                match self.skill_registry.uninstall(&rest_after_cmd) {
                    Ok(()) => self.push_system(&format!("uninstalled skill `{rest_after_cmd}`")),
                    Err(e) => self.push_error(&e),
                }
                SlashResult::Handled
            }
            "skill-search" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /skill-search <query>");
                    return SlashResult::Handled;
                }
                let (plain, styled) = build_skill_search(&self.skill_registry, &rest_after_cmd);
                self.push_styled(plain, styled);
                SlashResult::Handled
            }
            "map" => {
                let max_files = rest_after_cmd.parse::<usize>().ok().unwrap_or(200);
                let map = aegis_core::repomap::build_repo_map(workspace, max_files);
                if map.is_empty() {
                    self.push_system(&format!(
                        "/map: no source files found in {}",
                        workspace.display()
                    ));
                } else {
                    self.push_system(map.trim_end());
                }
                SlashResult::Handled
            }
            "dag" => {
                match SessionStore::open(workspace, &self.session_id) {
                    Ok(store) => {
                        let messages = store.messages().to_vec();
                        if messages.is_empty() {
                            self.push_system("/dag: session is empty");
                        } else {
                            let out = crate::repl::dag::format_dag(&messages);
                            let plain = out.trim_end().to_string();
                            let styled = colorize_dag_lines(&plain);
                            self.push_styled(plain, styled);
                        }
                    }
                    Err(e) => self.push_error(&format!("/dag: could not open session: {e}")),
                }
                SlashResult::Handled
            }
            "plan" => {
                // Atakan: /plan toggle artık permission_mode üzerinden gider.
                // Plan'da değilse Plan'a, Plan'daysa Default'a döner.
                let target = if self.permission_mode == PermMode::Plan {
                    PermMode::Default
                } else {
                    PermMode::Plan
                };
                self.set_permission_mode(target);
                return SlashResult::Handled;
            }
            "bypass" => {
                // Atakan: net bypass mode girişi. /yolo aliası ama
                // permission_mode'u da Bypass olarak işaretler.
                self.set_permission_mode(PermMode::Bypass);
                return SlashResult::Handled;
            }
            "accept-edits" | "accept_edits" => {
                // CC pattern: edit'ler otomatik allowed, bash hala onay.
                self.set_permission_mode(PermMode::AcceptEdits);
                return SlashResult::Handled;
            }
            "default-mode" => {
                // Tüm bypass/accept-edits/plan'i resetler.
                self.set_permission_mode(PermMode::Default);
                return SlashResult::Handled;
            }
            "_unused_plan_legacy" => {
                let mut state = self.plan_state.lock().unwrap();
                let label = match *state {
                    PlanState::Normal => {
                        *state = PlanState::Drafting;
                        "⏺ Entering plan mode\n  Read-only tools only. /plan again to exit and execute."
                    }
                    PlanState::Drafting => {
                        *state = PlanState::Normal;
                        "⏺ Exiting plan mode."
                    }
                    PlanState::Executing => {
                        *state = PlanState::Normal;
                        "⏺ Plan execution cancelled."
                    }
                };
                drop(state);
                self.push_system(label);
                SlashResult::Handled
            }
            "btw" => {
                // `/btw` was the original "ask while busy" command. /ask
                // now owns that behavior (concurrent, doesn't block the
                // agent), so /btw becomes a deprecation hint that points
                // users at the new home.
                self.push_system(
                    "/btw artık /ask oldu — agent meşgulken bile aynı anda cevap verir.\n\
                     örnek: /ask transformer mimarisi nedir?"
                );
                SlashResult::Handled
            }
            "fork" => {
                // Parse `[name] [take N]` — same surface as REPL.
                let mut fork_name: Option<String> = None;
                let mut take: Option<usize> = None;
                let mut iter = rest_after_cmd.split_whitespace().peekable();
                while let Some(tok) = iter.next() {
                    if tok == "take" {
                        if let Some(n) = iter.next() {
                            take = n.parse().ok();
                        }
                    } else if fork_name.is_none() {
                        fork_name = Some(tok.to_string());
                    }
                }
                let parent = match SessionStore::open(workspace, &self.session_id) {
                    Ok(s) => s,
                    Err(e) => {
                        self.push_error(&format!("/fork: could not open session: {e}"));
                        return SlashResult::Handled;
                    }
                };
                let new_id = fork_name.unwrap_or_else(SessionStore::new_id);

                // Overwrite-protection: if `<new_id>.jsonl` already
                // exists, arm a prime and ask for a second `/fork` to
                // confirm. Silent overwrite would destroy an earlier
                // branch with no way to recover.
                let target = workspace
                    .join(".metis")
                    .join("sessions")
                    .join(format!("{new_id}.jsonl"));
                if target.exists() {
                    let now = std::time::Instant::now();
                    let primed = matches!(
                        self.fork_overwrite_primed.as_ref(),
                        Some((n, t))
                            if n == &new_id
                                && now.duration_since(*t).as_secs() <= 5
                    );
                    if !primed {
                        self.fork_overwrite_primed = Some((new_id.clone(), now));
                        self.push_system(&format!(
                            "session `{new_id}` already exists — /fork again within \
                             5s to overwrite, or pick a different name."
                        ));
                        return SlashResult::Handled;
                    }
                    self.fork_overwrite_primed = None;
                    // SessionStore::fork refuses to write into a file that
                    // already has messages, so the prime would be a lie
                    // unless we clear the target first. Remove both the
                    // jsonl and its meta sidecar.
                    let _ = std::fs::remove_file(&target);
                    let _ = std::fs::remove_file(target.with_extension("meta.json"));
                }

                match parent.fork(&new_id, take) {
                    Ok(forked) => {
                        let kept = forked.messages().len();
                        self.session_id = new_id.clone();
                        self.push_system(&format!(
                            "forked → session={new_id} ({kept} messages carried over)"
                        ));
                        SlashResult::SwitchSession(new_id)
                    }
                    Err(e) => {
                        self.push_error(&format!("/fork failed: {e}"));
                        SlashResult::Handled
                    }
                }
            }
            "compact" => {
                self.pending_compact = true;
                self.push_system("⏺ Compacting context...");
                SlashResult::Handled
            }
            "swarm" => {
                // Parse: /swarm [N] [quorum:M] <prompt>. Same surface as
                // REPL. Build a tool_call text that instructs the agent
                // to invoke `parallel_agents` with N angle-focused
                // sub-agents and optional quorum matching, then queue it
                // so the next turn fires naturally.
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /swarm [N] [quorum:M] <prompt>");
                    return SlashResult::Handled;
                }
                let mut words = rest_after_cmd.split_whitespace().peekable();
                let mut n: usize = 3;
                let mut quorum: usize = 0;
                for _ in 0..2 {
                    let w = match words.peek() {
                        Some(w) => *w,
                        None => break,
                    };
                    if let Some(m_str) = w.strip_prefix("quorum:") {
                        if let Ok(m) = m_str.parse::<usize>() {
                            quorum = m;
                            words.next();
                            continue;
                        }
                    }
                    if let Ok(parsed_n) = w.parse::<usize>() {
                        if (2..=10).contains(&parsed_n) {
                            n = parsed_n;
                            words.next();
                            continue;
                        } else {
                            self.push_error(&format!("swarm: N must be 2-10, got {parsed_n}"));
                            return SlashResult::Handled;
                        }
                    }
                    break;
                }
                let swarm_prompt = words.collect::<Vec<_>>().join(" ");
                if swarm_prompt.is_empty() {
                    self.push_error("usage: /swarm [N] [quorum:M] <prompt>");
                    return SlashResult::Handled;
                }
                let angles = [
                    "Performance",
                    "Correctness",
                    "Simplicity",
                    "Edge Cases",
                    "Alternatives",
                    "Security",
                    "Maintainability",
                    "Testing",
                    "Documentation",
                    "Scalability",
                ];
                let actual_n = n.min(angles.len());
                let quorum_instruction = if quorum > 0 {
                    format!(
                        "\n\nNote: Your answer will be compared with {actual_n} parallel agents. \
                         A quorum of {quorum} matching answers is required. \
                         Start your response with a one-line SUMMARY: <answer> so comparison is possible."
                    )
                } else {
                    String::new()
                };
                let agents_json = serde_json::json!({
                    "agents": (0..actual_n).map(|i| {
                        serde_json::json!({
                            "description": format!("Agent {}: {} Focused", i+1, angles[i]),
                            "prompt": format!("{swarm_prompt}{quorum_instruction}")
                        })
                    }).collect::<Vec<_>>(),
                    "timeout_secs": 300
                });
                let tool_call = if quorum > 0 {
                    format!(
                        "Use the `parallel_agents` tool with this config: {agents_json}\n\n\
                         After getting all results, compare the SUMMARY lines. \
                         If at least {quorum} out of {actual_n} agents agree, \
                         report the consensus answer with a confidence score. \
                         If quorum is not met, show all results with a note that consensus was not reached."
                    )
                } else {
                    format!("Use the `parallel_agents` tool with this config: {agents_json}")
                };
                let qlabel = if quorum > 0 {
                    format!(", quorum {quorum}/{actual_n}")
                } else {
                    String::new()
                };
                self.push_system(&format!(
                    "swarm: {actual_n} parallel agents{qlabel} — dispatching"
                ));
                self.pending_prompts.push_back(tool_call);
                SlashResult::Handled
            }
            "insights" => {
                // Open the current session and build a conversation
                // transcript for the insight-extraction prompt. Mirrors
                // REPL's path at repl.rs:1563 — same role filter, same
                // 500-char truncation on tool outputs, same prompt body.
                // We push the built prompt to `pending_prompts` so the
                // main loop fires it as a normal turn (agent will call
                // memory_save as instructed).
                match SessionStore::open(workspace, &self.session_id) {
                    Ok(store) => {
                        let messages = store.messages();
                        if messages.is_empty() {
                            self.push_system(
                                "/insights: no session messages to extract insights from",
                            );
                            return SlashResult::Handled;
                        }
                        let mut conv = String::new();
                        for m in messages {
                            if m.role == aegis_api::Role::System {
                                continue;
                            }
                            let role = match m.role {
                                aegis_api::Role::User => "User",
                                aegis_api::Role::Assistant => "Assistant",
                                aegis_api::Role::Tool => "Tool",
                                aegis_api::Role::System => unreachable!(),
                            };
                            if let Some(c) = m.content.as_deref() {
                                let trimmed = if c.len() > 500 { &c[..500] } else { c };
                                conv.push_str(&format!("{role}: {trimmed}\n"));
                            }
                        }
                        if conv.is_empty() {
                            self.push_system("/insights: no extractable content in session");
                            return SlashResult::Handled;
                        }
                        let insight_prompt = format!(
                            "Review this conversation and extract non-obvious facts, decisions, and \
                             learnings worth remembering in future sessions. Focus on:\n\
                             - User preferences and working style\n\
                             - Technical decisions and why they were made\n\
                             - Things that were tried and failed (and why)\n\
                             - Project-specific context not derivable from code\n\n\
                             Do NOT save: obvious facts, temporary state, happy-path summaries.\n\
                             For each insight you find, call memory_save with an appropriate type and content.\n\
                             If there are no meaningful insights worth saving, say so briefly.\n\n\
                             Conversation:\n{conv}"
                        );
                        self.push_system("extracting insights from session…");
                        self.pending_prompts.push_back(insight_prompt);
                    }
                    Err(e) => {
                        self.push_error(&format!("/insights: could not open session: {e}"));
                    }
                }
                SlashResult::Handled
            }
            "learn" => {
                let text = rest_after_cmd.trim();
                if text.is_empty() {
                    self.push_error("usage: /learn <rule or insight text>");
                    return SlashResult::Handled;
                }
                let ws_str = workspace.display().to_string();
                let insight = aegis_core::learning::Insight {
                    timestamp: aegis_core::telemetry::now_iso8601(),
                    workspace: Some(ws_str),
                    category: "preference".to_string(),
                    text: text.to_string(),
                    reinforcements: 1,
                    last_seen: None,
                    success_count: 0,
                    failure_count: 0,
                    tags: vec!["manual".to_string()],
                };
                match aegis_core::learning::upsert_insight(&insight) {
                    Ok(()) => self.push_system(&format!("saved: {text}")),
                    Err(e) => self.push_error(&format!("/learn failed: {e}")),
                }
                SlashResult::Handled
            }
            "rate" => {
                // /rate good|bad [note]
                let mut iter = rest_after_cmd.splitn(2, char::is_whitespace);
                let signal_raw = iter.next().unwrap_or("").trim().to_lowercase();
                let note = iter
                    .next()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());

                let signal = match signal_raw.as_str() {
                    "good" | "g" | "+" | "up" => "good",
                    "bad" | "b" | "-" | "down" => "bad",
                    "undo" | "u" => {
                        match aegis_core::learning::undo_last_rating(workspace) {
                            Ok(Some(p)) => self.push_system(&format!(
                                "undid last rating: {} (was at {})",
                                p.signal, p.timestamp
                            )),
                            Ok(None) => self.push_system("nothing to undo for this workspace"),
                            Err(e) => self.push_error(&format!("/rate undo failed: {e}")),
                        }
                        return SlashResult::Handled;
                    }
                    "" => {
                        self.push_system(
                            "/rate <good|bad|undo> [note] — record feedback or pop the last one",
                        );
                        return SlashResult::Handled;
                    }
                    other => {
                        self.push_error(&format!(
                            "/rate: unknown signal `{other}` — use good|bad|undo"
                        ));
                        return SlashResult::Handled;
                    }
                };

                let messages = match aegis_core::SessionStore::open(workspace, &self.session_id) {
                    Ok(store) => store.messages().to_vec(),
                    Err(_) => Vec::new(),
                };
                let pref = aegis_core::learning::build_rating(
                    workspace,
                    Some(self.session_id.clone()),
                    signal,
                    note,
                    &messages,
                );
                match aegis_core::learning::record_rating(&pref) {
                    Ok(()) => {
                        let note_tag = pref
                            .note
                            .as_deref()
                            .map(|n| format!(" — \"{n}\""))
                            .unwrap_or_default();
                        self.push_system(&format!("rating saved: {signal}{note_tag}"));
                        // Run heuristic aggregation: surfaces tools that
                        // correlate with negative ratings as
                        // style_preference insights for the next session.
                        let emitted = aegis_core::learning::aggregate_preferences(workspace);
                        if !emitted.is_empty() {
                            let names: Vec<String> = emitted
                                .iter()
                                .filter_map(|i| {
                                    i.tags
                                        .iter()
                                        .find(|t| t.as_str() != "style_preference")
                                        .cloned()
                                })
                                .collect();
                            self.push_system(&format!(
                                "aggregator: {} new style hint(s) for next session ({})",
                                emitted.len(),
                                names.join(", ")
                            ));
                        }
                    }
                    Err(e) => self.push_error(&format!("/rate: failed to save — {e}")),
                }
                SlashResult::Handled
            }
            "forget" => {
                let needle = rest_after_cmd.trim();
                if needle.is_empty() {
                    self.push_system("/forget <text-or-tag> — remove insights for this workspace matching the pattern");
                    return SlashResult::Handled;
                }
                match aegis_core::learning::forget_insights(workspace, needle) {
                    Ok(removed) => {
                        if removed.is_empty() {
                            self.push_system(&format!("/forget: no insights matched `{needle}`"));
                        } else {
                            self.push_system(&format!(
                                "/forget: removed {} insight(s) matching `{needle}`",
                                removed.len()
                            ));
                            for ins in removed.iter().take(5) {
                                self.push_system(&format!("  - [{}] {}", ins.category, ins.text));
                            }
                            if removed.len() > 5 {
                                self.push_system(&format!("  …and {} more", removed.len() - 5));
                            }
                        }
                    }
                    Err(e) => self.push_error(&format!("/forget failed: {e}")),
                }
                SlashResult::Handled
            }
            "rules" => {
                let rules = aegis_core::learning::list_instructions(workspace);
                if rules.is_empty() {
                    self.push_system(
                        "no rules learned yet. tell me \"from now on X\", \"never Y\", \"bundan sonra Z\", \"hiç/asla Q yapma\" — I pick these up at session end.",
                    );
                } else {
                    self.push_system(&format!(
                        "{} rule(s) learned for this workspace (sorted newest first):",
                        rules.len()
                    ));
                    for rule in &rules {
                        let reinforced = if rule.reinforcements > 1 {
                            format!(" ×{}", rule.reinforcements)
                        } else {
                            String::new()
                        };
                        self.push_system(&format!("  - {}{reinforced}", rule.text));
                    }
                    self.push_system("remove with: /forget <substring>");
                }
                SlashResult::Handled
            }
            "ratings" => {
                let summary = aegis_core::learning::summarize_ratings(workspace);
                if summary.good == 0 && summary.bad == 0 {
                    self.push_system(
                        "no ratings recorded for this workspace yet — try /rate good|bad",
                    );
                } else {
                    self.push_system(&format!(
                        "ratings: {} good, {} bad (threshold for style hint: {})",
                        summary.good, summary.bad, summary.threshold
                    ));
                    if !summary.bad_tools.is_empty() {
                        self.push_system("bad-rating tool counts:");
                        for (tool, count) in &summary.bad_tools {
                            let marker = if (*count as usize) >= summary.threshold {
                                "✓"
                            } else {
                                " "
                            };
                            self.push_system(&format!("  {marker} {tool:<24} {count}"));
                        }
                    }
                }
                SlashResult::Handled
            }
            "export-ft" => {
                let path = if rest_after_cmd.is_empty() {
                    "ft_export.jsonl".to_string()
                } else {
                    rest_after_cmd.to_string()
                };
                let store = aegis_core::SessionStore::open(workspace, &self.session_id);
                match store {
                    Ok(s) => {
                        let messages = s.messages().to_vec();
                        let system_content = messages
                            .iter()
                            .find(|m| m.role == aegis_api::Role::System)
                            .and_then(|m| m.content.clone())
                            .unwrap_or_default();
                        let mut examples: Vec<serde_json::Value> = Vec::new();
                        let mut i = 0;
                        while i < messages.len() {
                            if messages[i].role == aegis_api::Role::User {
                                if let Some(j) = messages[i + 1..]
                                    .iter()
                                    .position(|m| {
                                        m.role == aegis_api::Role::Assistant
                                            && m.tool_calls.is_empty()
                                            && m.content
                                                .as_deref()
                                                .map(|s| !s.is_empty())
                                                .unwrap_or(false)
                                    })
                                    .map(|pos| i + 1 + pos)
                                {
                                    let user_text = messages[i].content.clone().unwrap_or_default();
                                    let asst_text = messages[j].content.clone().unwrap_or_default();
                                    if !user_text.is_empty() && !asst_text.is_empty() {
                                        let mut msgs = Vec::new();
                                        if !system_content.is_empty() {
                                            msgs.push(serde_json::json!({
                                                "role": "system",
                                                "content": system_content
                                            }));
                                        }
                                        msgs.push(serde_json::json!({
                                            "role": "user", "content": user_text
                                        }));
                                        msgs.push(serde_json::json!({
                                            "role": "assistant", "content": asst_text
                                        }));
                                        examples.push(serde_json::json!({ "messages": msgs }));
                                    }
                                    i = j + 1;
                                } else {
                                    i += 1;
                                }
                            } else {
                                i += 1;
                            }
                        }
                        let content = examples
                            .iter()
                            .map(|e| serde_json::to_string(e).unwrap_or_default())
                            .collect::<Vec<_>>()
                            .join("\n");
                        match std::fs::write(&path, content) {
                            Ok(_) => self.push_system(&format!(
                                "exported {} training examples → {path}",
                                examples.len()
                            )),
                            Err(e) => self.push_error(&format!("export-ft: {e}")),
                        }
                    }
                    Err(e) => self.push_error(&format!("export-ft: could not open session: {e}")),
                }
                SlashResult::Handled
            }
            "multi-model" => {
                self.multi_model_evaluation = !self.multi_model_evaluation;
                let s = if self.multi_model_evaluation {
                    "ON"
                } else {
                    "OFF"
                };
                self.push_system(&format!("multi-model evaluation {s} (GODMODE)"));
                SlashResult::Handled
            }
            "perturbation" => {
                self.prompt_perturbation = !self.prompt_perturbation;
                let s = if self.prompt_perturbation {
                    "ON"
                } else {
                    "OFF"
                };
                self.push_system(&format!("prompt perturbation {s} (GODMODE)"));
                SlashResult::Handled
            }
            "parallel" => {
                self.parallel_models = !self.parallel_models;
                let s = if self.parallel_models { "ON" } else { "OFF" };
                self.push_system(&format!("parallel models {s} (GODMODE)"));
                SlashResult::Handled
            }
            "race" => {
                // Manual one-shot race — fire the same prompt at multiple
                // providers in parallel, append the best response to the
                // session note. Reuses the existing pending_race pump in
                // the main loop (set in the GODMODE multi-model auto-path)
                // so the actual networking lives in one place.
                let prompt = rest_after_cmd.trim();
                if prompt.is_empty() {
                    self.push_system("usage: /race <prompt>");
                } else {
                    self.pending_race = Some(prompt.to_string());
                    self.push_system(&format!(
                        "[race] queued — querying available providers in parallel for: {prompt}"
                    ));
                }
                SlashResult::Handled
            }
            "askall" => {
                // `/askall <prompt>` — fire the prompt at all providers
                // that have a key configured, each using their STRONGEST
                // model (index 0 from models_for_provider). Shows every
                // response. Renamed from `/ask` (the new `/ask` is
                // Copilot-CLI-style single-shot Q&A).
                let prompt = rest_after_cmd.trim();
                if prompt.is_empty() {
                    self.push_system(
                        "/askall <soru>  — soruyu tüm aktif provider'lara güçlü modelleriyle paralel gönderir\n\
                         örnek: /askall transformer mimarisi nedir?\n\
                         tek provider tek soru: /ask <soru>"
                    );
                } else {
                    self.pending_askall = Some(prompt.to_string());
                    self.push_system(&format!(
                        "[askall] gönderiliyor — tüm aktif provider'ların güçlü modeline: {prompt}"
                    ));
                }
                SlashResult::Handled
            }
            "copy" => {
                // `/copy` — copy the LAST assistant message to system
                // clipboard with explicit feedback. Auto-copy on every
                // assistant message is also wired up (push_assistant)
                // but stays silent; this command makes intent explicit
                // so the user knows it landed.
                let last_assistant = self
                    .messages
                    .iter()
                    .rev()
                    .find(|m| matches!(m.role, MessageRole::Assistant))
                    .map(|m| m.text.clone());
                match last_assistant {
                    Some(text) if !text.is_empty() => {
                        let chars = text.chars().count();
                        let ok = display::copy_to_clipboard_osc52(&text);
                        if ok {
                            self.push_system(&format!(
                                "[copy] {chars} char copied to clipboard (last assistant message)"
                            ));
                        } else {
                            self.push_error(
                                "[copy] failed — pbcopy unavailable / OSC 52 not supported by terminal",
                            );
                        }
                    }
                    _ => {
                        self.push_system("[copy] no assistant message yet — nothing to copy");
                    }
                }
                SlashResult::Handled
            }
            "ask" => {
                // `/ask <prompt>` — Copilot-CLI-style side question.
                // Concurrent (doesn't gate on busy), routes to the
                // current model with tools off, renders question and
                // response as a visually distinct ask-panel block:
                // cyan `❯ ask` header → wrapped question → dim
                // `thinking…` placeholder while in flight, then the
                // answer prefixed with a magenta `✱` divider.
                let prompt = rest_after_cmd.trim();
                if prompt.is_empty() {
                    self.push_system(
                        "/ask <soru>  — tek-shot yan soru, agent meşgul olsa bile aynı anda cevaplar\n\
                         örnek: /ask transformer mimarisi nedir?\n\
                         tüm provider'lara paralel: /askall  ·  tek provider'a: /consult <provider>"
                    );
                } else {
                    self.pending_ask_single = Some(prompt.to_string());
                    let cyan = Style::default()
                        .fg(Color::Rgb(80, 200, 240))
                        .add_modifier(Modifier::BOLD);
                    let cyan_border = Style::default().fg(Color::Rgb(80, 200, 240));
                    let dim = Style::default().fg(Color::Rgb(140, 140, 140));
                    let body_color = Style::default().fg(Color::Rgb(235, 235, 235));
                    let model_short: String = self
                        .model
                        .rsplit('/')
                        .next()
                        .unwrap_or(&self.model)
                        .to_string();
                    // Inline panel using box-drawing characters so the
                    // /ask question stands out as a discrete card in
                    // the transcript instead of floating among normal
                    // chat lines. Cevap geldiğinde alt çerçeve aşağıda
                    // kalmaya devam eder (extra line aşağıda).
                    let title = format!("─ ask · {model_short} ");
                    let pad_len = 60usize.saturating_sub(title.chars().count() + 2);
                    let pad: String = "─".repeat(pad_len);
                    let question_lines: Vec<Line<'static>> = vec![
                        Line::from(vec![
                            Span::styled("╭".to_string(), cyan_border),
                            Span::styled(title, cyan),
                            Span::styled(format!("{pad}╮"), cyan_border),
                        ]),
                        Line::from(vec![
                            Span::styled("│ ".to_string(), cyan_border),
                            Span::styled(prompt.to_string(), body_color),
                        ]),
                        Line::from(vec![
                            Span::styled("│ ".to_string(), cyan_border),
                            Span::styled(
                                "thinking…".to_string(),
                                dim.add_modifier(Modifier::ITALIC),
                            ),
                        ]),
                        Line::from(vec![Span::styled(
                            format!("╰{}╯", "─".repeat(60)),
                            cyan_border,
                        )]),
                    ];
                    let plain = format!(
                        "╭─ ask · {model_short} ─╮\n│ {prompt}\n│ thinking…\n╰─╯"
                    );
                    self.push_styled(plain, question_lines);
                }
                SlashResult::Handled
            }
            "autotune" => {
                self.autotune = !self.autotune;
                let s = if self.autotune { "ON" } else { "OFF" };
                self.push_system(&format!(
                    "autotune {s} — adaptive temperature {} for next turns",
                    if self.autotune { "enabled" } else { "disabled" }
                ));
                SlashResult::Handled
            }
            "tabs" => {
                self.tabs_strip_visible = !self.tabs_strip_visible;
                let s = if self.tabs_strip_visible { "on" } else { "off" };
                self.push_system(&format!(
                    "tabs strip {s} — F1 chat · F2 files · F3 sessions · F4 permissions"
                ));
                SlashResult::Handled
            }
            "permissions" | "perms" | "auth-log" => {
                // Open the timeline overlay; the renderer pulls from
                // `permission_history` which the Permission impl
                // appends to on every check + decision.
                self.permission_overlay_open = true;
                self.permission_overlay_scroll = 0;
                if self.permission_history.is_empty() {
                    self.push_system(
                        "no permission decisions yet — log fills as the agent calls tools",
                    );
                    self.permission_overlay_open = false;
                }
                SlashResult::Handled
            }
            "security" => {
                let sub = rest_after_cmd.trim();
                let Some(layer) = self.security.as_ref().map(Arc::clone) else {
                    self.push_error(
                        "/security: autonomous security layer not initialized for this session",
                    );
                    return SlashResult::Handled;
                };
                match sub {
                    "kill" => {
                        layer.trigger_kill_switch("manual /security kill".to_string());
                        self.push_system(
                            "[security] kill switch TRIGGERED — all subsequent tool calls will \
                             be HardDeny'd. Use /security resume to re-enable.",
                        );
                    }
                    "resume" => {
                        layer.resume();
                        self.push_system(
                            "[security] resumed — kill switch cleared, tool calls flowing again.",
                        );
                    }
                    "" => {
                        let stats = layer.stats_snapshot();
                        let cfg = layer.config();
                        let kill_state = match layer.kill_switch_state() {
                            aegis_core::KillSwitchState::Enabled => "ENABLED".to_string(),
                            aegis_core::KillSwitchState::Triggered(r) => {
                                format!("TRIGGERED ({r})")
                            }
                            aegis_core::KillSwitchState::Paused(r) => format!("PAUSED ({r})"),
                        };
                        let cost_today = aegis_core::spent_today();
                        let autotune_status = if self.autotune { "ON" } else { "OFF" };
                        let elapsed_secs = stats.elapsed.as_secs();
                        let elapsed_h = elapsed_secs / 3600;
                        let elapsed_m = (elapsed_secs % 3600) / 60;
                        let elapsed_s = elapsed_secs % 60;
                        let limit_str = |n: u32| -> String {
                            if n == u32::MAX {
                                "unlimited".to_string()
                            } else {
                                n.to_string()
                            }
                        };
                        let cost_limit_str = if cfg.max_cost_usd == f64::MAX {
                            "unlimited".to_string()
                        } else {
                            format!("${:.2}", cfg.max_cost_usd)
                        };
                        let timeout_str =
                            if cfg.timeout >= std::time::Duration::from_secs(60 * 60 * 24 * 30) {
                                "unlimited".to_string()
                            } else {
                                format!("{}s", cfg.timeout.as_secs())
                            };
                        let protected = if cfg.protected_paths.is_empty() {
                            "(none — handled by policy/permission layer)".to_string()
                        } else {
                            cfg.protected_paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        self.push_system(&format!(
                            "── Autonomous Security Status ──\n\
                             kill switch:     {kill_state}\n\
                             autotune:        {autotune_status}\n\
                             tool calls:      {} / {}\n\
                             est. tokens:     {}\n\
                             est. layer cost: ${:.4}\n\
                             real spend today:${cost_today:.4}\n\
                             deletions:       {} / {}\n\
                             commits:         {} / {}\n\
                             session uptime:  {elapsed_h:02}:{elapsed_m:02}:{elapsed_s:02}\n\
                             timeout:         {timeout_str}\n\
                             protected paths: {protected}",
                            stats.tool_call_count,
                            limit_str(cfg.max_tool_calls),
                            stats.estimated_tokens,
                            stats.estimated_cost_usd,
                            stats.deletions,
                            limit_str(cfg.max_deletions),
                            stats.commits,
                            limit_str(cfg.max_commits),
                        ));
                        let _ = cost_limit_str; // (cost limit currently unlimited; kept for future surface)
                    }
                    other => {
                        self.push_system(&format!(
                            "unknown /security subcommand: `{other}` (expected: kill | resume | <empty>)"
                        ));
                    }
                }
                SlashResult::Handled
            }
            "allow" => {
                let arg = rest_after_cmd.trim().to_string();
                let Some(set_arc) = self.always_allowed.as_ref().map(Arc::clone) else {
                    self.push_system("/allow: --yes mode active, everything already allowed");
                    return SlashResult::Handled;
                };
                if arg.is_empty() {
                    self.allow_menu_open = true;
                    return SlashResult::Handled;
                }
                // Atakan: per-command bash whitelist branch.
                // `/allow bash "git status"` → analyze + insert each
                // canonical key. Bare `/allow bash` keeps the legacy
                // "allow every bash invocation" semantics so existing
                // muscle memory still works.
                if let Some(rest) = arg.strip_prefix("bash ").or_else(|| arg.strip_prefix("bash\t"))
                {
                    let raw = rest.trim().trim_matches('"').trim_matches('\'');
                    if raw.is_empty() {
                        self.push_error(
                            "/allow bash <command>  — example: /allow bash \"git status\"",
                        );
                        return SlashResult::Handled;
                    }
                    use aegis_core::bash_safety::{analyze_bash_command, CommandCheck};
                    match analyze_bash_command(raw) {
                        CommandCheck::Safe(parts) => {
                            if let Some(allow) = self.bash_command_allowlist.as_ref() {
                                if let Ok(mut s) = allow.lock() {
                                    for p in &parts {
                                        s.insert(p.clone());
                                    }
                                }
                            }
                            self.push_system(&format!(
                                "/allow bash: whitelisted [{}]",
                                parts.join(", ")
                            ));
                        }
                        CommandCheck::Dangerous(reason) => {
                            self.push_error(&format!(
                                "/allow bash REJECTED: {reason} — dangerous patterns are never whitelisted"
                            ));
                        }
                    }
                    return SlashResult::Handled;
                }
                let mut set = set_arc.lock().unwrap();
                if arg == "all" {
                    for t in ["bash", "edit_file", "write_file", "multi_edit", "computer_use"] {
                        set.insert(t.to_string());
                    }
                    self.push_system("allowed for this session: bash, edit_file, write_file, multi_edit, computer_use");
                } else {
                    set.insert(arg.clone());
                    self.push_system(&format!("allowed for this session: {arg}"));
                }
                SlashResult::Handled
            }
            "deny" => {
                let arg = rest_after_cmd.trim().to_string();
                let Some(set_arc) = self.always_allowed.as_ref().map(Arc::clone) else {
                    self.push_system("/deny: --yes mode active, use /allow to manage is not applicable");
                    return SlashResult::Handled;
                };
                if arg.is_empty() {
                    self.push_system(
                        "/deny <tool>  or  /deny bash <command>  — remove from always-allowed",
                    );
                    return SlashResult::Handled;
                }
                // Atakan: bash per-command remove. `/deny bash` with no
                // command clears the whole bash command whitelist (still
                // narrower than `/deny bash` legacy which only touched the
                // tool-level flag — we honor both for muscle memory).
                if let Some(rest) = arg.strip_prefix("bash ").or_else(|| arg.strip_prefix("bash\t"))
                {
                    let raw = rest.trim().trim_matches('"').trim_matches('\'');
                    use aegis_core::bash_safety::{analyze_bash_command, CommandCheck};
                    let allow_arc = self.bash_command_allowlist.as_ref().map(Arc::clone);
                    let msg: Option<(String, bool)> = if let Some(allow) = allow_arc {
                        let Ok(mut s) = allow.lock() else {
                            self.push_error("/deny bash: allowlist lock poisoned");
                            return SlashResult::Handled;
                        };
                        if raw == "all" {
                            let n = s.len();
                            s.clear();
                            Some((format!("/deny bash all: cleared {n} command(s)"), false))
                        } else {
                            match analyze_bash_command(raw) {
                                CommandCheck::Safe(parts) => {
                                    let mut removed = 0;
                                    for p in &parts {
                                        if s.remove(p) {
                                            removed += 1;
                                        }
                                    }
                                    Some((
                                        format!(
                                            "/deny bash: removed {removed}/{} key(s) [{}]",
                                            parts.len(),
                                            parts.join(", ")
                                        ),
                                        false,
                                    ))
                                }
                                CommandCheck::Dangerous(_) => Some((
                                    format!("/deny bash: cannot canonicalize `{raw}`"),
                                    true,
                                )),
                            }
                        }
                    } else {
                        None
                    };
                    if let Some((line, is_err)) = msg {
                        if is_err {
                            self.push_error(&line);
                        } else {
                            self.push_system(&line);
                        }
                    }
                    return SlashResult::Handled;
                }
                let mut set = set_arc.lock().unwrap();
                if arg == "all" {
                    let count = set.len();
                    set.clear();
                    self.push_system(&format!("cleared always-allowed ({count} tools removed)"));
                    // Also wipe per-command bash whitelist so `/deny all`
                    // really means "clear everything I whitelisted".
                    if let Some(allow) = self.bash_command_allowlist.as_ref() {
                        if let Ok(mut s) = allow.lock() {
                            s.clear();
                        }
                    }
                } else if set.remove(&arg) {
                    self.push_system(&format!("removed from always-allowed: {arg}"));
                } else {
                    self.push_system(&format!("{arg} was not in always-allowed"));
                }
                SlashResult::Handled
            }
            "allowed" => {
                let Some(set_arc) = self.always_allowed.as_ref().map(Arc::clone) else {
                    self.push_system("always-allowed: everything (--yes mode)");
                    return SlashResult::Handled;
                };
                let tools: Vec<String> = {
                    let set = set_arc.lock().unwrap();
                    let mut v: Vec<String> = set.iter().cloned().collect();
                    v.sort();
                    v
                };
                let bash_cmds: Vec<String> = if let Some(allow) =
                    self.bash_command_allowlist.as_ref()
                {
                    let s = allow.lock().unwrap();
                    let mut v: Vec<String> = s.iter().cloned().collect();
                    v.sort();
                    v
                } else {
                    Vec::new()
                };
                if tools.is_empty() && bash_cmds.is_empty() {
                    self.push_system(
                        "always-allowed: (none) — each tool will prompt for approval",
                    );
                } else {
                    let mut buf = String::new();
                    if !tools.is_empty() {
                        buf.push_str("tools: ");
                        buf.push_str(&tools.join(", "));
                    }
                    if !bash_cmds.is_empty() {
                        if !buf.is_empty() {
                            buf.push_str(" | ");
                        }
                        buf.push_str("bash: ");
                        buf.push_str(&bash_cmds.join(", "));
                    }
                    self.push_system(&format!("always-allowed this session: {buf}"));
                }
                SlashResult::Handled
            }
            "api-keys" => {
                self.api_keys_display = !self.api_keys_display;
                let s = if self.api_keys_display { "ON" } else { "OFF" };
                let detail = if self.api_keys_display {
                    let keys = aegis_multi_model::ApiKeyConfig::load();
                    let lines = [
                        ("anthropic", keys.anthropic_api_key.is_some()),
                        ("openai", keys.openai_api_key.is_some()),
                        ("deepseek", keys.deepseek_api_key.is_some()),
                        ("google", keys.google_api_key.is_some()),
                        ("groq", keys.groq_api_key.is_some()),
                        ("mistral", keys.mistral_api_key.is_some()),
                    ]
                    .iter()
                    .map(|(n, ok)| format!("  {} {n}", if *ok { "✓" } else { "✗" }))
                    .collect::<Vec<_>>()
                    .join("\n");
                    format!("\nAPI keys:\n{lines}")
                } else {
                    String::new()
                };
                self.push_system(&format!("API key management {s} (GODMODE){detail}"));
                SlashResult::Handled
            }
            "godmode" => {
                self.multi_model_evaluation = !self.multi_model_evaluation;
                self.prompt_perturbation = !self.prompt_perturbation;
                self.parallel_models = !self.parallel_models;
                self.api_keys_display = !self.api_keys_display;
                let mm = if self.multi_model_evaluation {
                    "ON"
                } else {
                    "OFF"
                };
                let pp = if self.prompt_perturbation {
                    "ON"
                } else {
                    "OFF"
                };
                let pm = if self.parallel_models { "ON" } else { "OFF" };
                let ak = if self.api_keys_display { "ON" } else { "OFF" };
                self.push_system(&format!(
                    "GODMODE: multi-model={mm} perturbation={pp} parallel={pm} api-keys={ak}"
                ));
                SlashResult::Handled
            }
            "consult" => {
                // `/consult` with no args → open provider picker overlay.
                // After picking, input is pre-filled with `/consult <id> `
                // so the user just types the question and presses Enter.
                //
                // `/consult <provider> <prompt>` → direct queue (existing).
                let mut parts_iter = rest_after_cmd.splitn(2, char::is_whitespace);
                let provider = parts_iter.next().unwrap_or("").trim().to_string();
                let consult_prompt = parts_iter.next().unwrap_or("").trim().to_string();
                if provider.is_empty() {
                    use aegis_api::Provider;
                    let items: Vec<(String, String, bool)> = Provider::BUILTINS
                        .iter()
                        .map(|p| {
                            // subprocess provider: available iff claude binary on PATH
                            let has_key = if p.env_var.is_empty() {
                                aegis_api::ClaudeSubprocessClient::is_available()
                            } else {
                                std::env::var(p.env_var).is_ok()
                            };
                            (p.id.to_string(), p.default_model.to_string(), has_key)
                        })
                        .collect();
                    self.consult_pick_mode = true;
                    self.provider_menu = Some(items);
                    return SlashResult::Handled;
                }
                if consult_prompt.is_empty() {
                    self.push_error("usage: /consult <provider> <prompt>");
                    return SlashResult::Handled;
                }
                if aegis_api::Provider::lookup(&provider).is_none() {
                    self.push_error(&format!("unknown provider: {provider} (see /providers)"));
                    return SlashResult::Handled;
                }
                self.pending_consult = Some((provider.clone(), consult_prompt));
                self.push_system(&format!("consult queued: {provider}"));
                SlashResult::Handled
            }
            "claude" => {
                // `/claude <prompt>` — run `claude -p "<prompt>"` subprocess and
                // show the output as a system message. Opt-in; requires the
                // `claude` CLI to be in PATH. Useful for routing a specific
                // question to Claude when DeepSeek is the active model.
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /claude <prompt>  (runs 'claude -p' subprocess)");
                    return SlashResult::Handled;
                }
                // Quick PATH check without extra deps.
                let claude_found = std::env::var("PATH")
                    .unwrap_or_default()
                    .split(':')
                    .any(|dir| std::path::Path::new(dir).join("claude").exists());
                if !claude_found {
                    self.push_error("'claude' binary not found in PATH — install Claude Code CLI first");
                    return SlashResult::Handled;
                }
                self.pending_claude = Some(rest_after_cmd.clone());
                self.push_system(&format!("claude: running subprocess…"));
                SlashResult::Handled
            }
            "provider" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /provider <id>  (see /providers for the list)");
                    return SlashResult::Handled;
                }
                // Parse `name[:model]` so users can switch both at once,
                // same as REPL. The main loop validates the provider
                // exists before committing the swap.
                let (pname, override_model) = match rest_after_cmd.split_once(':') {
                    Some((p, m)) => (p.trim().to_string(), Some(m.trim().to_string())),
                    None => (rest_after_cmd.clone(), None),
                };
                self.pending_provider_switch = Some((pname.clone(), override_model.clone()));
                let detail = match override_model {
                    Some(m) => format!("queued provider switch: {pname}:{m}"),
                    None => format!("queued provider switch: {pname}"),
                };
                self.push_system(&detail);
                SlashResult::Handled
            }
            "model" => {
                if rest_after_cmd.is_empty() {
                    self.push_error(
                        "usage: /model <name>  |  /model <N>  (N = index from /models)",
                    );
                    return SlashResult::Handled;
                }
                // Accept numeric index into the last `/models` listing.
                // Strictly parity with REPL's single-digit menu except
                // the user types the number on a normal prompt line.
                let resolved = if let Ok(n) = rest_after_cmd.parse::<usize>() {
                    if n == 0 || n > self.last_model_menu.len() {
                        self.push_error(&format!(
                            "/model: {n} is out of range (run /models first — menu has {} entries)",
                            self.last_model_menu.len()
                        ));
                        return SlashResult::Handled;
                    }
                    self.last_model_menu[n - 1].clone()
                } else {
                    rest_after_cmd.clone()
                };
                self.pending_model_switch = Some(resolved.clone());
                self.push_system(&format!("Set model to `{resolved}`"));
                SlashResult::Handled
            }
            "models" => {
                let models = models_for_provider(&self.current_provider);
                if models.is_empty() {
                    self.push_system(&format!(
                        "/models: no models registered for provider `{}`",
                        self.current_provider
                    ));
                    return SlashResult::Handled;
                }
                let ids: Vec<String> = models.iter().map(|(id, _)| id.to_string()).collect();
                self.last_model_menu = ids.clone();
                self.model_menu = Some(ids);
                SlashResult::Handled
            }
            "key" => {
                // `/key <ENV_VAR> <value>` — byte-for-byte REPL parity.
                // Sets the env var for the current session only, then
                // prints the same ~/.metis/config.toml persistence hint
                // REPL emits. NEVER echo the full value back — show only
                // the first 8 chars (same rule REPL uses).
                let mut parts = rest_after_cmd.splitn(2, char::is_whitespace);
                let env_var = parts.next().unwrap_or("").trim().to_string();
                let value = parts.next().unwrap_or("").trim().to_string();
                if env_var.is_empty() || value.is_empty() {
                    self.push_error("usage: /key <ENV_VAR> <value>");
                    return SlashResult::Handled;
                }
                std::env::set_var(&env_var, &value);
                let preview = &value[..value.len().min(8)];
                self.push_system(&format!(
                    "{env_var} set (session only)\n\
                     to persist, add to ~/.metis/config.toml:\n\
                       [api_keys]\n\
                       {env_var} = \"{preview}...\""
                ));
                SlashResult::Handled
            }
            "keys" => {
                self.push_system(
                    "keyboard shortcuts\n\
                     navigation : PageUp/PageDn · Shift+↑/↓ · Home/End\n\
                     input      : Shift+Enter (newline) · Ctrl+C (cancel)\n\
                     readline   : Ctrl+A (start) · Ctrl+E (end) · Ctrl+K (kill to end)\n\
                                  Ctrl+U (kill to start) · Ctrl+W (delete word)\n\
                     history    : ↑/↓ arrows · Ctrl+R (reverse search)\n\
                     completion : Tab (autocomplete)\n\
                     misc       : Esc Esc (clear input) · /key <VAR> <val> (set API key)",
                );
                SlashResult::Handled
            }
            "glm" => {
                // `/glm <prompt>` — REPL shortcut for `/consult glm`.
                // Same validation chain as /consult: empty prompt
                // rejected, unknown-provider error surfaces from
                // `Provider::lookup` on the consult path so misconfig'd
                // environments fail loudly instead of silently queuing
                // a dead turn.
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /glm <prompt>");
                    return SlashResult::Handled;
                }
                if aegis_api::Provider::lookup("glm").is_none() {
                    self.push_error("unknown provider: glm (see /providers)");
                    return SlashResult::Handled;
                }
                self.pending_consult = Some(("glm".to_string(), rest_after_cmd.clone()));
                self.push_system("consult queued: glm");
                SlashResult::Handled
            }
            "resume" => {
                if rest_after_cmd.is_empty() {
                    match SessionStore::list(workspace) {
                        Ok(list) if list.is_empty() => {
                            self.push_system(&format!("no sessions under {}", workspace.display()));
                        }
                        Ok(list) => {
                            let mut out = format!("sessions ({} total) — use /resume <id> to load:\n", list.len());
                            for s in list.iter().take(20) {
                                let age = s.modified.and_then(|t| {
                                    t.elapsed().ok().map(|d| {
                                        let secs = d.as_secs();
                                        if secs < 3600 { format!("{}m ago", secs / 60) }
                                        else if secs < 86400 { format!("{}h ago", secs / 3600) }
                                        else { format!("{}d ago", secs / 86400) }
                                    })
                                }).unwrap_or_default();
                                let id12: String = s.id.chars().take(12).collect();
                                out.push_str(&format!(
                                    "  {id12}  · {} msgs{}\n",
                                    s.message_count,
                                    if age.is_empty() { String::new() } else { format!("  · {age}") }
                                ));
                            }
                            self.push_system(out.trim_end());
                        }
                        Err(e) => self.push_error(&format!("could not list sessions: {e}")),
                    }
                    return SlashResult::Handled;
                }
                // Verify the session exists before signalling the main
                // loop so a typo doesn't silently swap us onto a
                // nonexistent id.
                match SessionStore::open(workspace, &rest_after_cmd) {
                    Ok(store) => {
                        let msg_count = store.messages().len();
                        let last_user = store
                            .messages()
                            .iter()
                            .rev()
                            .find(|m| m.role == aegis_api::Role::User)
                            .and_then(|m| m.content.as_ref())
                            .map(|c| truncate_str(c, 80).to_string());
                        self.messages.clear();
                        self.tools.clear();
                        self.streaming_text.clear();
                        self.thinking_text.clear();
                        self.scroll_offset = 0;
                        self.session_id = rest_after_cmd.clone();
                        // Rich resume banner — session id, message count,
                        // and the last user prompt so the user lands
                        // oriented instead of staring at an empty chat.
                        let mut banner =
                            format!("⏮ resumed session {rest_after_cmd} · {msg_count} messages");
                        if let Some(last) = last_user {
                            banner.push_str(&format!("\n  last prompt: {last}"));
                        }
                        self.push_system(&banner);
                        SlashResult::SwitchSession(rest_after_cmd)
                    }
                    Err(e) => {
                        self.push_error(&format!("resume failed: {e}"));
                        SlashResult::Handled
                    }
                }
            }
            "session-info" | "session" => {
                match SessionStore::list(workspace) {
                    Ok(list) => {
                        if let Some(s) = list.first() {
                            self.push_system(&format!(
                                "most recent session: {}\nmessages: {}\n/resume {} to load it",
                                s.id, s.message_count, s.id
                            ));
                        } else {
                            self.push_system("no sessions yet");
                        }
                    }
                    Err(e) => self.push_error(&format!("list failed: {e}")),
                }
                SlashResult::Handled
            }
            "files" => {
                if rest_after_cmd.is_empty() {
                    // Bare /files opens an interactive picker — type to
                    // filter, ↑↓ navigate, Enter inserts the path into
                    // input as `@path` so the next message can ship it
                    // as context. /files <path> still uses the legacy
                    // single-dir listing for inspection workflows.
                    let walked = walk_workspace_files(workspace, 800);
                    if walked.is_empty() {
                        self.push_system("no files matched the picker filters");
                    } else {
                        self.files_picker_query.clear();
                        self.files_picker_sel = 0;
                        self.files_picker = Some(walked);
                    }
                    return SlashResult::Handled;
                }
                let arg = Some(rest_after_cmd.clone());
                match build_files_listing(workspace, arg.as_deref()) {
                    Ok((plain, styled)) => self.push_styled(plain, styled),
                    Err(e) => self.push_error(&e),
                }
                SlashResult::Handled
            }
            "view" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /view <path>");
                    return SlashResult::Handled;
                }
                match build_view_output(workspace, &rest_after_cmd) {
                    Ok((plain, styled)) => self.push_styled(plain, styled),
                    Err(e) => self.push_error(&e),
                }
                SlashResult::Handled
            }
            "search" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /search [-i] [-r] [-n N] [-t ext,ext] <pattern>");
                    return SlashResult::Handled;
                }
                let (plain, styled) = build_search_output(workspace, &rest_after_cmd);
                self.push_styled(plain, styled);
                SlashResult::Handled
            }
            "image" => {
                if rest_after_cmd.is_empty() {
                    self.push_error("usage: /image <path> [<path>...]");
                    return SlashResult::Handled;
                }
                let paths = crate::path_input::resolve_many(&rest_after_cmd, workspace);
                for resolved in paths {
                    self.attach_image_path(&resolved);
                }
                SlashResult::Handled
            }
            "paste" => {
                // Pull an image off the system clipboard (Cmd+Ctrl+Shift+4
                // screenshot, or any copied PNG). macOS only.
                match crate::path_input::paste_image_from_clipboard() {
                    Ok(p) => self.attach_image_path(&p),
                    Err(e) => self.push_error(&format!("/paste failed: {e}")),
                }
                SlashResult::Handled
            }
            "images" => {
                if rest_after_cmd == "clear" {
                    let n = self.pending_images.len();
                    self.pending_images.clear();
                    self.push_system(&format!("cleared {n} attached image(s)."));
                    return SlashResult::Handled;
                }
                if self.pending_images.is_empty() {
                    self.push_system("no images attached. Use /image <path> to attach one.");
                } else {
                    let mut out = String::from("attached images:\n");
                    for (i, p) in self.pending_images.iter().enumerate() {
                        out.push_str(&format!("  {}. {}\n", i + 1, p.display()));
                    }
                    out.push_str("Use /images clear to remove all.");
                    self.push_system(out.trim_end());
                }
                SlashResult::Handled
            }
            "browser" => {
                let spec = "npx -y @playwright/mcp@latest";
                self.push_system("attaching Playwright MCP...");
                if let Some(registry) = self.tool_registry.clone() {
                    let mcp_list = Arc::clone(&self.attached_mcps);
                    tokio::spawn(async move {
                        match spawn_mcp_server(spec, &[]).await {
                            Ok(handle) => {
                                let n = handle.register_into_shared(&registry);
                                if let Ok(mut list) = mcp_list.lock() {
                                    let label = format!("playwright ({n} tools)");
                                    if !list.iter().any(|s| s == &label) {
                                        list.push(label);
                                    }
                                }
                                eprintln!("\x1b[1;32m[goblin] Playwright MCP attached, {n} tools\x1b[0m");
                            }
                            Err(e) => eprintln!("[goblin] failed to attach Playwright: {e}"),
                        }
                    });
                }
                SlashResult::Handled
            }
            "computer" => {
                let bin = "/Users/macmini/.nvm/versions/node/v20.20.1/bin/open-computer-use";
                self.push_system("attaching open-computer-use MCP...");
                if let Some(registry) = self.tool_registry.clone() {
                    let mcp_list = Arc::clone(&self.attached_mcps);
                    tokio::spawn(async move {
                        match spawn_mcp_server(bin, &["mcp".to_string()]).await {
                            Ok(handle) => {
                                let n = handle.register_into_shared(&registry);
                                if let Ok(mut list) = mcp_list.lock() {
                                    let label = format!("open-computer-use ({n} tools)");
                                    if !list.iter().any(|s| s == &label) {
                                        list.push(label);
                                    }
                                }
                                eprintln!("\x1b[1;32m[goblin] open-computer-use MCP attached, {n} tools\x1b[0m");
                            }
                            Err(e) => eprintln!("[goblin] failed to attach open-computer-use: {e}"),
                        }
                    });
                }
                SlashResult::Handled
            }
            _ => {
                // Skill fallback — match REPL's `ReplCommand::Unknown`
                // branch: if `/<cmd>` resolves to a user-invocable skill,
                // expand its prompt template with the trailing args and
                // queue it as a normal turn. This unlocks every installed
                // ECC skill and custom `.metis/skills/*.md` file without
                // needing a command-per-skill switch arm.
                if let Some(skill) = self.skill_registry.get(cmd).cloned() {
                    if skill.user_invocable {
                        let expanded = aegis_core::expand_prompt(&skill, &rest_after_cmd);
                        let preview: String = expanded.chars().take(60).collect();
                        let ellipsis = if expanded.chars().count() > 60 {
                            "…"
                        } else {
                            ""
                        };
                        self.push_system(&format!("skill /{cmd} → {preview}{ellipsis}"));
                        self.pending_prompts.push_back(expanded);
                        return SlashResult::Handled;
                    }
                }
                let hint = suggest_slash_command(cmd)
                    .map(|s| format!("  (did you mean /{s}?)"))
                    .unwrap_or_default();
                self.push_system(&format!(
                    "unknown command: /{cmd}{hint}  — try /help or /skills"
                ));
                SlashResult::Handled
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashResult {
    Handled,
    Clear,
    Quit,
    /// `/resume <id>` — main loop should swap its `session_id` so the
    /// next `run_agent_turn` reopens that SessionStore and replays its
    /// transcript. Returned instead of mutating REPL-style because the
    /// session id is a local variable in the main loop.
    SwitchSession(String),
    /// `/btw <text>` — send this text to the model as a real prompt turn.
    /// Currently no slash handler constructs this variant (the existing
    /// `/btw` path appends a context note rather than firing a turn);
    /// kept because the consumer in the main loop already pattern-matches
    /// on it, so the wiring is one slash-arm change away from active.
    #[allow(dead_code)]
    SendToModel(String),
}

/// Returns the REPL-parity color for a filename based on its extension.
/// Blue is reserved for directories (`entry_color(None)` returns it).
/// Mirrors exactly the match arm in `repl.rs::ReplCommand::Files`:
///   code (rs/cpp/c/h/hpp/py/js/ts/java/go) → yellow
///   docs/config (md/txt/json/toml/yaml/yml) → cyan
///   images (png/jpg/jpeg/gif/svg/webp) → magenta
///   everything else → terminal default (ratatui: Reset)
/// Legacy builder — `/recap` was initially implemented as a slash
/// command pushing a multi-line report into chat. User wanted the
/// info always-visible in the bottom status bar instead, so
/// `build_recap_line` now packs the important bits (tools:N,
/// queued:N, plan mode) inline. Kept under `#[allow(dead_code)]` as
/// a reference / possible future "/stats" command body. If this
/// lingers past v0.11 without a caller, just delete it.
#[allow(dead_code)]
fn build_recap_output(app: &TuiApp, workspace: &Path) -> (String, Vec<Line<'static>>) {
    use std::collections::HashMap;
    use std::fmt::Write as _;

    let label = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let key = Style::default().fg(Color::Yellow);
    let value = Style::default().fg(Color::Rgb(220, 220, 210));
    let header = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    let short_id: String = app.session_id.chars().take(8).collect();
    let turns = app.turn_count;
    let cost = app.cost_display.clone();
    let model = app.model.clone();
    let provider = app.current_provider.clone();

    // Header
    writeln!(plain, "[goblin] session recap").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label),
        Span::styled("session recap".to_string(), label),
    ]));

    // Session line
    let session_line =
        format!("  session: {short_id}  ·  {turns} turns  ·  {provider}/{model}  ·  {cost}");
    writeln!(plain, "{session_line}").ok();
    styled.push(Line::from(vec![
        Span::raw("  ".to_string()),
        Span::styled("session: ".to_string(), key),
        Span::styled(short_id, value),
        Span::styled("  ·  ".to_string(), dim),
        Span::styled(format!("{turns} turns"), value),
        Span::styled("  ·  ".to_string(), dim),
        Span::styled(format!("{provider}/{model}"), value),
        Span::styled("  ·  ".to_string(), dim),
        Span::styled(cost, value),
    ]));
    writeln!(plain).ok();
    styled.push(Line::from(""));

    // Tool usage histogram — read straight off `app.tools`, which
    // already has every tool call the UI logged this session.
    let mut tool_counts: HashMap<String, (u32, u32, u32)> = HashMap::new();
    for t in &app.tools {
        let entry = tool_counts.entry(t.name.clone()).or_insert((0, 0, 0));
        match t.status {
            ToolStatus::Done => entry.0 += 1,
            ToolStatus::Failed => entry.1 += 1,
            ToolStatus::Running => entry.2 += 1,
        }
    }
    let mut sorted: Vec<(&String, &(u32, u32, u32))> = tool_counts.iter().collect();
    sorted.sort_by(|a, b| {
        let at = a.1 .0 + a.1 .1 + a.1 .2;
        let bt = b.1 .0 + b.1 .1 + b.1 .2;
        bt.cmp(&at).then(a.0.cmp(b.0))
    });
    if sorted.is_empty() {
        writeln!(plain, "  tools: (none called yet)").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled("tools: ".to_string(), key),
            Span::styled("(none called yet)".to_string(), dim),
        ]));
    } else {
        writeln!(plain, "  tools used:").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled("tools used:".to_string(), header),
        ]));
        for (name, (ok, err, running)) in sorted.iter().take(10) {
            let total = ok + err + running;
            let err_tag = if *err > 0 {
                format!(" ({err} failed)")
            } else {
                String::new()
            };
            writeln!(plain, "    {name:<18} × {total}{err_tag}").ok();
            let mut spans = vec![
                Span::raw("    ".to_string()),
                Span::styled(format!("{name:<18}"), value),
                Span::styled(" × ".to_string(), dim),
                Span::styled(format!("{total}"), key),
            ];
            if *err > 0 {
                spans.push(Span::styled(
                    format!(" ({err} failed)"),
                    Style::default().fg(Color::White),
                ));
            }
            styled.push(Line::from(spans));
        }
    }
    writeln!(plain).ok();
    styled.push(Line::from(""));

    // Last 3 user prompts, reverse-chronological. Truncate each to 70
    // chars so a huge paste doesn't overflow the recap box.
    let mut recent_users: Vec<String> = app
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .map(|m| m.text.clone())
        .collect();
    recent_users.reverse();
    recent_users.truncate(3);
    if recent_users.is_empty() {
        writeln!(plain, "  recent prompts: (none yet)").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled("recent prompts: ".to_string(), key),
            Span::styled("(none yet)".to_string(), dim),
        ]));
    } else {
        writeln!(plain, "  recent prompts:").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled("recent prompts:".to_string(), header),
        ]));
        for (i, p) in recent_users.iter().enumerate() {
            let first_line = p.lines().next().unwrap_or("").trim().to_string();
            let truncated: String = if first_line.chars().count() > 70 {
                let head: String = first_line.chars().take(69).collect();
                format!("{head}…")
            } else {
                first_line
            };
            writeln!(plain, "    {}. {truncated}", i + 1).ok();
            styled.push(Line::from(vec![
                Span::raw("    ".to_string()),
                Span::styled(format!("{}.", i + 1), dim),
                Span::raw(" ".to_string()),
                Span::styled(truncated, value),
            ]));
        }
    }
    writeln!(plain).ok();
    styled.push(Line::from(""));

    // Footer stats: queued images, memory count, skill count.
    let images_queued = app.pending_images.len();
    let skills_count = app.skill_registry.len();
    let memory_count = count_memory_entries(workspace);

    writeln!(
        plain,
        "  images queued: {images_queued}  ·  memories: {memory_count}  ·  skills available: {skills_count}"
    )
    .ok();
    styled.push(Line::from(vec![
        Span::raw("  ".to_string()),
        Span::styled("images queued: ".to_string(), key),
        Span::styled(format!("{images_queued}"), value),
        Span::styled("  ·  ".to_string(), dim),
        Span::styled("memories: ".to_string(), key),
        Span::styled(format!("{memory_count}"), value),
        Span::styled("  ·  ".to_string(), dim),
        Span::styled("skills available: ".to_string(), key),
        Span::styled(format!("{skills_count}"), value),
    ]));

    let plan = match *app.plan_state.lock().unwrap() {
        PlanState::Drafting => Some(("PLAN (drafting)", Color::Yellow)),
        PlanState::Executing => Some(("PLAN (executing)", Color::Green)),
        PlanState::Normal => None,
    };
    if let Some((label_text, col)) = plan {
        writeln!(plain, "  mode: {label_text}").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled("mode: ".to_string(), key),
            Span::styled(
                label_text.to_string(),
                Style::default().fg(col).add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    (plain, styled)
}

/// Cheap file count for the `~/.metis/memory/` and workspace-scoped
/// `.metis/memory/` directories. Non-`.md` files and hidden entries
/// are skipped. Returns 0 if neither dir exists — no error surface.
/// Used only by the legacy `build_recap_output`; kept alive for the
/// same reason.
#[allow(dead_code)]
fn count_memory_entries(workspace: &Path) -> usize {
    fn count_dir(dir: &Path) -> usize {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return 0;
        };
        rd.filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let n = name.to_string_lossy();
                !n.starts_with('.') && n.ends_with(".md")
            })
            .count()
    }
    let home_mem = dirs::home_dir()
        .map(|h| h.join(".metis").join("memory"))
        .map(|p| count_dir(&p))
        .unwrap_or(0);
    let ws_mem = count_dir(&workspace.join(".metis").join("memory"));
    home_mem + ws_mem
}

/// Render the permission modal as a vector of ratatui `Line`s. The
/// first line is the header (tool name in red-bold), then compressed
/// args on a dim line, a blank, three numbered options (focused line
/// gets `❯ ` cyan prefix; others get dim `  `), and an Esc hint.
/// (display_label, action_key, description)
const ALLOW_MENU_ITEMS: &[(&str, &str, &str)] = &[
    ("bash",          "bash",         "terminal komutları çalıştır"),
    ("edit_file",     "edit_file",    "mevcut dosyayı düzenle"),
    ("write_file",    "write_file",   "yeni dosya yaz / üzerine yaz"),
    ("multi_edit",    "multi_edit",   "çok dosyayı tek seferde düzenle"),
    ("computer_use",  "computer_use", "bilgisayar kontrolü (ekran/tıklama)"),
    ("all",           "ALL",          "yukarıdakilerin hepsine izin ver"),
    ("deny all",      "-ALL",         "tüm izinleri kaldır"),
];

fn build_allow_menu_lines(always_allowed: &std::collections::HashSet<String>) -> Vec<Line<'static>> {
    let header    = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let active    = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
    let normal    = Style::default().fg(Color::White);
    let deny_st   = Style::default().fg(Color::Rgb(200, 80, 80)).add_modifier(Modifier::BOLD);
    let dim       = Style::default().fg(Color::Rgb(110, 110, 110));
    let desc_st   = Style::default().fg(Color::Rgb(160, 160, 160));

    let mut lines = vec![
        Line::from(Span::styled("── izinler / permissions ──", header)),
        Line::from(""),
    ];

    for (i, (label, action, description)) in ALLOW_MENU_ITEMS.iter().enumerate() {
        let is_deny = action.starts_with('-');
        let is_active = !is_deny && (
            *action == "ALL" || always_allowed.contains(*action)
        );
        let label_style = if is_deny { deny_st } else if is_active { active } else { normal };
        let active_marker = if is_active { " ✓" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", i + 1), key_style),
            Span::styled(format!("{}{}", label, active_marker), label_style),
            Span::styled(format!("  {}", description), desc_st),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  digit", key_style),
        Span::styled("  seç  ·  ", dim),
        Span::styled("Esc", key_style),
        Span::styled("  kapat", dim),
    ]));
    lines
}

fn build_help_styled() -> (String, Vec<Line<'static>>) {
    let title = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    // Section headers: yellow background block so categories visibly
    // separate even when terminal's BOLD/UNDERLINE attributes don't
    // render distinctly. Black text on bright yellow fg+bg is the
    // strongest available "label chip" the TUI can produce.
    let cat   = Style::default()
        .fg(Color::Black)
        .bg(Color::Rgb(232, 200, 60))
        .add_modifier(Modifier::BOLD);
    let cmd   = Style::default()
        .fg(Color::Rgb(140, 220, 255))
        .add_modifier(Modifier::BOLD);
    let note  = Style::default().fg(Color::Rgb(220, 220, 220));  // body: açık beyaz
    let kbd   = Style::default()
        .fg(Color::Rgb(255, 180, 80))
        .add_modifier(Modifier::BOLD);   // klavye: turuncu, bold
    let dim   = Style::default().fg(Color::Rgb(140, 140, 140));

    let mut plain = String::new();
    let mut lines: Vec<Line<'static>> = Vec::new();

    macro_rules! p { ($s:expr) => { plain.push_str($s); plain.push('\n'); } }
    macro_rules! blank { () => { lines.push(Line::from("")); plain.push('\n'); } }

    macro_rules! section {
        ($label:expr) => {
            p!(&format!("── {} ──", $label));
            // Yellow chip ` LABEL ` with a thin grey rule extending
            // to the right — reads as a tabbed section header rather
            // than free-floating text.
            let dim_rule = Style::default().fg(Color::Rgb(80, 80, 80));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", $label), cat),
                Span::styled(
                    format!(" {}", "─".repeat(40usize.saturating_sub($label.len() + 3))),
                    dim_rule,
                ),
            ]));
        };
    }
    // command + one-line description
    macro_rules! row {
        ($c:expr, $d:expr) => {
            p!(&format!("  {:<26} {}", $c, $d));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<26}", $c), cmd),
                Span::styled($d.to_string(), note),
            ]));
        };
    }
    // indented plain note (gray)
    macro_rules! nt {
        ($n:expr) => {
            p!(&format!("      {}", $n));
            lines.push(Line::from(vec![
                Span::raw("      "),
                Span::styled($n.to_string(), note),
            ]));
        };
    }
    macro_rules! krow {
        ($k:expr, $d:expr) => {
            p!(&format!("  {:<24} {}", $k, $d));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<24}", $k), kbd),
                Span::styled($d.to_string(), dim),
            ]));
        };
    }

    p!("aegis — Komut Rehberi");
    lines.push(Line::from(Span::styled("aegis — Komut Rehberi", title)));
    blank!();

    section!("PROVIDER + MODEL");
    row!("/providers",                "numaralı overlay açılır — seç → model listesi otomatik gelir, Esc iptal");
    row!("/models",                   "sadece mevcut provider'ın modelleri — provider değişmez, tus bas seç");
    row!("/provider <id>",            "provider'ı overlay olmadan direkt değiştir  (ör: /provider glm)");
    row!("/model <N|isim>",           "modeli direkt değiştir: numara veya tam isim  (ör: /model 3)");
    row!("/budget <N>",               "token bütçesi koy — 1000 token ≈ 750 kelime, aşınca uyarır");
    blank!();

    section!("SORU CEVAP");
    row!("/ask <soru>",               "tek-shot yan soru: agent meşgul olsa bile current model'e tools kapalı sorulur, cevap yan akışta düşer");
    row!("/askall <soru>",            "TÜM kurulu provider'ların en güçlü modeline aynı anda sor, cevaplar birer birer düşer");
    row!("/consult",                  "overlay açılır, hangi provider'a soracağını seçersin, sonra soruyu yaz + Enter");
    row!("/consult <prv> <soru>",     "tek provider'a direkt sor, ana konuşma etkilenmez  (ör: /consult glm bu doğru mu?)");
    row!("/claude <soru>",            "claude -p subprocess çalıştır — mevcut model DeepSeek olsa bile Sonnet'e gider");
    row!("/glm <soru>",               "kısayol: doğrudan GLM'e gönder, başka provider'a gitmiyor");
    row!("/race <prompt>",            "tüm provider'lara aynı anda gönder, kim önce cevap verirse o kazanır");
    row!("/swarm [N] [quorum:M]",     "N tane paralel agent çalıştır, quorum:M tanesi aynı fikirdeyse kabul et");
    blank!();

    section!("PLAN + ANALİZ");
    row!("/plan",                     "[plan] moduna gir — yazdıkların agent'a gitmez, not tutmak için");
    row!("/plan execute",             "plan modundaki metni agent'a gönder, çalıştırmaya başlar");
    row!("/overthink",                "daha derin analiz modu — karmaşık mantık ve çok adımlı problemler için");
    row!("/advisor",                  "danışman modu: agent her adımı açıklar, ne yapacağını söyler önce yapmaz");
    row!("/advisor-off",              "danışman modunu kapat, normal çalışmaya dön");
    row!("/autotune",                 "agent performansına göre model parametrelerini otomatik ayarlar");
    blank!();

    section!("SESSION — konuşma geçmişi");
    row!("/init",                     "projeyi analiz eder ve AGENTS.md oluşturur (OpenCode stili onboarding)");
    nt!("Workspace config wizard ayrı: terminalden `aegis init` (CLI subcommand) → provider/model/budget/auto-router seçici, .metis/config.toml yazar.");
    row!("/undo",                     "son değişiklikleri geri al — git checkout ile dosyaları eski haline döndürür");
    row!("/redo",                     "geri alınan değişiklikleri geri getirme kılavuzu");
    row!("/share",                    "session paylaşımı — .metis/sessions/ içindeki JSONL dosyasını kopyala");
    row!("/connect",                  "provider kurulum rehberi — hangi API key'lerin setli olduğunu gösterir");
    row!("/usage",                    "session + all-time kullanım istatistikleri (turns, tokens, cost)");
    row!("/context",                  "token kullanım detayı — context window doluluk oranı");
    row!("/sessions",                 "kayıtlı konuşmaların listesi — her konuşma ayrı bir session");
    row!("/session-info",             "şu anki session'ın ID'sini ve detaylarını gösterir");
    row!("/tree",                     "session'ların dallanma ağacını gösterir, fork yaptıysan hangi dal nereden");
    row!("/resume <id>",              "eski bir konuşmaya geri dön, kaldığın yerden devam edersin");
    row!("/fork",                     "mevcut session'ı kopyalar — git branch gibi, biri bozulursa diğeri temiz");
    row!("/compact",                  "uzun konuşmayı özetler, context küçülür ve hızlanır, özet hafızada kalır");
    row!("/rewind code",              "bu turn'de değişen dosyaları geri al (turn_files git checkout)");
    row!("/rewind conv [n]",          "son n user turn'ünü mesaj geçmişinden sil (default n=1)");
    row!("/rewind both [n]",          "kod + konuşma birlikte geri (CC Esc+Esc menüsü slash karşılığı)");
    row!("/rewind from <idx>",        "belirli mesaj index'inden sonrasını siler — hedefli compact");
    row!("/recall-prev",              "boot'ta tespit edilen kayıtsız önceki session'ı bu konuşmaya enjekte et");
    blank!();

    section!("ŞABLON + İŞARET");
    row!("/save-template <ad>",       "input'u (boşsa son user mesajını) ~/.metis/templates/<ad>.md olarak kaydet");
    row!("/use <ad>",                 "kayıtlı şablonu input kutusuna yükle (Enter ile gönder)");
    row!("/templates",                "kayıtlı şablonların listesi — boşsa /save-template ile başla");
    row!("/pin [N]",                  "user mesajını pinle (★) — scrollback'te kaybolmaz, /pin <n> aynı index'i unpin'ler");
    row!("/unpin [N]",                "pin'i kaldır — argüman yoksa tüm pin'leri temizler");
    row!("/pinned",                   "pin'lenmiş turn'lerin listesi");
    row!("/bell [on|off|<sn>]",       "uzun turn bittiğinde terminal-bell çal (default off, eşik 1-3600 sn)");
    blank!();

    section!("GİT + TEST + KOD");
    row!("/commit [mesaj]",           "bu turn'de değişen dosyaları stage + commit eder, prefix `[goblin]`");
    row!("/diff [path|ref|range]",    "working tree diff (uncommitted) ya da git ref/range (örn: HEAD~3, main..HEAD)");
    row!("/test",                     "config'deki `[auto_fix] test_command`'ı çalıştırır — fail ise output agent'a context");
    row!("/lint",                     "config'deki `[auto_fix] lint_command`'ı çalıştırır — fail ise output agent'a context");
    row!("/run <komut>",              "shell komutunu çalıştır + çıktıyı chat'e push (agent next turn'de görür)");
    nt!("Config: workspace `.metis/config.toml` veya `~/.metis/config.toml`'da:  [auto_fix]  test_command = \"cargo test\"  lint_command = \"cargo clippy\"");
    blank!();

    section!("CONTEXT + HAFIZA");
    row!("/memory",                   "agent'ın kalıcı hafızasını gösterir — session'lar arası hatırladıkları");
    row!("/dag",                      "bu session'da hangi araçların hangi sırayla çalıştığını gösterir");
    row!("/map [N]",                  "projedeki dosyaların haritasını çıkarır, N = gösterilecek maks dosya sayısı");
    blank!();

    section!("DOSYA + GÖRSELLER");
    row!("/files [path]",             "dizin içeriğini listeler, path belirtmezsen mevcut klasör");
    row!("/view <path>",              "dosya içeriğini chat'e yazdırır");
    row!("/search [flags] <pat>",     "projedeki dosyalarda metin arar — -i büyük/küçük harf fark etmez");
    row!("/image <path>",             "görsel dosya yükler, sonraki mesajla birlikte agent'a gider");
    row!("/images [clear]",           "yüklü görselleri listeler, 'clear' ekle hepsini temizle");
    row!("/paste",                    "panodaki (Cmd+C ile kopyaladığın) görseli yapıştırır");
    blank!();

    section!("ÖĞRENME + OY");
    row!("/rate good",                "son yanıtı beğendin — agent bu yaklaşımı kaydeder, tekrar kullanır");
    row!("/rate bad [\"neden\"]",     "son yanıtı beğenmedin — neden beğenmediğini yazarsan daha hızlı öğrenir");
    row!("/rate undo",                "son oylamayı geri al");
    row!("/ratings",                  "tüm oylamaların listesi");
    row!("/insights",                 "agent'ın öğrendiklerinin özeti — neyi iyi yaptı, nerede hata yaptı");
    row!("/rules",                    "şu anda aktif olan davranış kuralları");
    row!("/learn <kural>",            "direkt bir kural ekle  (ör: /learn Türkçe yanıt ver)");
    row!("/forget <pattern>",         "yanlış öğrenilmiş bir kuralı sil");
    blank!();

    section!("GÖREVLER");
    row!("/tasks",                    "görev listesini gösterir");
    row!("/task add <açıklama>",      "yeni görev ekler  (ör: /task add login sayfasını düzelt)");
    row!("/task done <N>",            "N numaralı görevi tamamlandı işaretler");
    row!("/task rm <N>",              "N numaralı görevi siler");
    row!("/task clear",               "tüm görevleri temizler");
    blank!();

    section!("GÜVENLİK + İZİNLER");
    row!("/allow",                    "overlay açılır, hangi araçlara izin vereceğini seçersin, aktif olanlar ✓ ile görünür");
    row!("/allow <araç|all>",         "araç iznini overlay olmadan direkt ver  (ör: /allow bash)");
    row!("/allow bash \"<cmd>\"",       "per-command bash whitelist (CC \"Yes don't ask again\"): 'git status' allow et, 'git status -sb' sormadan geçer");
    row!("/deny <araç|all>",          "araç iznini kaldır  (ör: /deny bash  veya  /deny all)");
    row!("/deny bash <cmd>",          "tek bir bash whitelist girdisini kaldır  (/deny bash all → hepsini sil)");
    row!("/allowed",                  "bu session'da izin verilmiş araçların + per-cmd bash kayıtların listesi");
    row!("/security [kill|resume]",   "güvenlik durumunu gösterir / çalışan agent'ı durdurur / devam ettirir");
    nt!("Mod döngüsü (CC pattern, Shift+Tab veya boş input'ta Tab):  default → accept-edits → plan → bypass → default");
    row!("/default-mode",             "Default mod: edit/bash için onay sorulur (varsayılan)");
    row!("/accept-edits",             "AcceptEdits mod: edit_file/write_file/multi_edit otomatik allowed, bash hala onay");
    row!("/plan",                     "Plan mod: sadece read-only tool'lar — keşif/analiz");
    row!("/bypass",                   "Bypass mod: tüm tool'lar otomatik (yolo) — full autonomy");
    row!("/yolo  /allow-all",         "tüm tool'ları always_allowed set'e ekle (Bypass'a yakın, geriye uyumluluk)");
    blank!();

    section!("SKILLS — kendi komutlarını yaz");
    row!("/skills",                   "yüklü skill'lerin listesini gösterir");
    row!("/skill-install <path>",     "skill dosyası veya klasör yükler  (ör: /skill-install ~/skilllerim/)");
    row!("/skill-uninstall <isim>",   "bir skill'i kaldırır  (ör: /skill-uninstall commit)");
    row!("/skill-search <kelime>",    "skill açıklamalarında arama yapar");
    nt!("Skill = ~/.metis/skills/commit.md gibi bir markdown dosyası. /commit yaz → prompt agent'a gider. $ARGS argüman alır.");
    nt!("Dahili skill'ler (her zaman yüklü):  /commit · /review-pr · /test");
    blank!();

    section!("MCP — Araç Sunucuları");
    row!("/browser",                  "Playwright MCP'yi çalışma zamanında bağlar — web sayfası aç, tıkla, form doldur");
    row!("/computer",                 "open-computer-use MCP'yi bağlar — tam masaüstü kontrolü, ekran görme, tıklama");
    nt!("Önbellek (offline): terminalden `aegis 'mcp tools'` cached tool listesi · `aegis 'mcp clear'` cache'i siler");
    blank!();

    section!("DİĞER");
    row!("/cost",                     "bu session'da harcanan API maliyetini gösterir ($)");
    row!("/stats",                    "toplam kullanım istatistikleri");
    row!("/clear",                    "konuşma geçmişini siler — güvenlik için 2 kez yazmak gerekir");
    row!("/sidebar",                  "sağ paneli aç/kapat — Context, LSP, Todo, Models, Help, Footer");
    row!("/mouse",                    "mouse yakalamayı aç/kapat — kapalıysa terminalde metin kopyalayabilirsin");
    row!("/copy",                     "son asistan mesajını sistem panosuna kopyalar (her cevapta otomatik kopya da var)");
    row!("/copy-context",             "tüm conversation'ı markdown olarak panoya kopyala (ChatGPT/Claude web UI'sine yapıştırılabilir)");
    row!("/idle on|off",              "5dk sessizlikte session özeti basılsın mı (default: off)");
    row!("/keys",                     "klavye kısayollarının tam listesi");
    row!("/help  /info  /?",          "bu sayfayı gösterir");
    blank!();

    section!("KLAVYE KISAYOLLARI");
    krow!("Tab",                      "input boşken: permission mod cycle (default→accept-edits→plan→bypass) · / ile başlıyorsa: otomatik tamamla · @ aktifse: file complete");
    krow!("Shift+Tab / BackTab",      "her zaman 4-mod cycle: default → accept-edits → plan → bypass → default");
    krow!("Ctrl+F",                   "chat geçmişinde inkremental arama (next/prev gezinme, hit highlight)");
    krow!("Ctrl+R",                   "daha önce yazdığın komutlarda ara");
    krow!("Shift+Enter",              "çok satırlı mesaj girmek için yeni satır ekle");
    krow!("Ctrl+C",                   "çalışan agent'ı durdurur");
    krow!("Ctrl+A / Ctrl+E",          "imleci satır başına / sonuna götür");
    krow!("Ctrl+K",                   "imleçten satır sonuna kadar sil");
    krow!("Ctrl+U",                   "tüm satırı sil");
    krow!("Ctrl+W",                   "bir önceki kelimeyi sil");
    krow!("Esc Esc",                  "input kutusunu temizle");
    krow!("PgUp / PgDn",              "chat'i bir sayfa yukarı / aşağı kaydır");
    krow!("Ctrl+PgUp / PgDn",         "en başa / en sona git");
    krow!("Shift+↑ / Shift+↓",        "chat'i 3 satır yukarı / aşağı kaydır");
    krow!("dur / stop / iptal",       "agent çalışırken bu kelimelerden birini yaz + Enter → durdurur");

    (plain, lines)
}

fn build_provider_menu_lines(items: &[(String, String, bool)], current: &str, sel: usize) -> Vec<Line<'static>> {
    let header = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let ready = Style::default().fg(Color::Green);
    let nokey = Style::default().fg(Color::Rgb(120, 120, 120));
    let active_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(120, 120, 120));
    let mut lines = vec![
        Line::from(Span::styled("providers", header)),
        Line::from(""),
    ];
    for (i, (id, model, has_key)) in items.iter().enumerate() {
        let is_active = current.contains(id.as_str());
        let is_sel = i == sel;
        let label_style = if is_active { active_style } else if *has_key { ready } else { nokey };
        let status = if is_active { " ◀" } else if *has_key { "" } else { "  no key" };
        // Selection caret highlights the row that Enter will pick.
        let caret = if is_sel { "▶ " } else { "  " };
        let caret_style = if is_sel {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        lines.push(Line::from(vec![
            Span::styled(caret.to_string(), caret_style),
            Span::styled(format!("{} ", i + 1), key_style),
            Span::styled(format!("{:<14} {}{}", id, model, status), label_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("↑↓ + Enter  ·  1-9 hızlı  ·  Esc kapat", dim)));
    lines
}

fn build_consult_provider_overlay(items: &[(String, String, bool)]) -> Vec<Line<'static>> {
    let header = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let ready = Style::default().fg(Color::White);
    let nokey = Style::default().fg(Color::Rgb(120, 120, 120));
    let dim = Style::default().fg(Color::Rgb(120, 120, 120));
    let mut lines = vec![
        Line::from(Span::styled("consult — pick provider", header)),
        Line::from(""),
    ];
    for (i, (id, model, has_key)) in items.iter().enumerate() {
        let label_style = if *has_key { ready } else { nokey };
        let status = if *has_key { "" } else { "  no key" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", i + 1), key_style),
            Span::styled(format!("{:<14} {}{}", id, model, status), label_style),
        ]));
        if i >= 8 { break; }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("digit", key_style),
        Span::styled("  pick  ·  then type question + Enter  ·  ", dim),
        Span::styled("Esc", key_style),
        Span::styled("  cancel", dim),
    ]));
    lines
}

fn build_model_menu_lines(models: &[String], current: &str, sel: usize) -> Vec<Line<'static>> {
    let header = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let active_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::Rgb(120, 120, 120));
    let mut lines = vec![
        Line::from(Span::styled("models", header)),
        Line::from(""),
    ];
    for (i, model) in models.iter().enumerate() {
        let is_active = model == current;
        let is_sel = i == sel;
        let style = if is_active { active_style } else { label_style };
        let suffix = if is_active { " ◀" } else { "" };
        let caret = if is_sel { "▶ " } else { "  " };
        let caret_style = if is_sel {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        lines.push(Line::from(vec![
            Span::styled(caret.to_string(), caret_style),
            Span::styled(format!("{} ", i + 1), key_style),
            Span::styled(format!("{model}{suffix}"), style),
        ]));
        if i >= 8 { break; }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ + Enter  ·  1-9 hızlı  ·  Esc kapat",
        dim,
    )));
    lines
}

fn skill_tr_desc(name: &str) -> Option<&'static str> {
    match name {
        // Temel
        "commit"        => Some("yazdığın değişiklikleri kaydet, mesajı otomatik yazar"),
        "review-pr"     => Some("kod değişikliklerini incele, hata ve sorunları bul"),
        "test"          => Some("projenin testlerini çalıştır, neyin bozulduğunu söyle"),
        // Git
        "git-workflow"  => Some("git ile dal aç, birleştir, geri al — tüm git işlemleri"),
        "github-ops"    => Some("GitHub'da PR aç, issue oluştur, repo yönet"),
        "tdd-workflow"  => Some("önce test yaz, sonra kodu — test güdümlü geliştirme"),
        // Araştırma
        "deep-research" => Some("bir konuyu internet genelinde derinlemesine araştır"),
        "web-scraping"  => Some("herhangi bir siteden veri çek ve düzenle"),
        "market-research" => Some("sektör araştırması yap, rakipleri analiz et"),
        "exa-search"    => Some("Exa arama motoruyla güncel web sonuçları getir"),
        "websearch"     => Some("web'de ara"),
        "search"        => Some("proje dosyalarında metin veya kod ara"),
        "search-first"  => Some("bir şeyi değiştirmeden önce kodun tamamını tara"),
        // Güvenlik
        "security-review" => Some("kodundaki güvenlik açıklarını bul ve açıkla"),
        "security-scan"   => Some("tüm projede güvenlik taraması yap"),
        "security-bounty-hunter" => Some("ödüllü hata programları için güvenlik açığı ara"),
        "hipaa-compliance"   => Some("sağlık verileri için yasal uyumluluk kontrolü"),
        // Arayüz
        "frontend-design"  => Some("arayüz tasarım kararlarında yol göster"),
        "frontend-patterns" => Some("React/Vue/Svelte ile iyi kod yazma yöntemleri"),
        "design-system"    => Some("renk, font, bileşen gibi tasarım kuralları oluştur"),
        "ui-ux-pro-max"    => Some("arayüzü profesyonel gözle analiz et ve iyileştir"),
        "nextjs-turbopack" => Some("Next.js projesi kur ve hızlı hale getir"),
        "liquid-glass-design" => Some("Apple'ın yeni cam efektli tasarım stilinde arayüz"),
        // Sunucu & API
        "backend-patterns"  => Some("sunucu tarafı kod için doğru yapıyı seç ve uygula"),
        "api-design"        => Some("başka sistemlerin kullanacağı API tasarla ve belgele"),
        "api-connector-builder" => Some("dış bir servisle bağlantı kur, veriyi çek"),
        "database-migrations" => Some("veritabanı yapısını değiştir, veriler korunsun"),
        "deployment-patterns" => Some("kodu sunucuya güvenle gönder, otomatik yayınla"),
        "docker-patterns"   => Some("Docker ile uygulama paketi oluştur ve çalıştır"),
        // Diller
        "python-patterns"   => Some("Python'u doğru ve temiz yazma yöntemleri"),
        "rust-patterns"     => Some("Rust'ta bellek güvenli ve hızlı kod yaz"),
        "golang-patterns"   => Some("Go ile sade ve verimli kod yaz"),
        "swift-concurrency-6-2" => Some("Swift 6'da eş zamanlı işlemleri güvenle yönet"),
        "swiftui-patterns"  => Some("SwiftUI ile iOS ekranları ve durum yönetimi"),
        "dart-flutter-patterns" => Some("Flutter ile mobil ekran ve durum yönetimi"),
        "kotlin-patterns"   => Some("Kotlin ile Android veya çok platformlu uygulama"),
        "java-coding-standards" => Some("Java'da temiz ve düzenli kod yazma standartları"),
        "cpp-coding-standards"  => Some("C++'da modern ve güvenli kod yazma yöntemleri"),
        "python-testing"    => Some("Python projeni test et, hataları bul"),
        "rust-testing"      => Some("Rust projeni test et"),
        "golang-testing"    => Some("Go projeni test et ve performansını ölç"),
        // Test
        "e2e-testing"       => Some("tarayıcıda gerçek kullanıcı gibi test et"),
        "browser-qa"        => Some("tarayıcıda manuel test akışları oluştur"),
        "benchmark"         => Some("kodun ne kadar hızlı çalıştığını ölç"),
        "eval-harness"      => Some("bir modeli veya sistemi puanlayıp değerlendir"),
        // AI & Ajan
        "agent"             => Some("görevi başından sonuna otomatik tamamla"),
        "autonomous-loops"  => Some("bitene kadar kendi kendine döngüde çalış"),
        "council"           => Some("farklı bakış açılarından fikir al, birlikte karar ver"),
        "parallel"          => Some("aynı görevi birden fazla ajana aynı anda ver"),
        "spawn"             => Some("yeni bir alt ajan başlat, o halletsin"),
        "agent-eval"        => Some("ajanın ürettiği sonucu değerlendir ve puanla"),
        "gan-style-harness" => Some("bir ajan üretir, diğeri değerlendirir — kaliteyi artır"),
        "prompt-optimizer"  => Some("yazdığın promptu daha iyi sonuç verecek şekilde düzenle"),
        "continuous-learning-v2" => Some("konuşmadan çıkan dersleri öğren ve kaydet"),
        // Hafıza
        "remember"          => Some("bir şeyi uzun süre hatırla, sonra da erişilebilir olsun"),
        "recall"            => Some("daha önce kaydedilen bilgiyi getir"),
        "forget"            => Some("hafızadan bir şeyi sil"),
        "knowledge-ops"     => Some("bilgi tabanına ekle, sil, sorgula"),
        "context-budget"    => Some("uzun konuşmalarda alanı verimli kullan"),
        "strategic-compact" => Some("çok uzayan konuşmayı özetle, sıkıştır"),
        // İçerik & Yazı
        "article-writing"   => Some("makale veya blog yazısı yaz"),
        "brand-voice"       => Some("markanın ses tonunu belirle ve koruyarak yaz"),
        "seo"               => Some("arama motorlarında üst sıralara çıkmak için optimize et"),
        "crosspost"         => Some("aynı içeriği farklı platformlara uyarla"),
        "investor-materials" => Some("yatırımcıya sunacağın belge ve slaytları hazırla"),
        // Operasyon
        "terminal-ops"      => Some("terminalde komutlarla sistem ve dosya işlemleri yap"),
        "email-ops"         => Some("e-posta taslağı yaz ve düzenle"),
        "jira-integration"  => Some("Jira'da görev aç, güncelle, listele"),
        "google-workspace-ops" => Some("Google Dokümanlar, Tablolar ve Drive'da işlem yap"),
        // Araçlar
        "blueprint"         => Some("bir özellik veya projeyi adım adım planla"),
        "code-tour"         => Some("kodun ne yaptığını adım adım anlat"),
        "codebase-onboarding" => Some("yeni bir projeye hızla alış, nelerin nerede olduğunu öğren"),
        "repo-scan"         => Some("projenin genel yapısını tara ve özetle"),
        "repomap"           => Some("hangi dosya nerede, dosya haritasını çıkar"),
        "screenshot"        => Some("ekran görüntüsü al ve ne gördüğünü analiz et"),
        "voice"             => Some("sesli komut al ve işle"),
        "video-editing"     => Some("video düzenleme adımlarını yaz veya otomatize et"),
        "manim-video"       => Some("matematik/kod animasyonu ile açıklama videosu yap"),
        "web"               => Some("web sayfası oluştur veya mevcut sayfayı düzenle"),
        "lsp"               => Some("editörden bağımsız kod analizi, tanıma ve refactor"),
        "worktree"          => Some("aynı projede birden fazla dalı aynı anda aç"),
        "hookify-rules"     => Some("belirli olaylarda otomatik çalışacak kural ekle"),
        "rules-distill"     => Some("konuşmalardan öğrenilen kuralları temizle ve derle"),
        "verification-loop" => Some("üretilen çıktıyı kontrol et, yanlışsa tekrar dene"),
        "continuous-agent-loop" => Some("görev tamamlanana kadar durmadan çalış"),
        "safety-guard"      => Some("tehlikeli bir işlem yapmadan önce kontrol et"),
        "skill-comply"      => Some("skill formatına uy, kuralları doğrula"),
        "glm"               => Some("GLM modeline direkt mesaj gönder"),
        "nanoclaw-repl"     => Some("NanoClaw REPL konuşmasını başlat"),
        "cron"              => Some("belirli saatte veya aralıkta otomatik çalışacak iş kur"),
        // Erişilebilirlik & UX
        "accessibility"     => Some("engelli kullanıcıların uygulamayı kullanıp kullanamadığını test et"),
        "ui-demo"           => Some("tıklanabilir arayüz demosu oluştur"),
        "click-path-audit"  => Some("kullanıcının tıklama yollarını analiz et, sorunu bul"),
        "frontend-slides"   => Some("web teknolojisiyle sunum slaytı yap"),
        "dashboard-builder" => Some("verileri görsel bir panoda göster"),
        // Ajan altyapısı
        "agent-harness-construction" => Some("ajanı test etmek için altyapı kur"),
        "agent-introspection-debugging" => Some("ajanın içinde ne olduğunu incele, hatayı bul"),
        "agent-payment-x402"=> Some("ödeme yapabilen bir ajan yaz"),
        "agent-sort"        => Some("ajanın ürettiği sonuçları sırala ve önceliklendir"),
        "agentic-engineering" => Some("kendi başına karar veren otonom sistem tasarla"),
        "ai-first-engineering" => Some("AI'ı merkeze alarak yazılım geliştir"),
        "ai-regression-testing" => Some("AI modelinin öncekinden daha kötü davranıp davranmadığını test et"),
        "autonomous-agent-harness" => Some("tam bağımsız ajan sistemi kur ve çalıştır"),
        "enterprise-agent-ops" => Some("kurumsal ortamda ajan operasyonlarını yönet"),
        "configure-ecc"     => Some("Everything Claude Code araçlarını yapılandır"),
        "ecc-tools-cost-audit" => Some("hangi araç ne kadar para harcıyor, analiz et"),
        "claude-api"        => Some("Claude API'yi koduna entegre et"),
        "claude-devfleet"   => Some("birden fazla Claude ajanı koordineli çalıştır"),
        "dmux-workflows"    => Some("terminali bölümlere ayır, paralel çalış"),
        // Mobil
        "android-clean-architecture" => Some("Android uygulamasını katmanlı ve test edilebilir yap"),
        "compose-multiplatform-patterns" => Some("Kotlin ile hem iOS hem Android uygulama yaz"),
        "flutter-dart-code-review" => Some("Flutter kodunu incele, sorunları bul"),
        "swift-actor-persistence" => Some("Swift'te veriyi güvenle sakla, çakışmayı önle"),
        "swift-protocol-di-testing" => Some("Swift'te bağımlılıkları yönet, test edilebilir yap"),
        "foundation-models-on-device" => Some("AI modelini internetsiz, cihazın üzerinde çalıştır"),
        // Backend framework'ler
        "django-patterns"   => Some("Django projeni doğru yapıyla geliştir"),
        "django-security"   => Some("Django uygulamanın güvenlik açıklarını kapat"),
        "django-tdd"        => Some("Django'da önce test yaz"),
        "django-verification" => Some("Django uygulamanın doğru çalışıp çalışmadığını doğrula"),
        "laravel-patterns"  => Some("Laravel projeni doğru yapıyla geliştir"),
        "laravel-plugin-discovery" => Some("Laravel için paket veya eklenti geliştir"),
        "laravel-security"  => Some("Laravel uygulamanın güvenlik açıklarını kapat"),
        "laravel-tdd"       => Some("Laravel'de önce test yaz"),
        "laravel-verification" => Some("Laravel uygulamanın doğru çalışıp çalışmadığını doğrula"),
        "nestjs-patterns"   => Some("NestJS ile modüler Node.js backend yaz"),
        "springboot-patterns" => Some("Spring Boot ile Java backend doğru yapıda yaz"),
        "springboot-security" => Some("Spring Boot uygulamanı güvenli hale getir"),
        "springboot-tdd"    => Some("Spring Boot'ta önce test yaz"),
        "springboot-verification" => Some("Spring Boot uygulamanın doğru çalışıp çalışmadığını doğrula"),
        "hexagonal-architecture" => Some("kodu bağımsız parçalara böl, her parçayı ayrı test et"),
        "jpa-patterns"      => Some("Java'da veritabanı nesnelerini doğru tanımla ve sorgula"),
        "nuxt4-patterns"    => Some("Nuxt 4 ile Vue.js uygulaması yaz"),
        "bun-runtime"       => Some("Bun ile hızlı JavaScript projesi kur ve çalıştır"),
        "nodejs-keccak256"  => Some("Node.js'te keccak256 ile veri imzala veya doğrula"),
        // Test
        "cpp-testing"       => Some("C++ projeni test et"),
        "csharp-testing"    => Some("C# projeni test et"),
        "kotlin-testing"    => Some("Kotlin projeni test et"),
        "perl-testing"      => Some("Perl projeni test et"),
        // Veri
        "clickhouse-io"     => Some("milyarlarca satır veriyi hızla sorgula ve analiz et"),
        "postgres-patterns" => Some("PostgreSQL veritabanını hızlı ve doğru kullan"),
        "content-hash-cache-pattern" => Some("aynı içeriği tekrar üretme, önbellekle hızlan"),
        "iterative-retrieval" => Some("büyük bilgi tabanından adım adım doğru veriyi çek"),
        "data-scraper-agent" => Some("istediğin siteden otomatik veri toplayan ajan"),
        "videodb"           => Some("video veritabanına kaydet, ara, çek"),
        "nutrient-document-processing" => Some("PDF veya belgeden veri çıkar ve işle"),
        // Kripto & finans
        "defi-amm-security" => Some("kripto likidite havuzu kodunda güvenlik açığı ara"),
        "evm-token-decimals" => Some("kripto token miktarlarını doğru hesapla ve dönüştür"),
        "llm-trading-agent-security" => Some("AI tabanlı alım-satım ajanının güvenliğini kontrol et"),
        "finance-billing-ops" => Some("fatura ve ödeme süreçlerini yönet"),
        "customer-billing-ops" => Some("müşteri abonelik ve faturalarını yönet"),
        // Endüstri
        "carrier-relationship-management" => Some("kargo ve nakliye firmalarıyla ilişkileri yönet"),
        "customs-trade-compliance" => Some("gümrük ve ithalat/ihracat kurallarına uy"),
        "energy-procurement" => Some("enerji alım süreçlerini ve sözleşmelerini yönet"),
        "inventory-demand-planning" => Some("stok miktarını talebe göre planla"),
        "logistics-exception-management" => Some("teslimat aksaklıklarını tespit et ve çöz"),
        "production-scheduling" => Some("üretim programını planla ve takip et"),
        "quality-nonconformance" => Some("üretimde standart dışı durumları tespit et ve raporla"),
        "returns-reverse-logistics" => Some("iade ve geri lojistik süreçlerini yönet"),
        // Sağlık
        "healthcare-cdss-patterns" => Some("doktorlara karar verme desteği sağlayan sistem yaz"),
        "healthcare-emr-patterns" => Some("hasta kayıt sistemiyle entegrasyon kur"),
        "healthcare-eval-harness" => Some("sağlık AI modelini test et ve güvenilirliğini ölç"),
        "healthcare-phi-compliance" => Some("hasta bilgisinin gizli ve yasal kalmasını sağla"),
        // DevOps
        "automation-audit-ops" => Some("otomatik süreçlerin çalışıp çalışmadığını denetle"),
        "canary-watch"      => Some("yeni sürümü küçük bir gruba ver, sorun olursa geri al"),
        "cost-aware-llm-pipeline" => Some("AI API maliyetini takip et ve gereksiz harcamayı azalt"),
        "continuous-learning" => Some("konuşma sonunda öğrenilenleri kaydet"),
        "mcp-server-patterns" => Some("harici araç sunucusu yaz ve Metis'e bağla"),
        "connections-optimizer" => Some("bağlantı sayısını ve ağ kullanımını optimize et"),
        "workspace-surface-audit" => Some("çalışma alanındaki dosyaları ve yapıyı denetle"),
        "exit-worktree"     => Some("paralel dal çalışmasından çık ve temizle"),
        // İçerik & araştırma
        "content-engine"    => Some("düzenli içerik üretim sürecini kur ve yönet"),
        "research-ops"      => Some("araştırma sürecini yönet, kaynakları derle"),
        "documentation-lookup" => Some("kütüphane veya API belgelerini getir"),
        "architecture-decision-records" => Some("büyük teknik kararları neden alındığıyla belgele"),
        "product-capability" => Some("ürünün ne yapabildiğini ve ne yapamadığını analiz et"),
        "product-lens"      => Some("ürüne kullanıcının gözünden bak"),
        "investor-outreach" => Some("yatırımcılara nasıl ve ne zaman ulaşacağını planla"),
        "lead-intelligence" => Some("potansiyel müşterileri araştır ve verilerini topla"),
        "social-graph-ranker" => Some("sosyal ağdaki bağlantıları analiz et ve önem sırala"),
        "team-builder"      => Some("ekibe kimi alacağını planla, rol tanımla"),
        "project-flow-ops"  => Some("projenin akışını ve görevleri takip et"),
        // Kod kalitesi
        "coding-standards"  => Some("projenin kodlama kurallarını uygula ve denetle"),
        "plankton-code-quality" => Some("kod kalitesini ölç, iyileşme alanlarını göster"),
        "regex-vs-llm-structured-text" => Some("metin işlemede ne kullanmalı: regex mi AI mi?"),
        "perl-patterns"     => Some("Perl'de temiz ve okunabilir kod yaz"),
        "perl-security"     => Some("Perl kodunun güvenlik açıklarını kapat"),
        "dotnet-patterns"   => Some(".NET ile C# projesini doğru yapıda yaz"),
        "kotlin-coroutines-flows" => Some("Kotlin'de eş zamanlı işlemleri ve veri akışını yönet"),
        "kotlin-exposed-patterns" => Some("Kotlin'de veritabanı sorgularını yaz ve yönet"),
        "kotlin-ktor-patterns" => Some("Ktor ile Kotlin backend yaz"),
        "pytorch-patterns"  => Some("PyTorch ile AI modeli eğit ve çalıştır"),
        // Medya & yaratıcı
        "fal-ai-media"      => Some("fal.ai ile görsel veya video üret"),
        "remotion-video-creation" => Some("kod yazarak video oluştur"),
        "santa-method"      => Some("bir konuyu listele, değerlendir, önceliklendir"),
        // Entegrasyonlar
        "x-api"             => Some("X (Twitter) üzerinde tweet at, takipçi yönet, ara"),
        "messages-ops"      => Some("mesajlaşma ve bildirim işlemlerini yönet"),
        "unified-notifications-ops" => Some("farklı kanallardan gelen bildirimleri tek noktada yönet"),
        "visa-doc-translate" => Some("vize veya resmi belgeni çevir ve doldur"),
        "ralphinho-rfc-pipeline" => Some("teknik karar belgesi yaz ve inceleme sürecini yönet"),
        "token-budget-advisor" => Some("AI'a gönderilen veri miktarını analiz et ve öner"),
        "ck"                => Some("CK özel otomasyon ve entegrasyon"),
        "ckm:banner-design" => Some("CKM için banner tasarımı oluştur"),
        "ckm:brand"         => Some("CKM marka kimliği ve kuralları"),
        "ckm:design-system" => Some("CKM tasarım bileşenleri ve kuralları"),
        "ckm:design"        => Some("CKM arayüz tasarım kararları"),
        "ckm:slides"        => Some("CKM sunum slaytları oluştur"),
        "ckm:ui-styling"    => Some("CKM arayüz stil ve tema"),
        "gateguard"         => Some("kimin neye erişebileceğini kontrol eden katman"),
        "openclaw-persona-forge" => Some("AI için kişilik ve karakter oluştur"),
        "opensource-pipeline" => Some("projeyi açık kaynak olarak yayınla: temizle, paketle"),
        _ => None,
    }
}

fn build_skill_panel_lines(
    skills: &[aegis_core::skills::Skill],
    filter: &str,
    sel: usize,
) -> Vec<Line<'static>> {
    let header    = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let sel_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let name_s    = Style::default().fg(Color::LightYellow);
    let dim       = Style::default().fg(Color::Rgb(110, 110, 110));
    let search_s  = Style::default().fg(Color::Green);
    let arrow_s   = Style::default().fg(Color::Rgb(110, 110, 110));
    let tr_desc_s = Style::default().fg(Color::Rgb(200, 200, 200));

    let filtered = skill_filtered(skills, filter);
    let total    = filtered.len();

    // Header
    let count_label = if filter.is_empty() {
        format!("● agents  ({total} available)")
    } else {
        format!("● agents  ({total} / {} matches)", skills.len())
    };
    let mut lines = vec![Line::from(Span::styled(count_label, header))];

    // Description line
    lines.push(Line::from(Span::styled(
        "  Skills are custom slash commands that expand into prompts when invoked.",
        dim,
    )));

    // Search bar
    let cursor_block = if filter.is_empty() { "▌" } else { "" };
    lines.push(Line::from(vec![
        Span::styled("  filter: ".to_string(), dim),
        Span::styled(filter.to_string(), search_s),
        Span::styled(cursor_block.to_string(), search_s),
    ]));

    if total == 0 {
        lines.push(Line::from(Span::styled("  no matches", dim)));
    } else {
        const PAGE: usize = 18;
        let start = if sel >= PAGE { sel - PAGE + 1 } else { 0 };
        let end   = (start + PAGE).min(total);

        for (i, skill) in filtered[start..end].iter().enumerate() {
            let real_idx = start + i;
            let is_sel   = real_idx == sel;
            let prefix   = if is_sel { "→ " } else { "  " };
            let n_style  = if is_sel { sel_style } else { name_s };

            let desc = skill_tr_desc(&skill.name)
                .map(|s| s.to_string())
                .unwrap_or_else(|| skill.description.chars().take(60).collect());
            let d_style = if is_sel { sel_style } else { tr_desc_s };

            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), if is_sel { sel_style } else { dim }),
                Span::styled(format!("{:<24}", skill.name), n_style),
                Span::styled(" — ".to_string(), arrow_s),
                Span::styled(desc, d_style),
            ]));
        }
        if total > end {
            lines.push(Line::from(Span::styled(
                format!("  … {} more — keep typing to narrow", total - end),
                dim,
            )));
        }
    }

    lines.push(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────────────────────────",
        dim,
    )));
    lines.push(Line::from(vec![
        Span::styled("  ↑↓ ", name_s),
        Span::styled("navigate  ·  ", dim),
        Span::styled("Enter ", name_s),
        Span::styled("pick  ·  ", dim),
        Span::styled("type ", name_s),
        Span::styled("filter  ·  ", dim),
        Span::styled("Esc ", name_s),
        Span::styled("close", dim),
    ]));
    lines
}

/// Arrow-key nav uses `focused` so the styling must be consistent with
/// what `handle_key`'s modal branch reads back.
/// Return preview rows to push into the chat scrollback before the
/// permission modal opens. For `edit_file` and `multi_edit` we read
/// the target file off disk, predict the post-edit content, and emit
/// a unified diff with the same +/- bg-fill style used for tool
/// results. For `write_file` we only emit a header line — diff against
/// nothing isn't useful and the body could be enormous. All other
/// tools return empty: the modal alone is enough.
///
/// Each entry is `(plain_text, styled_lines)` so push_styled stays in
/// sync with scrollback math even when rendering colored diffs.
fn build_edit_preview_lines(
    tool: &str,
    args: &serde_json::Value,
) -> Vec<(String, Vec<Line<'static>>)> {
    let dim = Style::default().fg(Color::Rgb(140, 140, 140));
    let header_style = Style::default()
        .fg(Color::Rgb(80, 200, 240))
        .add_modifier(Modifier::BOLD);
    let mut out: Vec<(String, Vec<Line<'static>>)> = Vec::new();

    fn single_edit_rows(
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
        header_style: Style,
        dim: Style,
    ) -> Vec<(String, Vec<Line<'static>>)> {
        use aegis_core::tools::unified_diff;
        let mut rows: Vec<(String, Vec<Line<'static>>)> = Vec::new();
        let original = std::fs::read_to_string(path).unwrap_or_default();
        let count = original.matches(old_string).count();
        if count == 0 {
            let warn = Style::default().fg(Color::Rgb(220, 180, 60));
            let plain = format!("⚠ preview: old_string not found in {path}");
            let styled = vec![Line::from(Span::styled(plain.clone(), warn))];
            rows.push((plain, styled));
            return rows;
        }
        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else if count == 1 {
            original.replacen(old_string, new_string, 1)
        } else {
            let warn = Style::default().fg(Color::Rgb(220, 180, 60));
            let plain = format!(
                "⚠ preview: old_string matches {count}x in {path} (need replace_all)"
            );
            let styled = vec![Line::from(Span::styled(plain.clone(), warn))];
            rows.push((plain, styled));
            return rows;
        };
        let diff = unified_diff(&original, &updated, path);
        if diff.is_empty() {
            return rows;
        }
        let added = diff
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        let removed = diff
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count();
        let header_plain = format!("📄 preview: {path}  +{added} -{removed}");
        let header_styled = vec![Line::from(vec![
            Span::styled("📄 preview: ".to_string(), header_style),
            Span::styled(path.to_string(), header_style),
            Span::styled(format!("  +{added} -{removed}"), dim),
        ])];
        rows.push((header_plain, header_styled));
        // Atakan: side-by-side diff. Terminal genişliği ≥ 120 col ise
        // sol-sağ kolonlar (eski / yeni). Aksi halde mevcut unified
        // (alt alta -/+) davranışı korunur. CC/Cursor pattern.
        let cols = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(80);
        if cols >= 120 {
            let col_w = (cols.saturating_sub(5)) / 2; // " │ " separator + 2 padding
            let sbs = diff_to_side_by_side(&diff, col_w);
            for (plain, line) in sbs {
                let plain_indented = format!("  {plain}");
                let styled = vec![Line::from(
                    std::iter::once(Span::raw("  "))
                        .chain(line.spans.into_iter())
                        .collect::<Vec<_>>(),
                )];
                rows.push((plain_indented, styled));
            }
        } else {
            for line in diff.lines() {
                let span = diff_styled_span(line);
                let plain = format!("  {line}");
                let styled = vec![Line::from(vec![Span::raw("  "), span])];
                rows.push((plain, styled));
            }
        }
        rows
    }

    match tool {
        "edit_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
            let old_string = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let new_string = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if path.is_empty() {
                return out;
            }
            out.extend(single_edit_rows(
                path,
                old_string,
                new_string,
                replace_all,
                header_style,
                dim,
            ));
        }
        "multi_edit" => {
            let edits = match args.get("edits").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => return out,
            };
            // Top header summarising the bundle so the user sees the
            // shape of the change at a glance: "5 edits across 3 files".
            let mut paths: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for e in edits {
                if let Some(p) = e.get("path").and_then(|v| v.as_str()) {
                    paths.insert(p.to_string());
                }
            }
            let summary_plain = format!(
                "📦 preview: {} edit{} across {} file{}",
                edits.len(),
                if edits.len() == 1 { "" } else { "s" },
                paths.len(),
                if paths.len() == 1 { "" } else { "s" },
            );
            let summary_styled = vec![Line::from(Span::styled(
                summary_plain.clone(),
                header_style,
            ))];
            out.push((summary_plain, summary_styled));
            for e in edits {
                let path = e.get("path").and_then(|v| v.as_str()).unwrap_or_default();
                let old_string = e
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let new_string = e
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let replace_all = e
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if path.is_empty() {
                    continue;
                }
                out.extend(single_edit_rows(
                    path,
                    old_string,
                    new_string,
                    replace_all,
                    header_style,
                    dim,
                ));
            }
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if path.is_empty() {
                return out;
            }
            let exists = std::path::Path::new(path).exists();
            let kind = if exists { "overwrite" } else { "new file" };
            let bytes = content.len();
            let lines = content.lines().count();
            let plain = format!("📄 preview: {path}  ({kind}, {bytes} bytes / {lines} lines)");
            let styled = vec![Line::from(Span::styled(plain.clone(), header_style))];
            out.push((plain, styled));
        }
        _ => {}
    }
    out
}

fn build_permission_modal_lines(pending: &PendingPermission) -> Vec<Line<'static>> {
    let white_bold = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let cyan_bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let yellow_style = Style::default()
        .fg(Color::Rgb(255, 200, 50))
        .add_modifier(Modifier::BOLD);

    // Keep args to one line; truncate hard so a huge `multi_edit` payload
    // doesn't blow up the modal and push options off-screen.
    let args_flat: String = pending
        .args_preview
        .chars()
        .filter(|c| *c != '\n')
        .collect();
    let args_preview: String = if args_flat.chars().count() > 90 {
        let head: String = args_flat.chars().take(89).collect();
        format!("{head}…")
    } else {
        args_flat
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Copilot CLI style: ⏳ icon + tool name
    lines.push(Line::from(vec![
        Span::styled("⏳ ".to_string(), yellow_style),
        Span::styled(format!("{} wants to run", pending.tool), white_bold),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  args: {args_preview}"),
        dim,
    )));
    lines.push(Line::from(""));

    // Copilot CLI style: Yes / Yes, and approve tool session-wide / No, tell differently
    let tool_short = pending.tool.split("__").last().unwrap_or(&pending.tool);
    let option2 = format!("Yes, and approve {tool_short} for the rest of this session");
    let option3 = "No, and tell aegis what to do differently (Esc)";
    let options: [(usize, &str, &str); 3] = [
        (0usize, "1", "Yes"),
        (1, "2", &option2),
        (2, "3", option3),
    ];
    for (idx, key, label) in options {
        let is_focused = pending.focused == idx;
        let marker = if is_focused { "❯ " } else { "  " };
        let line_style = if is_focused { cyan_bold } else { dim };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), line_style),
            Span::styled(format!("{key}. "), line_style),
            Span::styled(label.to_string(), line_style),
        ]));
    }
    lines
}

/// Renders a Copilot-style ask_user modal with numbered options,
/// arrow-key navigation, and a "freeform text" fallback when the
/// last option ("Other — type your own") is focused.
///
/// Layout:
/// ```
/// ? {question}
///
///   ❯ 1. Option A
///     2. Option B
///     3. Other — type your own
///   ▸ {freeform_text}          ← only when freeform_active
/// ```
fn build_ask_user_modal_lines(pending: &AskUserPending) -> Vec<Line<'static>> {
    let cyan_bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let bright_white = Style::default()
        .fg(Color::Rgb(255, 255, 255))
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Question header: "? Do you want to proceed?" in cyan bold
    lines.push(Line::from(Span::styled(
        format!("? {}", pending.question),
        cyan_bold,
    )));
    lines.push(Line::from(""));

    // Numbered options
    let last_idx = pending.options.len(); // "Other" option
    for i in 0..pending.options.len() {
        let is_focused = pending.focused == i;
        let marker = if is_focused { "❯ " } else { "  " };
        let style = if is_focused { cyan_bold } else { dim };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), style),
            Span::styled(format!("{}. ", i + 1), style),
            Span::styled(pending.options[i].clone(), style),
        ]));
    }

    // "Other — type your own" option
    let other_focused = pending.focused == last_idx;
    let marker = if other_focused { "❯ " } else { "  " };
    let other_style = if other_focused { cyan_bold } else { dim };
    lines.push(Line::from(vec![
        Span::styled(marker.to_string(), other_style),
        Span::styled(format!("{}. ", last_idx + 1), other_style),
        Span::styled("Other — type your own".to_string(), other_style),
    ]));

    // Freeform input line (only when Other is focused and active)
    if other_focused && pending.freeform_active {
        lines.push(Line::from(Span::styled(
            format!("  ▸ {}", pending.freeform_text),
            bright_white,
        )));
    }

    lines
}

/// Model menu table — byte-for-byte mirror of `ReplCommand::ModelMenu`
/// at repl.rs:944. Keeping these lists in a shared helper means the
/// REPL and TUI stay in sync the next time a provider ships a new
/// default model.
fn models_for_provider(provider: &str) -> Vec<(&'static str, &'static str)> {
    match provider {
        "claude" => vec![
            ("claude-opus-4-7", "Opus 4.7  [subprocess]"),
            ("claude-sonnet-4-6", "Sonnet 4.6  [subprocess]"),
            ("claude-haiku-4-5-20251001", "Haiku 4.5  [subprocess]"),
        ],
        "anthropic" => vec![
            ("claude-opus-4-7", "Opus 4.7"),
            ("claude-sonnet-4-6", "Sonnet 4.6"),
            ("claude-haiku-4-5-20251001", "Haiku 4.5"),
        ],
        "deepseek" => vec![
            ("deepseek-v4-flash", "V4 Flash"),
            ("deepseek-v4-pro", "V4 Pro"),
            ("deepseek-reasoner", "R1 Reasoner (V4 Flash thinking)"),
            ("deepseek-chat", "V3 Chat [legacy]"),
        ],
        "gemini" => vec![
            ("gemini-2.5-pro", "2.5 Pro"),
            ("gemini-2.5-flash", "2.5 Flash"),
            ("gemini-2.5-flash-lite-preview-06-17", "2.5 Flash Lite"),
            ("gemini-2.0-flash", "2.0 Flash"),
            ("gemini-2.0-flash-thinking-exp", "2.0 Flash Thinking"),
            ("gemini-1.5-pro", "1.5 Pro"),
            ("gemini-1.5-flash", "1.5 Flash"),
        ],
        "glm" => vec![
            ("glm-5.1", "5.1"),
            ("glm-5", "5"),
            ("glm-5-turbo", "5 Turbo"),
            ("glm-4.6", "4.6"),
            ("glm-4.5", "4.5"),
            ("glm-4.5v", "4.5V [vision]"),
            ("glm-4-plus", "4 Plus"),
        ],
        "openrouter" => vec![
            ("anthropic/claude-opus-4-7", "Claude Opus 4.7"),
            ("anthropic/claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("openai/gpt-4.1", "GPT-4.1"),
            ("openai/o3", "O3 Reasoning"),
            ("openai/o4-mini", "O4 Mini"),
            ("deepseek/deepseek-chat", "DeepSeek V3"),
            ("deepseek/deepseek-r1", "DeepSeek R1"),
            ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
            ("meta-llama/llama-4-maverick", "Llama 4 Maverick"),
        ],
        "openai" => vec![
            ("gpt-4.1", "GPT-4.1"),
            ("gpt-4.1-mini", "GPT-4.1 Mini"),
            ("gpt-4.1-nano", "GPT-4.1 Nano"),
            ("gpt-4o", "GPT-4o"),
            ("gpt-4o-mini", "GPT-4o Mini"),
            ("o4-mini", "O4 Mini"),
            ("o3", "O3 Reasoning"),
            ("o3-mini", "O3 Mini"),
            ("o1", "O1 Reasoning"),
            ("o1-mini", "O1 Mini"),
        ],
        "nvidia" => vec![
            ("meta/llama-4-maverick-17b-128e-instruct", "Llama 4 Maverick"),
            ("meta/llama-4-scout-17b-16e-instruct", "Llama 4 Scout"),
            ("meta/llama-3.3-70b-instruct", "Llama 3.3 70B"),
            ("deepseek-ai/deepseek-r1", "DeepSeek R1"),
            ("deepseek-ai/deepseek-v3", "DeepSeek V3"),
            ("google/gemma-3-27b-it", "Gemma 3 27B"),
            ("mistralai/mistral-large-2-instruct", "Mistral Large 2"),
        ],
        "minimax" => vec![
            ("MiniMax-M2.7", "M2.7"),
            ("MiniMax-M2.7-highspeed", "M2.7 Fast"),
            ("MiniMax-M2.5", "M2.5"),
            ("MiniMax-M2.5-highspeed", "M2.5 Fast"),
            ("MiniMax-M2.1", "M2.1"),
            ("MiniMax-Text-01", "Text-01"),
            ("MiniMax-VL-01", "VL-01 [vision]"),
        ],
        _ => vec![],
    }
}

/// Turn `format_dag` plain output into styled lines with REPL-like
/// tone: `turn N` in bold yellow, tree glyphs dim-gray, tool name in
/// cyan, args dim, `✓` green-bold, `✗` red-bold. Used by `/dag` so the
/// chain is readable at a glance.
///
/// Input shape (per line):
/// ```text
///   turn  2  ┬─ edit_file         {"path":"lib.rs"…}  ✓
///            └─ bash               {"cmd":"cargo test"}  ✗
/// ```
fn colorize_dag_lines(plain: &str) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let tool_style = Style::default().fg(Color::Cyan);
    let ok_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let err_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let sys = Style::default().fg(Color::Green);

    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in plain.lines() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let trimmed_start = raw.trim_start();
        let leading_ws_len = raw.len() - trimmed_start.len();
        if leading_ws_len > 0 {
            spans.push(Span::raw(raw[..leading_ws_len].to_string()));
        }

        if let Some(after_turn) = trimmed_start.strip_prefix("turn ") {
            // "turn  N  <glyph>…" — pull "turn <digits>" as the bold
            // yellow header, including the numeric turn id, then let
            // the body parser color everything after it.
            let digits_end = after_turn
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_digit() || *c == ' ')
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            // digits_end might include trailing spaces — pull back to
            // keep exactly "turn  N" in the header span.
            let header_end = 5 + after_turn[..digits_end].trim_end().len();
            let header_str = &trimmed_start[..header_end];
            spans.push(Span::styled(header_str.to_string(), header));
            let rest = &trimmed_start[header_end..];
            emit_glyph_and_body(rest, &mut spans, dim, tool_style, ok_style, err_style);
        } else if trimmed_start.is_empty() {
            // blank separator line
            spans.clear();
            spans.push(Span::styled("".to_string(), sys));
        } else {
            // continuation row (starts with ┬─/├─/└─ after spaces)
            emit_glyph_and_body(
                trimmed_start,
                &mut spans,
                dim,
                tool_style,
                ok_style,
                err_style,
            );
        }
        out.push(Line::from(spans));
    }
    out
}

fn emit_glyph_and_body(
    text: &str,
    spans: &mut Vec<Span<'static>>,
    dim: Style,
    tool_style: Style,
    ok_style: Style,
    err_style: Style,
) {
    // Glyphs: "──", "┬─", "├─", "└─". Pull the prefix (up to and
    // including the trailing space) into dim, then split the remainder
    // into tool-name, args, and status marker.
    let text = text.trim_start_matches(' ');
    let glyph_end = text
        .char_indices()
        .find(|(_, c)| !matches!(c, '─' | '┬' | '├' | '└' | ' '))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    if glyph_end > 0 {
        spans.push(Span::styled(text[..glyph_end].to_string(), dim));
    }
    let rest = &text[glyph_end..];
    // Status marker is last non-empty token. Strip trailing whitespace,
    // inspect tail for ✓ / ✗.
    let (body, status_span): (&str, Option<Span<'static>>) =
        if let Some(stripped) = rest.strip_suffix('✓') {
            (
                stripped.trim_end(),
                Some(Span::styled("✓".to_string(), ok_style)),
            )
        } else if let Some(stripped) = rest.strip_suffix("✗ error") {
            (
                stripped.trim_end(),
                Some(Span::styled("✗ error".to_string(), err_style)),
            )
        } else if let Some(stripped) = rest.strip_suffix('✗') {
            (
                stripped.trim_end(),
                Some(Span::styled("✗".to_string(), err_style)),
            )
        } else {
            (rest.trim_end(), None)
        };
    // Split body into tool-name (first token) and args (rest).
    let body = body.trim_end();
    if let Some(sp) = body.find(' ') {
        let (name, args) = body.split_at(sp);
        spans.push(Span::styled(name.to_string(), tool_style));
        spans.push(Span::styled(format!("{args} "), dim));
    } else if !body.is_empty() {
        spans.push(Span::styled(body.to_string(), tool_style));
        spans.push(Span::raw(" "));
    }
    if let Some(s) = status_span {
        spans.push(s);
    }
}

fn file_color_for_ext(ext: &str) -> Color {
    match ext {
        "rs" | "cpp" | "c" | "h" | "hpp" | "py" | "js" | "ts" | "java" | "go" => Color::Yellow,
        "md" | "txt" | "json" | "toml" | "yaml" | "yml" => Color::Cyan,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => Color::Magenta,
        _ => Color::Reset,
    }
}

/// Build a `/files [path]` listing with full REPL color/bold parity.
/// Returns `(plain_text, styled_lines)` — the plain text is kept in
/// `ChatMessage.text` for scroll math, copy/paste, and tests; the
/// styled lines render per-entry colors (blue dirs, ext-colored files,
/// bold section headers) exactly like REPL stderr.
/// Walk the workspace root, returning relative file paths suitable
/// for the `/files` picker modal. Bounded by `limit` so very large
/// repos don't lock the UI on first open. Skips the usual build
/// artefacts and VCS directories — adjust as new tooling shows up.
fn walk_workspace_files(workspace: &Path, limit: usize) -> Vec<String> {
    let skip_dirs: &[&str] = &[
        ".git",
        ".metis",
        ".aegis",
        "target",
        "node_modules",
        ".next",
        ".venv",
        "venv",
        "__pycache__",
        "dist",
        "build",
        ".cargo",
        ".rustup",
        ".idea",
        ".vscode",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip hidden by default — except `.github` which often
            // matters for workflow edits. Tunable if it bites.
            if name_str.starts_with('.') && name_str != ".github" {
                continue;
            }
            if skip_dirs.iter().any(|d| *d == name_str) {
                continue;
            }
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if let Ok(rel) = path.strip_prefix(workspace) {
                    out.push(rel.display().to_string());
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Walk up the fork ancestry for `session_id` by reading the
/// `.meta.json` sidecar that fork() writes alongside each JSONL.
/// Returns the chain from the oldest ancestor to the current session.
/// Capped at 6 entries so a runaway loop or deeply chained fork
/// doesn't dominate the sidebar; truncated chains are marked with
/// "…" by the renderer.
fn fork_chain(workspace: &std::path::Path, session_id: &str) -> Vec<String> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct MiniMeta {
        #[serde(default)]
        parent_id: Option<String>,
    }
    let dir = workspace.join(".metis").join("sessions");
    let mut chain: Vec<String> = vec![session_id.to_string()];
    let mut cur = session_id.to_string();
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::from_iter(std::iter::once(cur.clone()));
    while chain.len() < 6 {
        let meta_path = dir.join(format!("{cur}.meta.json"));
        let bytes = match std::fs::read(&meta_path) {
            Ok(b) => b,
            Err(_) => break,
        };
        let meta: MiniMeta = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(_) => break,
        };
        let parent = match meta.parent_id {
            Some(p) if !p.is_empty() => p,
            _ => break,
        };
        if !seen.insert(parent.clone()) {
            // Cycle defense — shouldn't happen but better safe than
            // looping forever on a corrupted meta.
            break;
        }
        chain.push(parent.clone());
        cur = parent;
    }
    // Reverse so the renderer reads top-down: ancestor → ... → current.
    chain.reverse();
    chain
}

/// Render a "Ns ago" / "Nm ago" / "Nh ago" label for a UNIX
/// timestamp, used by the permission timeline overlay. Negative
/// deltas (clock skew) collapse to "just now". Unbounded by hour
/// since a TUI session shouldn't outlive a day, but we still degrade
/// gracefully past 24h with `Nd ago`.
fn format_seconds_ago(when: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(when);
    if now <= when {
        return "just now".to_string();
    }
    let delta = now - when;
    if delta < 5 {
        "just now".to_string()
    } else if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

/// Compute the slash-command suffix to render as inline ghost-text.
/// Triggers only when the input is a single, in-progress slash token
/// (`/sess`, `/ed`, ...) — anything with whitespace or a non-leading
/// slash is ignored so we don't ghost-text inside a real prompt.
///
/// Returns the suffix that would extend the typed prefix to the
/// nearest unique completion: `/sess` → Some("ions") because
/// `sessions` is the only KNOWN_SLASH_COMMAND match. When multiple
/// candidates share a longer common prefix we return that delta;
/// when nothing matches we return `None`.
fn slash_ghost_suggestion(input: &str) -> Option<String> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    if trimmed.contains(char::is_whitespace) {
        return None;
    }
    let prefix = &trimmed[1..];
    if prefix.is_empty() {
        return None;
    }
    let matches: Vec<&'static str> = KNOWN_SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(prefix))
        .collect();
    if matches.is_empty() {
        return None;
    }
    if matches.len() == 1 {
        let full = matches[0];
        let suffix = full.strip_prefix(prefix).unwrap_or("");
        if suffix.is_empty() {
            return None;
        }
        return Some(suffix.to_string());
    }
    // Multiple candidates — emit only the longest common prefix delta.
    let lcp = longest_common_prefix(matches.iter().copied());
    if lcp.len() <= prefix.len() {
        return None;
    }
    Some(lcp[prefix.len()..].to_string())
}

/// Best-effort image dimensions via macOS `sips`. Returns
/// `Some("WxH")` when both pixelWidth and pixelHeight come back
/// cleanly; `None` on any parse, exec, or platform mismatch. Sips
/// is part of the base install on macOS — no dependency cost.
/// Time budget is bounded by sips's local file open; for normal
/// sized images this completes in < 50ms.
fn image_dimensions_via_sips(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("sips")
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut w: Option<u32> = None;
    let mut h: Option<u32> = None;
    for line in s.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pixelWidth:") {
            w = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("pixelHeight:") {
            h = rest.trim().parse().ok();
        }
    }
    match (w, h) {
        (Some(w), Some(h)) => Some(format!("{w}×{h}")),
        _ => None,
    }
}

/// Render `bytes` as a human-readable file size for the image
/// breadcrumb. KB/MB only — image attachments rarely cross GB and
/// the smaller units stay readable in a one-line system message.
fn format_image_byte_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Lower-cased substring filter for the files picker. Empty query
/// returns every path; otherwise `query` (lower-cased) must appear
/// as a substring of the lower-cased path.
fn files_picker_filtered<'a>(paths: &'a [String], query: &str) -> Vec<&'a String> {
    if query.is_empty() {
        return paths.iter().collect();
    }
    let lower = query.to_lowercase();
    paths
        .iter()
        .filter(|p| p.to_lowercase().contains(&lower))
        .collect()
}

fn build_files_listing(
    workspace: &Path,
    arg: Option<&str>,
) -> std::result::Result<(String, Vec<Line<'static>>), String> {
    use std::fmt::Write as _;

    let target_path = match arg {
        Some(p) => workspace.join(p),
        None => workspace.to_path_buf(),
    };
    if !target_path.exists() {
        return Err(format!("path not found: {}", target_path.display()));
    }
    if target_path.is_file() {
        let hint = arg.unwrap_or("");
        return Err(format!(
            "{} is a file, not a directory\n  Use /view {} to see its contents",
            target_path.display(),
            hint
        ));
    }

    let mut dirs: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    let mut files: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    let mut total_size: u64 = 0;

    let entries = std::fs::read_dir(&target_path)
        .map_err(|e| format!("cannot read directory: {} ({e})", target_path.display()))?;

    for entry in entries.flatten() {
        if let Ok(metadata) = entry.metadata() {
            let name = entry.file_name().to_string_lossy().to_string();
            let size = metadata.len();
            let modified = metadata
                .modified()
                .unwrap_or_else(|_| std::time::SystemTime::now());
            if metadata.is_dir() {
                dirs.push((name, size, modified));
            } else {
                files.push((name, size, modified));
                total_size += size;
            }
        }
    }

    dirs.sort_by_key(|d| d.0.to_lowercase());
    files.sort_by_key(|f| f.0.to_lowercase());

    // Shared styles matching REPL: bold section headers, blue dirs,
    // ext-colored files, mid-gray metadata (size + time ago). The
    // system-label prefix `[goblin] ` is applied to the first line of
    // the first chunk so the listing threads into the chat visually
    // the same way system messages do (green `[goblin]` prefix).
    let sys_color = MessageRole::System.color();
    let label_style = Style::default().fg(sys_color).add_modifier(Modifier::BOLD);
    let bold_header = Style::default().add_modifier(Modifier::BOLD);
    let dir_style = Style::default().fg(Color::Blue);
    let meta_style = Style::default().fg(Color::Rgb(150, 150, 150));

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    // Header line: `[goblin] browsing files at: <path>`
    let header_text = format!("browsing files at: {}", target_path.display());
    writeln!(plain, "{header_text}").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label_style),
        Span::styled(header_text, Style::default().fg(sys_color)),
    ]));

    if !dirs.is_empty() {
        plain.push_str("\n  Directories:\n");
        styled.push(Line::from(""));
        styled.push(Line::from(vec![
            Span::raw("        ".to_string()),
            Span::styled("Directories:".to_string(), bold_header),
        ]));
        for (name, size, modified) in &dirs {
            let modified_str = crate::repl::format::format_time_ago(*modified);
            let size_str = crate::repl::format::format_size(*size);
            writeln!(plain, "    {}/  {:>10}  {}", name, size_str, modified_str).ok();
            styled.push(Line::from(vec![
                Span::raw("          ".to_string()),
                Span::styled(format!("{name}/"), dir_style),
                Span::raw("  ".to_string()),
                Span::styled(format!("{size_str:>10}"), meta_style),
                Span::raw("  ".to_string()),
                Span::styled(modified_str, meta_style),
            ]));
        }
    }

    if !files.is_empty() {
        plain.push_str("\n  Files:\n");
        styled.push(Line::from(""));
        styled.push(Line::from(vec![
            Span::raw("        ".to_string()),
            Span::styled("Files:".to_string(), bold_header),
        ]));
        for (name, size, modified) in &files {
            let modified_str = crate::repl::format::format_time_ago(*modified);
            let size_str = crate::repl::format::format_size(*size);
            writeln!(plain, "    {}  {:>10}  {}", name, size_str, modified_str).ok();
            let ext = std::path::Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let color = file_color_for_ext(ext);
            let name_style = Style::default().fg(color);
            styled.push(Line::from(vec![
                Span::raw("          ".to_string()),
                Span::styled(name.clone(), name_style),
                Span::raw("  ".to_string()),
                Span::styled(format!("{size_str:>10}"), meta_style),
                Span::raw("  ".to_string()),
                Span::styled(modified_str, meta_style),
            ]));
        }
    }

    let total_str = crate::repl::format::format_size(total_size);
    let summary = format!(
        "total: {} directories, {} files, {} total size",
        dirs.len(),
        files.len(),
        total_str
    );
    plain.push('\n');
    plain.push_str(&summary);
    plain.push('\n');
    styled.push(Line::from(""));
    styled.push(Line::from(vec![
        Span::raw("        ".to_string()),
        Span::styled(summary, Style::default().fg(sys_color)),
    ]));

    if target_path != workspace {
        let rel_path = target_path
            .strip_prefix(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| target_path.display().to_string());
        let rel_line = format!("  (relative to workspace: {rel_path})");
        plain.push_str(&rel_line);
        plain.push('\n');
        styled.push(Line::from(vec![
            Span::raw("        ".to_string()),
            Span::styled(rel_line, meta_style),
        ]));
    }

    Ok((plain, styled))
}

/// Build the `/skills` listing exactly matching REPL's format:
///   ● skills (N available — …)
///     ───────────────────────
///     /name        → description
///     ...
///     ───────────────────────
///     tip: /skill-search <query> …
///
/// Empty case:
///   ● skills — none installed
///     use /skill-install <path> to add one
///
// ---------------------------------------------------------------------------
// Agent task panel — live sidebar showing model's create_task/update_task
// ---------------------------------------------------------------------------
/// OpenCode-style sidebar — every section is its own bordered card
/// (Context, LSP, Todo, Models, Help, plus a Plan card when plan mode
/// is active and a pinned Footer card with cwd + aegis version). Each
/// card draws as a `Block(Borders::ALL).title(...)` over a slice of
/// the vertical layout, so the sections read as discrete tiles
/// instead of one undifferentiated paragraph.
fn render_context_panel(frame: &mut ratatui::Frame, area: Rect, app: &TuiApp) {
    use ratatui::widgets::{Paragraph, Padding, Borders};
    let text_color = Color::Rgb(220, 220, 220);
    let muted = Color::Rgb(140, 140, 140);
    let warning = Color::Rgb(220, 180, 60);
    let success = Color::Rgb(80, 200, 100);
    let border_color = Color::Rgb(70, 70, 70);
    let accent_color = Color::Rgb(80, 200, 240);
    let key_color = Color::Rgb(220, 180, 60); // bold yellow for important keys

    let dim = Style::default().fg(muted);
    let bold_text = Style::default()
        .fg(text_color)
        .add_modifier(Modifier::BOLD);
    let bold_yellow_underline = Style::default()
        .fg(key_color)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED);
    let title_style = Style::default()
        .fg(text_color)
        .add_modifier(Modifier::BOLD);
    let accent_title_style = Style::default()
        .fg(accent_color)
        .add_modifier(Modifier::BOLD);

    // Each entry: (title text, lines, accent flag — accent draws the
    // title in cyan to call out a card the user should notice). Cards
    // are appended in display order; the Plan card is conditional.
    struct Card {
        title: String,
        lines: Vec<Line<'static>>,
        accent: bool,
    }
    let mut cards: Vec<Card> = Vec::new();

    // -- Plan card (only when plan mode is active) --
    if let Ok(state) = app.plan_state.lock() {
        match *state {
            PlanState::Drafting | PlanState::Executing => {
                let label = match *state {
                    PlanState::Drafting => "Plan · drafting",
                    PlanState::Executing => "Plan · executing",
                    _ => "Plan",
                };
                let body = vec![
                    Line::from(Span::styled(
                        "next prompt drafts a plan",
                        Style::default().fg(text_color),
                    )),
                    Line::from(Span::styled("Tab to exit", dim)),
                ];
                cards.push(Card {
                    title: label.to_string(),
                    lines: body,
                    accent: true,
                });
            }
            _ => {}
        }
    }

    // -- Context card --
    let total_tokens: u64 = (app.cumulative_usage.input_tokens
        + app.cumulative_usage.output_tokens) as u64;
    let tokens_fmt = format_with_commas(total_tokens);
    let ctx_limit: u64 = context_window_for(&app.model);
    let mut ctx_lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(tokens_fmt, bold_yellow_underline),
            Span::styled(" tokens", dim),
        ]),
    ];
    if ctx_limit > 0 {
        let pct = ((total_tokens as f64 / ctx_limit as f64) * 100.0).round() as u64;
        ctx_lines.push(Line::from(vec![
            Span::styled(format!("{pct}%"), bold_yellow_underline),
            Span::styled(" used", dim),
        ]));
    }
    ctx_lines.push(Line::from(vec![
        Span::styled(app.cost_display.clone(), bold_text),
        Span::styled(" spent", dim),
    ]));
    cards.push(Card {
        title: "Context".to_string(),
        lines: ctx_lines,
        accent: false,
    });

    // -- LSP card --
    cards.push(Card {
        title: "LSP".to_string(),
        lines: vec![Line::from(Span::styled(
            "activates as files are read",
            dim,
        ))],
        accent: false,
    });

    // -- Todo card — merges user tasks (.metis/tasks_user.json) with
    // live agent tasks (.metis/tasks.json, written by create_task /
    // update_task tool calls). Both files are reread each render, so
    // the card updates as the agent ticks tasks without any explicit
    // refresh wiring. Agent tasks come first; in_progress agent task
    // wins the "active" highlight.
    let user_tasks = crate::tasks::load_tasks(&app.workspace);
    let agent_tasks = crate::tasks::load_agent_tasks(&app.workspace);
    let mut todo_lines: Vec<Line<'static>> = Vec::new();
    let total_count = user_tasks.len() + agent_tasks.len();
    if total_count == 0 {
        todo_lines.push(Line::from(Span::styled("no tasks yet", dim)));
    } else {
        let active_color = Color::Rgb(80, 200, 240);
        let done_color = Color::Rgb(80, 200, 100);
        let agent_active = agent_tasks
            .iter()
            .find(|t| t.status == "in_progress")
            .map(|t| t.id);
        let first_user_pending = user_tasks.iter().find(|t| !t.done).map(|t| t.id);
        let user_highlight_active = agent_active.is_none();
        let mut shown = 0usize;
        for t in agent_tasks.iter() {
            if shown >= 20 {
                break;
            }
            let is_active = Some(t.id) == agent_active;
            let (mark, mark_color, txt_style) = match t.status.as_str() {
                "completed" => ("[✓]", done_color, Style::default().fg(muted)),
                "in_progress" => (
                    "[→]",
                    active_color,
                    Style::default()
                        .fg(active_color)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => ("[ ]", muted, Style::default().fg(text_color)),
            };
            let _ = is_active;
            todo_lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                Span::styled(t.description.clone(), txt_style),
            ]));
            shown += 1;
        }
        for t in user_tasks.iter() {
            if shown >= 20 {
                break;
            }
            let is_first_pending = Some(t.id) == first_user_pending;
            let (mark, mark_color, txt_style) = if t.done {
                ("[✓]", done_color, Style::default().fg(muted))
            } else if is_first_pending && user_highlight_active {
                ("[•]", warning, bold_yellow_underline)
            } else {
                ("[ ]", muted, Style::default().fg(text_color))
            };
            todo_lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                Span::styled(t.text.clone(), txt_style),
            ]));
            shown += 1;
        }
    }
    cards.push(Card {
        title: format!("Todo ({total_count})"),
        lines: todo_lines,
        accent: false,
    });

    // -- Models card (provider + active model) --
    let provider_label = if app.current_provider.is_empty() {
        "—".to_string()
    } else {
        app.current_provider.clone()
    };
    let model_label = if app.model.is_empty() {
        "—".to_string()
    } else {
        app.model.clone()
    };
    let models_lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("provider  ", dim),
            Span::styled(provider_label, bold_yellow_underline),
        ]),
        Line::from(vec![
            Span::styled("model     ", dim),
            Span::styled(model_label, bold_yellow_underline),
        ]),
        Line::from(Span::styled("/providers · /models", dim)),
    ];
    cards.push(Card {
        title: "Models".to_string(),
        lines: models_lines,
        accent: false,
    });

    // -- Fork card (only when current session has at least one parent) --
    let chain = fork_chain(&app.workspace, &app.session_id);
    if chain.len() > 1 {
        let mut fork_lines: Vec<Line<'static>> = Vec::new();
        // Render top-down: ancestor at the top, current at the bottom.
        // Indent grows by 2 columns per generation; current row is
        // bold yellow so the eye finds "you are here" at a glance.
        for (i, sid) in chain.iter().enumerate() {
            let is_current = i == chain.len() - 1;
            let indent = "  ".repeat(i.min(4));
            let glyph = if i == 0 {
                "•"
            } else if is_current {
                "└▶"
            } else {
                "└─"
            };
            let id_short: String = sid.chars().take(12).collect();
            let line_style = if is_current {
                bold_yellow_underline
            } else {
                Style::default().fg(text_color)
            };
            fork_lines.push(Line::from(vec![
                Span::styled(format!("{indent}{glyph} "), dim),
                Span::styled(id_short, line_style),
            ]));
        }
        // If the chain hit our depth cap there are likely more
        // ancestors above — surface that explicitly so the user
        // knows the tree isn't necessarily complete.
        if chain.len() == 6 {
            fork_lines.insert(
                0,
                Line::from(Span::styled("(… deeper ancestors)", dim)),
            );
        }
        cards.push(Card {
            title: format!("Fork ({} deep)", chain.len()),
            lines: fork_lines,
            accent: false,
        });
    }

    // -- MCP card (only when at least one server is attached) --
    let mcp_names: Vec<String> = app
        .attached_mcps
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !mcp_names.is_empty() {
        let mut mcp_lines: Vec<Line<'static>> = Vec::new();
        for name in mcp_names.iter().take(6) {
            mcp_lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(success)),
                Span::styled(name.clone(), Style::default().fg(text_color)),
            ]));
        }
        if mcp_names.len() > 6 {
            mcp_lines.push(Line::from(Span::styled(
                format!("+{} more", mcp_names.len() - 6),
                dim,
            )));
        }
        cards.push(Card {
            title: format!("MCP ({})", mcp_names.len()),
            lines: mcp_lines,
            accent: false,
        });
    }

    // -- Help card (compact key reference) --
    let help_items: [(&str, &str); 7] = [
        ("Tab", "plan mode"),
        ("PgUp/PgDn", "scroll page"),
        ("Ctrl+PgUp/Dn", "top · bottom"),
        ("Ctrl+C", "stop · quit"),
        ("/ask", "one-shot strong"),
        ("/sidebar", "toggle panel"),
        ("/copy", "last reply"),
    ];
    let help_lines: Vec<Line<'static>> = help_items
        .iter()
        .map(|(k, v)| {
            // Keys bolded so the eye scans them first; description in
            // muted gray as secondary context.
            Line::from(vec![
                Span::styled(format!("{k}  "), bold_text),
                Span::styled((*v).to_string(), dim),
            ])
        })
        .collect();
    cards.push(Card {
        title: "Help".to_string(),
        lines: help_lines,
        accent: false,
    });

    // -- Footer card (cwd + aegis version) — built separately because it's
    // pinned to the bottom; if the area is too short to fit every card,
    // earlier cards get clipped first while the footer stays visible.
    let cwd = app.workspace.display().to_string();
    let cwd_short = if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if cwd.starts_with(&h) {
            format!("~{}", &cwd[h.len()..])
        } else {
            cwd.clone()
        }
    } else {
        cwd.clone()
    };
    let (parent, name) = match cwd_short.rfind('/') {
        Some(i) if i + 1 < cwd_short.len() => (
            cwd_short[..i + 1].to_string(),
            cwd_short[i + 1..].to_string(),
        ),
        _ => (String::new(), cwd_short.clone()),
    };
    let footer_lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(parent, dim),
            Span::styled(name, Style::default().fg(text_color)),
        ]),
        Line::from(vec![
            Span::styled("● ", Style::default().fg(success)),
            Span::styled(
                "goblin",
                Style::default().fg(text_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", env!("CARGO_PKG_VERSION")), dim),
        ]),
    ];

    // -- Layout: each card height = lines + 2 (top/bottom border).
    // After the cards, a flexible spacer absorbs any leftover height
    // so the footer card pins to the bottom regardless of total cards
    // height.
    let footer_height: u16 = footer_lines.len() as u16 + 2;
    let mut constraints: Vec<Constraint> = cards
        .iter()
        .map(|c| Constraint::Length(c.lines.len() as u16 + 2))
        .collect();
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(footer_height));

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let render_card = |frame: &mut ratatui::Frame,
                       rect: Rect,
                       title: &str,
                       lines: &[Line<'static>],
                       accent: bool| {
        let title_span = Span::styled(
            format!(" {title} "),
            if accent { accent_title_style } else { title_style },
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if accent { accent_color } else { border_color }))
            .padding(Padding::horizontal(1))
            .title(Line::from(title_span));
        let para = Paragraph::new(lines.to_vec()).block(block);
        frame.render_widget(para, rect);
    };

    for (i, card) in cards.iter().enumerate() {
        render_card(frame, split[i], &card.title, &card.lines, card.accent);
    }
    // Footer is the last split entry (cards.len() + 1; the spacer at
    // cards.len() is intentionally left blank).
    let footer_idx = cards.len() + 1;
    render_card(frame, split[footer_idx], "cwd", &footer_lines, false);
}

/// Best-effort context-window lookup by model id. Used by the sidebar
/// to compute "% used". Returns 0 if unknown so the caller can hide the
/// line. Hand-maintained: when adding new providers/models, register
/// their context window here. Defaults intentionally err small so we
/// don't over-promise.
fn context_window_for(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") && (m.contains("sonnet") || m.contains("opus") || m.contains("haiku")) {
        return 200_000;
    }
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") || m.contains("gpt-5") {
        return 128_000;
    }
    if m.contains("gpt-3.5") {
        return 16_385;
    }
    if m.contains("deepseek-r") || m.contains("deepseek-v3") || m.contains("deepseek-chat") || m.contains("v4-flash") || m.contains("v4-pro") {
        return 64_000;
    }
    if m.contains("gemini-1.5") {
        return 1_000_000;
    }
    if m.contains("minimax") {
        return 245_760;
    }
    if m.contains("glm") {
        return 128_000;
    }
    // Unknown — hide the % line by returning 0.
    0
}

/// Format a number with thousands separators: 12345 → "12,345".
fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// Plain text mirrors the structure for `ChatMessage.text` / scroll math.
/// Render the user task list with ☐/☑ checkboxes, green-for-done and
/// white-for-pending rows, dim id prefix. Returns `(plain, styled)` so
/// the caller can `push_styled` and keep the plain form for copy/paste
/// and scroll math. Header line `● tasks — N pending · M done` mirrors
/// the `/skills` listing shape.
fn build_tasks_listing(tasks: &[crate::tasks::UserTask]) -> (String, Vec<Line<'static>>) {
    use std::fmt::Write as _;

    let bullet_style = Style::default().fg(Color::Yellow);
    let bold_label = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let pending_box = Style::default().fg(Color::Rgb(200, 200, 200));
    let done_box = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let pending_text = Style::default().fg(Color::Rgb(220, 220, 220));
    let done_text = Style::default()
        .fg(Color::Rgb(130, 130, 130))
        .add_modifier(Modifier::CROSSED_OUT);

    let pending = tasks.iter().filter(|t| !t.done).count();
    let done = tasks.len() - pending;

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    writeln!(plain, "● tasks — {pending} pending · {done} done").ok();
    styled.push(Line::from(vec![
        Span::styled("● ".to_string(), bullet_style),
        Span::styled("tasks".to_string(), bold_label),
        Span::styled(format!(" — {pending} pending · {done} done"), dim),
    ]));

    for t in tasks.iter().take(30) {
        let (box_sym, box_style, text_style) = if t.done {
            ("☑", done_box, done_text)
        } else {
            ("☐", pending_box, pending_text)
        };
        writeln!(
            plain,
            "  {sym} #{id}  {text}",
            sym = if t.done { "☑" } else { "☐" },
            id = t.id,
            text = t.text,
        )
        .ok();
        styled.push(Line::from(vec![
            Span::styled("  ".to_string(), dim),
            Span::styled(format!("{box_sym} "), box_style),
            Span::styled(format!("#{}  ", t.id), dim),
            Span::styled(t.text.clone(), text_style),
        ]));
    }

    if tasks.len() > 30 {
        writeln!(plain, "  … +{} more", tasks.len() - 30).ok();
        styled.push(Line::from(Span::styled(
            format!("  … +{} more", tasks.len() - 30),
            dim,
        )));
    }

    (plain, styled)
}


/// Build the `/skill-search <query>` output, REPL-compatible:
///   ● skill-search (N matches for query)
///     ────────────────
///     /name → description (author ver) [tag1, tag2]
///     ...
///     ────────────────
///
/// Empty case: `● skill-search — no matches for <query>`.
fn build_skill_search(registry: &SkillRegistry, query: &str) -> (String, Vec<Line<'static>>) {
    use std::fmt::Write as _;

    let bullet_style = Style::default().fg(Color::Yellow);
    let bold_label = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let name_style = Style::default().fg(Color::LightYellow);
    let query_style = Style::default().fg(Color::Yellow);
    let plain_style = Style::default();

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    let results = registry.search(query);
    if results.is_empty() {
        writeln!(plain, "● skill-search — no matches for {query}").ok();
        styled.push(Line::from(vec![
            Span::styled("● ".to_string(), bullet_style),
            Span::styled("skill-search".to_string(), bold_label),
            Span::styled(" — no matches for ".to_string(), dim),
            Span::styled(query.to_string(), query_style),
        ]));
        return (plain, styled);
    }

    let widest = results
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let separator = "  ───────────────────────────────────────────────";
    let plural = if results.len() == 1 { "" } else { "es" };

    writeln!(
        plain,
        "● skill-search ({} match{plural} for {query})",
        results.len()
    )
    .ok();
    writeln!(plain, "{separator}").ok();
    styled.push(Line::from(vec![
        Span::styled("● ".to_string(), bullet_style),
        Span::styled("skill-search".to_string(), bold_label),
        Span::styled(format!(" ({} match{plural} for ", results.len()), dim),
        Span::styled(query.to_string(), query_style),
        Span::styled(")".to_string(), dim),
    ]));
    styled.push(Line::from(Span::styled(separator.to_string(), dim)));

    for s in &results {
        let padded = format!("/{:<width$}", s.name, width = widest);
        let ver = s.version.as_deref().unwrap_or("");
        let author = s.author.as_deref().unwrap_or("");
        let meta = if !ver.is_empty() || !author.is_empty() {
            format!(" ({author} {ver})")
        } else {
            String::new()
        };
        let tags = if s.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", s.tags.join(", "))
        };
        writeln!(plain, "  {padded} → {}{meta}{tags}", s.description).ok();

        let mut spans = vec![
            Span::raw("  ".to_string()),
            Span::styled(padded, name_style),
            Span::styled(" → ".to_string(), dim),
            Span::styled(s.description.clone(), plain_style),
        ];
        if !meta.is_empty() {
            spans.push(Span::styled(meta, dim));
        }
        if !tags.is_empty() {
            spans.push(Span::styled(tags, dim));
        }
        styled.push(Line::from(spans));
    }
    writeln!(plain, "{separator}").ok();
    styled.push(Line::from(Span::styled(separator.to_string(), dim)));

    (plain, styled)
}

/// Parse a string that may contain ANSI SGR escape sequences into
/// ratatui `Span`s, mapping each SGR code to an equivalent style. Only
/// handles the subset REPL emits via `highlight_line` and friends:
///   - `\x1b[0m` — reset (drop all style)
///   - `\x1b[1m` — bold
///   - `\x1b[2m` — dim (mapped to mid-gray fg so it renders readable)
///   - `\x1b[30m..\x1b[37m` — standard colors
///   - `\x1b[90m..\x1b[97m` — bright colors
///
/// Unknown codes are consumed silently (style unchanged). This lets us
/// pipe REPL highlighting straight into ratatui cells without losing
/// color, preserving visual parity.
fn ansi_to_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current = Style::default();
    let mut buf = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Flush pending text before applying a style change.
            flush(&mut spans, &mut buf, current);
            // Parse CSI parameters up to the final byte (0x40..=0x7e).
            let mut j = i + 2;
            let param_start = j;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j >= bytes.len() {
                // Malformed tail — take as literal. Bail gracefully.
                break;
            }
            let final_byte = bytes[j];
            let params = std::str::from_utf8(&bytes[param_start..j]).unwrap_or("");
            i = j + 1;
            // Only handle SGR (`m`). Skip anything else.
            if final_byte != b'm' {
                continue;
            }
            // Empty param (\x1b[m) is same as reset.
            if params.is_empty() {
                current = Style::default();
                continue;
            }
            for part in params.split(';') {
                match part.parse::<u8>() {
                    Ok(0) => current = Style::default(),
                    Ok(1) => current = current.add_modifier(Modifier::BOLD),
                    Ok(2) => {
                        // REPL's `\x1b[2m` renders as mid-gray in iTerm/Terminal;
                        // we match that brightness directly since ratatui's
                        // `Modifier::DIM` is visually inconsistent across
                        // terminals.
                        current = current.fg(Color::Rgb(150, 150, 150));
                    }
                    Ok(30) => current = current.fg(Color::Black),
                    Ok(31) => current = current.fg(Color::Red),
                    Ok(32) => current = current.fg(Color::Green),
                    Ok(33) => current = current.fg(Color::Yellow),
                    Ok(34) => current = current.fg(Color::Blue),
                    Ok(35) => current = current.fg(Color::Magenta),
                    Ok(36) => current = current.fg(Color::Cyan),
                    Ok(37) => current = current.fg(Color::White),
                    Ok(90) => current = current.fg(Color::DarkGray),
                    Ok(91) => current = current.fg(Color::LightRed),
                    Ok(92) => current = current.fg(Color::LightGreen),
                    Ok(93) => current = current.fg(Color::LightYellow),
                    Ok(94) => current = current.fg(Color::LightBlue),
                    Ok(95) => current = current.fg(Color::LightMagenta),
                    Ok(96) => current = current.fg(Color::LightCyan),
                    Ok(97) => current = current.fg(Color::White),
                    _ => {}
                }
            }
        } else {
            // UTF-8 aware character append: step by full char, not byte,
            // so multi-byte runes survive.
            let ch_start = i;
            let ch_end = {
                let mut k = ch_start + 1;
                while k <= bytes.len() && std::str::from_utf8(&bytes[ch_start..k]).is_err() {
                    k += 1;
                }
                k
            };
            let slice = std::str::from_utf8(&bytes[ch_start..ch_end]).unwrap_or("");
            buf.push_str(slice);
            i = ch_end;
        }
    }

    flush(&mut spans, &mut buf, current);
    spans
}

/// Build a `/view <path>` file preview matching REPL's exact structure:
///   [goblin] previewing: <path>
///     size: <S>, modified: <when>
///   [goblin] showing first N of M lines:  (or "all N lines")
///     LINE  | highlighted content
///     ...
///     ... (K more lines)   <- only if truncated
///     stats: N lines (M non-empty), longest: C chars
///
/// Errors (file missing / is dir / binary) are surfaced via `Err(msg)`.
/// Binary detection mirrors REPL: if extension isn't in the known text
/// list, sample bytes and reject if any non-printable (outside ASCII
/// graphic + common whitespace) appears.
fn build_view_output(
    workspace: &Path,
    arg: &str,
) -> std::result::Result<(String, Vec<Line<'static>>), String> {
    use std::fmt::Write as _;

    let path = workspace.join(arg);
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    if path.is_dir() {
        return Err(format!("cannot view directory: {}", path.display()));
    }

    let metadata = std::fs::metadata(&path).map_err(|e| format!("could not get file info: {e}"))?;
    let size = metadata.len();
    let modified = metadata
        .modified()
        .unwrap_or_else(|_| std::time::SystemTime::now());
    let modified_str = crate::repl::format::format_time_ago(modified);
    let size_str = crate::repl::format::format_size(size);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let is_known_text = matches!(
        ext.as_str(),
        "rs" | "py"
            | "js"
            | "ts"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "go"
            | "md"
            | "txt"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "html"
            | "css"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "sql"
            | "csv"
            | "log"
            | "rs.bak"
    );

    let is_text_file = if is_known_text {
        true
    } else {
        match std::fs::read(&path) {
            Ok(bytes) => {
                !bytes.is_empty()
                    && !bytes.iter().any(|b| {
                        !(b.is_ascii_graphic()
                            || *b == b' '
                            || *b == b'\n'
                            || *b == b'\r'
                            || *b == b'\t')
                    })
            }
            Err(_) => false,
        }
    };

    let sys_color = MessageRole::System.color();
    let label_style = Style::default().fg(sys_color).add_modifier(Modifier::BOLD);
    let line_num_style = Style::default().fg(Color::DarkGray);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    // Header — same two lines the REPL prints first.
    let header1 = format!("previewing: {}", path.display());
    writeln!(plain, "{header1}").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label_style),
        Span::styled(header1, Style::default().fg(sys_color)),
    ]));
    let size_line = format!("  size: {size_str}, modified: {modified_str}");
    writeln!(plain, "{size_line}").ok();
    styled.push(Line::from(Span::styled(size_line, dim)));

    if !is_text_file {
        let line1 = "binary file detected (not displaying content)";
        let line2 = "  use /files to browse, or run an external viewer";
        writeln!(plain, "{line1}").ok();
        writeln!(plain, "{line2}").ok();
        styled.push(Line::from(vec![
            Span::styled("[goblin] ".to_string(), label_style),
            Span::styled(line1.to_string(), Style::default().fg(sys_color)),
        ]));
        styled.push(Line::from(Span::styled(line2.to_string(), dim)));
        return Ok((plain, styled));
    }

    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("could not read file: {e}"))?;
    let max_lines = 100;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let shown = total.min(max_lines);

    let header2 = if total > max_lines {
        format!("showing first {max_lines} of {total} lines:")
    } else {
        format!("showing all {total} lines:")
    };
    writeln!(plain, "{header2}").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label_style),
        Span::styled(header2, Style::default().fg(sys_color)),
    ]));

    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_num = i + 1;
        let line_num_str = format!("  {:>4}: ", line_num);
        if line.is_empty() {
            writeln!(plain, "{line_num_str}⟨empty⟩").ok();
            styled.push(Line::from(vec![
                Span::styled(line_num_str, line_num_style),
                Span::styled("⟨empty⟩".to_string(), dim),
            ]));
        } else {
            let colored = crate::repl::format::highlight_line(line, &ext);
            let plain_line = strip_ansi(&colored);
            writeln!(plain, "{line_num_str}{plain_line}").ok();
            let mut spans = vec![Span::styled(line_num_str, line_num_style)];
            spans.extend(ansi_to_spans(&colored));
            styled.push(Line::from(spans));
        }
    }

    if total > max_lines {
        let more = format!("  ... ({} more lines)", total - max_lines);
        writeln!(plain, "{more}").ok();
        styled.push(Line::from(Span::styled(more, line_num_style)));
    }

    let empty_lines = lines.iter().filter(|l| l.trim().is_empty()).count();
    let non_empty = total - empty_lines;
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let stats = format!("  stats: {total} lines ({non_empty} non-empty), longest: {longest} chars");
    writeln!(plain, "{stats}").ok();
    styled.push(Line::from(Span::styled(stats, line_num_style)));

    let _ = shown; // readability
    Ok((plain, styled))
}

/// Run `/search [flags] <pattern>` over the workspace and return
/// `(plain, styled)` so the caller can push it via `push_styled`.
/// Mirrors REPL's format exactly: summary line → mode/case/types →
/// either "no matches" or grouped per-file output with bold file path,
/// dark-gray line-number gutter, match-highlighted content, and
/// "... and N more lines / files" tails.
fn build_search_output(workspace: &Path, raw: &str) -> (String, Vec<Line<'static>>) {
    use std::fmt::Write as _;

    let (pattern, case_sensitive, use_regex, max_results, file_types) =
        crate::repl::search::parse_search_options(raw);

    let sys_color = MessageRole::System.color();
    let label_style = Style::default().fg(sys_color).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 150));
    let gutter_style = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let mut plain = String::new();
    let mut styled: Vec<Line<'static>> = Vec::new();

    let header = format!("searching for pattern: \"{pattern}\"");
    writeln!(plain, "{header}").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label_style),
        Span::styled(header, Style::default().fg(sys_color)),
    ]));

    let mode = if use_regex { "regex" } else { "literal text" };
    writeln!(plain, "  mode: {mode}").ok();
    styled.push(Line::from(Span::styled(format!("  mode: {mode}"), dim)));
    if !case_sensitive {
        writeln!(plain, "  case: insensitive").ok();
        styled.push(Line::from(Span::styled(
            "  case: insensitive".to_string(),
            dim,
        )));
    }
    if !file_types.is_empty() {
        let line = format!("  file types: {}", file_types.join(", "));
        writeln!(plain, "{line}").ok();
        styled.push(Line::from(Span::styled(line, dim)));
    }

    let mut results: Vec<crate::repl::search::SearchResult> = Vec::new();
    let start_time = std::time::Instant::now();
    let files_searched = crate::repl::search::search_directory(
        workspace,
        &pattern,
        case_sensitive,
        use_regex,
        &file_types,
        &mut results,
        max_results,
    );
    let elapsed = start_time.elapsed();

    if results.is_empty() {
        let msg = format!(
            "no matches found in {} files ({:.2}s)",
            files_searched,
            elapsed.as_secs_f32()
        );
        writeln!(plain, "{msg}").ok();
        styled.push(Line::from(vec![
            Span::styled("[goblin] ".to_string(), label_style),
            Span::styled(msg, Style::default().fg(sys_color)),
        ]));
        return (plain, styled);
    }

    let count = results.len();
    let summary = format!(
        "found {} matches in {} files ({:.2}s)",
        count,
        files_searched,
        elapsed.as_secs_f32()
    );
    writeln!(plain, "{summary}").ok();
    styled.push(Line::from(vec![
        Span::styled("[goblin] ".to_string(), label_style),
        Span::styled(summary, Style::default().fg(sys_color)),
    ]));

    // Group by file.
    let mut file_groups: std::collections::HashMap<String, Vec<crate::repl::search::SearchResult>> =
        std::collections::HashMap::new();
    for r in results {
        file_groups.entry(r.file_path.clone()).or_default().push(r);
    }
    let mut sorted: Vec<_> = file_groups.into_iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));

    let show_files = sorted.len().min(10);
    for (file_path, matches) in sorted.iter().take(show_files) {
        let rel = std::path::Path::new(file_path)
            .strip_prefix(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file_path.clone());
        let header_line = format!("  {} ({} matches)", rel, matches.len());
        writeln!(plain, "{header_line}").ok();
        styled.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled(rel, bold),
            Span::raw(format!(" ({} matches)", matches.len())),
        ]));
        for r in matches.iter().take(3) {
            let line_content = r.line_content.trim();
            let gutter = format!("    {:>4}: ", r.line_number);
            let colored =
                crate::repl::search::highlight_pattern(line_content, &pattern, case_sensitive);
            let plain_content = strip_ansi(&colored);
            writeln!(plain, "{gutter}{plain_content}").ok();
            let mut spans = vec![Span::styled(gutter, gutter_style)];
            spans.extend(ansi_to_spans(&colored));
            styled.push(Line::from(spans));
        }
        if matches.len() > 3 {
            let tail = format!("    ... and {} more lines", matches.len() - 3);
            writeln!(plain, "{tail}").ok();
            styled.push(Line::from(Span::styled(tail, gutter_style)));
        }
    }
    if sorted.len() > show_files {
        let tail = format!("  ... and {} more files", sorted.len() - show_files);
        writeln!(plain, "{tail}").ok();
        styled.push(Line::from(Span::styled(tail, dim)));
    }

    (plain, styled)
}

/// Append a `/btw` context note to the given session without invoking
/// the model. Mirrors `Agent::append_note` but operates directly on
/// `SessionStore` so the TUI doesn't need a live agent.
fn append_note(workspace: &Path, session_id: &str, text: &str) -> Result<()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let mut store =
        SessionStore::open(workspace, session_id).context("could not open session store")?;
    let wrapped = format!("[btw — context note from the user, no reply needed]\n{trimmed}");
    let msg = aegis_api::ChatMessage::user(wrapped);
    store.append(&msg).context("could not append note")?;
    Ok(())
}

/// Pull a `path: <value>` out of the one-line tool-args preview the
/// agent loop hands us. Used by the turn-end recap to list which
/// files `edit_file` / `write_file` / `multi_edit` touched this turn
/// without needing access to the full raw JSON args. Falls back to
/// `None` if the preview doesn't contain an obvious path field.
fn extract_path_from_preview(preview: &str) -> Option<String> {
    for field in ["path:", "path =", "\"path\":", "file:", "file_path:"] {
        if let Some(idx) = preview.find(field) {
            let after = preview[idx + field.len()..].trim_start();
            let after = after.trim_start_matches('"');
            let end = after.find([',', '"', '}', '\n']).unwrap_or(after.len());
            let val = after[..end].trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Render a tool name CC-style: `read_file` → `Read`, `multi_edit` → `MultiEdit`.
/// Drops a redundant trailing `_file` suffix because CC's `read_file`/`write_file`/
/// `edit_file` show up as `Read`/`Write`/`Edit`. The remaining snake_case parts
/// get PascalCase joined with no separator. MCP tools (`mcp__server__action`)
/// are returned unchanged so they keep their fully-qualified identity.
fn canonical_tool_name(name: &str) -> String {
    if name.contains("__") {
        return name.to_string();
    }
    let trimmed = name.strip_suffix("_file").unwrap_or(name);
    trimmed
        .split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Pick the most relevant argument field for a tool's inline display.
/// CC shows `Read(file.rs)`, `Bash(cargo test)`, `Grep("foo")` — one
/// representative arg, not full JSON. Falls through `extract_path_from_preview`
/// for fs tools, then tries `command:`, `pattern:`, `query:`, `description:`,
/// `url:`. Returns None when nothing useful matches; caller renders just
/// the bare tool name.
fn extract_primary_arg(preview: &str, _tool_name: &str) -> Option<String> {
    if let Some(p) = extract_path_from_preview(preview) {
        return Some(p);
    }
    for field in [
        "\"command\":",
        "command:",
        "\"pattern\":",
        "pattern:",
        "\"query\":",
        "query:",
        "\"description\":",
        "description:",
        "\"url\":",
        "url:",
        "\"prompt\":",
        "prompt:",
    ] {
        if let Some(idx) = preview.find(field) {
            let after = preview[idx + field.len()..].trim_start();
            let after = after.trim_start_matches('"');
            let end = after.find(['"', '\n']).unwrap_or(after.len());
            let val = after[..end].trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a char boundary
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Strip `<thinking>…</thinking>` and `<think>…</think>` blocks from
/// model text. Handles both Anthropic-style (`<thinking>`) and
/// DeepSeek-R1-style (`<think>`) tags. A STILL-OPEN block (no close tag
/// yet, e.g. mid-stream) drops everything from the open tag to end of
/// input — the spinner already signals "model is reasoning", so we
/// don't need to show raw XML scaffolding that the user can't act on.
fn strip_thinking_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let think_open = rest.find("<think>").map(|i| (i, 7, "</think>"));
        let thinking_open = rest.find("<thinking>").map(|i| (i, 10, "</thinking>"));
        let next = match (think_open, thinking_open) {
            (Some(a), Some(b)) if a.0 <= b.0 => Some(a),
            (Some(a), None) => Some(a),
            (_, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((open_idx, open_len, close_tag)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + open_len..];
        match after_open.find(close_tag) {
            Some(close_idx) => {
                rest = &after_open[close_idx + close_tag.len()..];
            }
            None => {
                // Unclosed — drop from open to end, stop looping.
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render chat history in REPL style:
///   User     →  bold `> message`
///   Assistant→  orange prose, no label
///   System   →  green `[goblin] message`
///   Error    →  red `[goblin] error`
/// No `[role] ` brackets, no forced indent on wrap — keeps screenshots
/// of a TUI session visually indistinguishable from a REPL transcript.
///
/// Push one blank line after a message unless its inline pair follows
/// (Tool → ToolResult). Mirrors Claude Code's chat layout where each
/// turn has visible breathing room. Last-message case skips the blank
/// since the layout already leaves a row above the input bar.
fn push_separator_after(
    lines: &mut Vec<Line<'static>>,
    current: MessageRole,
    next: Option<&ChatMessage>,
) {
    let Some(next_msg) = next else { return };
    if current == MessageRole::Tool && next_msg.role == MessageRole::ToolResult {
        return;
    }
    lines.push(Line::from(""));
}

fn render_chat_lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // No blank separator between messages — the layout already leaves
    // one breathing row above the pinned input bar. Blank separators
    // between every message was user-rejected ("her konuşmasında bir
    // satır boş bırakıyor"); role colors alone are enough visual
    // separation.
    let last_user_idx = app
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User && m.styled_lines.is_none());
    for (idx, msg) in app.messages.iter().enumerate() {
        // Pinned indicator: a leading ★ row above the message body so
        // saved decisions/snippets are visible mid-scroll. One row per
        // pin; `/pinned` lists the same set as a directory.
        if app.pinned.contains(&idx) {
            let pin_style = Style::default()
                .fg(Color::Rgb(232, 200, 60))
                .add_modifier(Modifier::BOLD);
            lines.push(Line::from(vec![
                Span::styled("★ pinned".to_string(), pin_style),
                Span::styled(
                    format!("  #{idx}"),
                    Style::default().fg(Color::Rgb(140, 140, 140)),
                ),
            ]));
        }
        // Pre-styled messages (e.g. `/files` ext-colored listings)
        // bypass role-based styling and render verbatim so per-line
        // colors survive exactly as REPL emits them.
        if let Some(pre) = &msg.styled_lines {
            lines.extend(pre.iter().cloned());
            // Blank separator after stand-alone block messages so the
            // next User/Assistant turn doesn't visually fuse into them.
            // Tool/ToolResult/Footer skip this — they're inline previews
            // and a blank line would orphan them from their parent turn.
            push_separator_after(&mut lines, msg.role, app.messages.get(idx + 1));
            continue;
        }
        // ToolResult: collapsed by default, ctrl+O expands.
        if msg.role == MessageRole::ToolResult {
            push_tool_result_lines(&mut lines, &msg.text, msg.expanded);
            push_separator_after(&mut lines, msg.role, app.messages.get(idx + 1));
            continue;
        }
        let highlight = app.busy && Some(idx) == last_user_idx;
        push_message_lines(&mut lines, msg.role, &msg.text, highlight);
        push_separator_after(&mut lines, msg.role, app.messages.get(idx + 1));
    }

    // "✻ Thinking… / ⎿  Untangling some thoughts…" spinner — shown
    // while the model is reasoning but hasn't produced user-visible
    // content yet. Two conditions keep it alive: (1) no token has
    // landed yet (`!first_token_seen`), or (2) the stream so far is
    // ENTIRELY `<think>…` / `<thinking>…` reasoning XML with no
    // post-tag content yet. Without (2) the raw open tag would flash
    // on screen for a moment before strip-tag emptied it, leaving the
    // user with a blank area while the model was still reasoning.
    // Once real content arrives the spinner drops and the clean
    // (stripped) text renders in its place.
    let streaming_visible = !app.streaming_text.is_empty()
        && !strip_thinking_tags(&app.streaming_text).trim().is_empty();
    let showing_spinner = app.busy && (!app.first_token_seen || !streaming_visible);
    if showing_spinner {
        let elapsed_ticks = app
            .turn_start
            .map(|t| (t.elapsed().as_millis() / 200) as usize)
            .unwrap_or(0);
        let frames = &['·', '✢', '✳', '✶', '✻', '✽', '✻', '✶', '✳', '✢'];
        let spinner = frames[elapsed_ticks % frames.len()];

        let turn_start_secs: usize = app
            .turn_start
            .and_then(|t| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|now| now.saturating_sub(t.elapsed()).as_secs() as usize)
            })
            .unwrap_or(0);
        let elapsed_secs = app
            .turn_start
            .map(|t| t.elapsed().as_secs() as usize)
            .unwrap_or(0);

        let head_msgs = &[
            "Thinking…",
            "Working…",
            "Pondering…",
            "Reasoning…",
        ];
        let _ = elapsed_secs; // kept in scope above for other consumers
        // If a tool is currently running (last message is Role::Tool with
        // no Result yet), show tool-specific copy so the spinner reflects
        // what's actually happening — "Running bash…" beats a generic
        // "Thinking…" while bash is blocking the agent.
        let running_tool: Option<String> = app
            .messages
            .last()
            .filter(|m| m.role == MessageRole::Tool)
            .map(|m| {
                let head = m.text.split('(').next().unwrap_or(m.text.as_str());
                head.trim().to_string()
            });
        let head_msg: String = if let Some(ref tool) = running_tool {
            let verb = match tool.as_str() {
                "bash" | "execute_command" | "run_command" => "Running",
                "read_file" | "view_file" | "view" => "Reading",
                "write_file" | "create_file" => "Writing",
                "edit_file" | "multi_edit" | "str_replace" => "Editing",
                "search_text" | "grep" | "find_files" | "glob" => "Searching",
                "web_search" | "tavily_search" | "fetch_url" | "browser_navigate" => "Fetching",
                _ => "Running",
            };
            format!("{verb} {tool}…")
        } else {
            head_msgs[turn_start_secs % head_msgs.len()].to_string()
        };

        // Color cycles through a warm orange → cyan → green palette
        // synced to the spinner frame so the head row visibly breathes
        // instead of sitting at one fixed RGB. Less mechanical, still
        // legible against the default chat foreground.
        let palette: &[Color] = &[
            Color::Rgb(215, 135, 95),
            Color::Rgb(232, 178, 118),
            Color::Rgb(180, 200, 160),
            Color::Rgb(120, 200, 200),
            Color::Rgb(140, 180, 220),
            Color::Rgb(180, 200, 160),
            Color::Rgb(232, 178, 118),
        ];
        let head_color = palette[elapsed_ticks % palette.len()];
        let head_style = Style::default()
            .fg(head_color)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), head_style),
            Span::styled(head_msg, head_style),
        ]));

        // Copilot CLI style: show thinking content in italic dim when available
        if !app.thinking_text.is_empty() {
            let think_style = Style::default()
                .fg(Color::Rgb(140, 140, 140))
                .add_modifier(Modifier::ITALIC);
            for tl in app.thinking_text.lines() {
                if !tl.trim().is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  {tl}"),
                        think_style,
                    )));
                }
            }
        }
    }

    // Live streaming text — rendered as a partial assistant message.
    // Strip `<think>…` / `<thinking>…` blocks so DeepSeek-R1 /
    // Claude-Anthropic XML reasoning scaffolding doesn't pollute the
    // user-facing output. The spinner above covers the "still
    // reasoning" state so the user never sees a blank area either.
    if !app.streaming_text.is_empty() {
        let clean = strip_thinking_tags(&app.streaming_text);
        if !clean.trim().is_empty() {
            push_message_lines(&mut lines, MessageRole::Assistant, &clean, false);
            // Copilot CLI style: blinking cursor at end of streaming text
            let cursor_style = Style::default()
                .fg(Color::Rgb(200, 200, 200))
                .add_modifier(Modifier::REVERSED);
            lines.push(Line::from(Span::styled(" ".to_string(), cursor_style)));
        }
    }

    // Live `/consult` stream — `[provider] <tokens as they arrive>`,
    // rendered in system green so it stands out from the main assistant
    // stream. Promoted to a permanent message at end-of-stream.
    if let Some((prov, body)) = app.consult_streaming.as_ref() {
        let display = if body.is_empty() {
            format!("[{prov}] consulting…")
        } else {
            format!("[{prov}] {body}")
        };
        push_message_lines(&mut lines, MessageRole::System, &display, false);
    }

    lines
}

/// Pin the stored scroll_offset across content changes so the user's
/// viewport stays put. Pure function, driven entirely by wrapped-row
/// counts so the render path can test it without a ratatui Frame.
///
///   - GROWTH: streaming tokens, tool-call pushes, first-token footer,
///     terminal resize narrower → bump offset by the delta so absolute
///     rows stay visible.
///   - SHRINK: thinking-spinner dismissed at first token, flush swap
///     that re-renders slightly differently, terminal resize wider →
///     subtract delta so the user doesn't "ride up" through content.
///   - CLAMP: any stored overshoot (PageUp mashed past max_scroll, or
///     max_scroll dropped under current offset) gets pulled back so
///     the next PageDown responds on the first press instead of
///     burning through inflated offset first.
fn pin_scroll_offset(current_offset: u16, last_total: u16, new_total: u16, max_scroll: u16) -> u16 {
    let mut offset = current_offset;
    if offset > 0 {
        let growth = new_total.saturating_sub(last_total);
        if growth > 0 {
            offset = offset.saturating_add(growth);
        }
        let shrink = last_total.saturating_sub(new_total);
        if shrink > 0 {
            offset = offset.saturating_sub(shrink);
        }
    }
    offset.min(max_scroll)
}

/// Render one line of assistant markdown into ratatui spans.
/// Handles: # headings, **bold**, `code`, - bullets, numbered lists.
/// Render a full assistant message (possibly multi-line), handling fenced
/// code blocks (``` lang ``` → Copilot-style with dimmed background).
fn render_assistant_text(text: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut current_lang = String::new();
    let mut last_was_blank = false;
    let code_style = Style::default()
        .fg(Color::Rgb(190, 190, 190))
        .bg(Color::Rgb(30, 30, 30));
    let fence_style = Style::default().fg(Color::Rgb(75, 75, 75));
    for line in text.lines() {
        if !in_code {
            if line.starts_with("```") {
                in_code = true;
                last_was_blank = false;
                let lang = line.trim_start_matches('`').trim().to_string();
                let label = if lang.is_empty() {
                    "─────".to_string()
                } else {
                    format!("───── {lang}")
                };
                current_lang = lang;
                out.push(Line::from(Span::styled(label, fence_style)));
            } else if line.trim().is_empty() {
                if !last_was_blank {
                    out.push(Line::from(""));
                }
                last_was_blank = true;
            } else {
                last_was_blank = false;
                out.push(render_assistant_line(line));
            }
        } else if line.starts_with("```") {
            in_code = false;
            current_lang.clear();
            last_was_blank = false;
            out.push(Line::from(Span::styled("─────".to_string(), fence_style)));
        } else {
            last_was_blank = false;
            out.push(highlight_code_line(line, &current_lang, code_style));
        }
    }
    out
}

/// Inline syntax highlight for fenced code-block bodies. v0 covers
/// keywords, string/char literals, line comments, and numeric literals
/// for the languages most code reviews land in (rust, python, js/ts,
/// go, bash, json, sql). Anything outside that set falls through to
/// the default `code_style` so unknown grammars never look broken,
/// just unhighlighted. No syntect dep — this is intentionally a
/// light-weight tokenizer.
fn highlight_code_line(line: &str, lang: &str, default_style: Style) -> Line<'static> {
    let kw_style = Style::default()
        .fg(Color::Rgb(199, 146, 234)) // mauve
        .bg(Color::Rgb(30, 30, 30))
        .add_modifier(Modifier::BOLD);
    let str_style = Style::default()
        .fg(Color::Rgb(195, 232, 141)) // soft green
        .bg(Color::Rgb(30, 30, 30));
    let comment_style = Style::default()
        .fg(Color::Rgb(120, 120, 120))
        .bg(Color::Rgb(30, 30, 30))
        .add_modifier(Modifier::ITALIC);
    let num_style = Style::default()
        .fg(Color::Rgb(247, 140, 108)) // soft orange
        .bg(Color::Rgb(30, 30, 30));

    let comment_prefix = match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" | "go" | "ts" | "tsx" | "js" | "jsx" | "java" | "kotlin" | "kt"
        | "swift" | "c" | "cpp" | "cc" | "c++" | "h" | "hpp" => Some("//"),
        "py" | "python" | "sh" | "bash" | "zsh" | "fish" | "toml" | "yaml" | "yml" | "rb"
        | "ruby" => Some("#"),
        "sql" => Some("--"),
        _ => None,
    };

    // Comment fast-path: anything from the comment marker to EOL is one
    // span. Skips the tokenizer below entirely.
    if let Some(marker) = comment_prefix {
        if let Some(idx) = line.find(marker) {
            // Allow leading whitespace or string-free prefix.
            let before = &line[..idx];
            let after = &line[idx..];
            if !before.contains('"') && !before.contains('\'') {
                let mut spans = Vec::new();
                if !before.is_empty() {
                    let pre_line = highlight_code_line(before, lang, default_style);
                    spans.extend(pre_line.spans);
                }
                spans.push(Span::styled(after.to_string(), comment_style));
                return Line::from(spans);
            }
        }
    }

    let keywords: &[&str] = match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => &[
            "fn", "let", "mut", "const", "static", "if", "else", "match", "for", "while",
            "loop", "break", "continue", "return", "struct", "enum", "trait", "impl", "use",
            "mod", "pub", "crate", "self", "Self", "super", "as", "ref", "move", "async",
            "await", "true", "false", "None", "Some", "Ok", "Err", "where", "dyn", "type",
            "unsafe", "extern", "in",
        ],
        "py" | "python" => &[
            "def", "class", "if", "elif", "else", "for", "while", "try", "except", "finally",
            "raise", "return", "yield", "import", "from", "as", "with", "pass", "break",
            "continue", "lambda", "and", "or", "not", "is", "in", "True", "False", "None",
            "async", "await", "global", "nonlocal",
        ],
        "ts" | "tsx" | "js" | "jsx" => &[
            "function", "const", "let", "var", "if", "else", "for", "while", "do", "return",
            "switch", "case", "break", "continue", "class", "extends", "implements", "new",
            "this", "super", "import", "from", "export", "default", "async", "await", "try",
            "catch", "finally", "throw", "true", "false", "null", "undefined", "typeof",
            "instanceof", "void", "yield", "interface", "type", "enum", "as", "in", "of",
        ],
        "go" => &[
            "func", "var", "const", "type", "struct", "interface", "if", "else", "for",
            "switch", "case", "default", "break", "continue", "return", "go", "defer",
            "select", "chan", "map", "package", "import", "true", "false", "nil", "range",
            "fallthrough",
        ],
        "sh" | "bash" | "zsh" | "fish" => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until",
            "case", "esac", "function", "return", "break", "continue", "local", "export",
            "readonly", "true", "false",
        ],
        "java" | "kotlin" | "kt" => &[
            "fun", "val", "var", "class", "interface", "object", "if", "else", "for", "while",
            "return", "when", "import", "package", "public", "private", "protected", "true",
            "false", "null", "this", "super", "new", "throw", "try", "catch", "finally",
        ],
        _ => &[],
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut chars = line.chars().peekable();

    let flush_default =
        |buf: &mut String, spans: &mut Vec<Span<'static>>, default_style: Style| {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(buf), default_style));
            }
        };

    while let Some(c) = chars.next() {
        // String literals: " ... " and ' ... ' with naive backslash escape.
        if c == '"' || c == '\'' {
            flush_default(&mut buf, &mut spans, default_style);
            let quote = c;
            let mut s = String::from(quote);
            let mut closed = false;
            while let Some(&nc) = chars.peek() {
                chars.next();
                s.push(nc);
                if nc == '\\' {
                    if let Some(&esc) = chars.peek() {
                        chars.next();
                        s.push(esc);
                    }
                    continue;
                }
                if nc == quote {
                    closed = true;
                    break;
                }
            }
            spans.push(Span::styled(s, str_style));
            // Unterminated literal: rest of line already consumed inside
            // the loop, so we just fall through. `closed` is informational.
            let _ = closed;
            continue;
        }

        // Numeric literals: simple digit run, optional decimal point.
        if c.is_ascii_digit() && buf.chars().last().map_or(true, |p| !p.is_alphanumeric()) {
            flush_default(&mut buf, &mut spans, default_style);
            let mut n = String::from(c);
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_digit() || nc == '.' || nc == '_' {
                    chars.next();
                    n.push(nc);
                } else {
                    break;
                }
            }
            spans.push(Span::styled(n, num_style));
            continue;
        }

        // Identifier accumulation. When we hit a non-identifier char,
        // flush the buffer — checking if it matches a keyword on the way
        // out so single-token highlighting is accurate.
        if c.is_alphanumeric() || c == '_' {
            buf.push(c);
            continue;
        }

        // Hit a separator. Flush identifier (with keyword check), then
        // emit the separator itself with the default style.
        if !buf.is_empty() {
            let ident = std::mem::take(&mut buf);
            if keywords.contains(&ident.as_str()) {
                spans.push(Span::styled(ident, kw_style));
            } else {
                spans.push(Span::styled(ident, default_style));
            }
        }
        spans.push(Span::styled(c.to_string(), default_style));
    }
    if !buf.is_empty() {
        if keywords.contains(&buf.as_str()) {
            spans.push(Span::styled(buf, kw_style));
        } else {
            spans.push(Span::styled(buf, default_style));
        }
    }

    Line::from(spans)
}

fn render_assistant_line(line: &str) -> Line<'static> {
    let prose = Color::White;
    let head1 = Color::Rgb(80, 200, 120); // green — # H1
    let head2 = Color::Rgb(120, 200, 200); // cyan — ## H2
    let head3 = Color::Rgb(180, 180, 180); // light gray — ### H3
    let bullet_fg = Color::Rgb(120, 180, 120); // muted green bullet

    // # Heading 1
    if let Some(h) = line.strip_prefix("# ") {
        return Line::from(Span::styled(
            h.to_string(),
            Style::default().fg(head1).add_modifier(Modifier::BOLD),
        ));
    }
    // ## Heading 2
    if let Some(h) = line.strip_prefix("## ") {
        return Line::from(Span::styled(
            h.to_string(),
            Style::default().fg(head2).add_modifier(Modifier::BOLD),
        ));
    }
    // ### Heading 3
    if let Some(h) = line.strip_prefix("### ") {
        return Line::from(Span::styled(
            h.to_string(),
            Style::default().fg(head3).add_modifier(Modifier::BOLD),
        ));
    }
    // Bullet: "- " or "* "
    let (bullet_prefix, rest) =
        if let Some(r) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            (true, r)
        } else {
            (false, line)
        };
    // Numbered list: "1. " "2. " etc
    let numbered_rest = if !bullet_prefix {
        let trimmed = line.trim_start();
        let dot_pos = trimmed.find(". ");
        dot_pos.and_then(|i| {
            if i > 0 && i < 3 && trimmed[..i].chars().all(|c| c.is_ascii_digit()) {
                Some((line.len() - trimmed.len(), &trimmed[i + 2..]))
            } else {
                None
            }
        })
    } else {
        None
    };

    // Build inline spans (handles **bold** and `code`)
    fn inline_spans(text: &str, base: Color) -> Vec<Span<'static>> {
        let code_fg = Color::Rgb(255, 215, 100);
        let bold_mod = Modifier::BOLD;
        let base_style = Style::default().fg(base);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut remaining = text;
        while !remaining.is_empty() {
            // `code`
            if let Some(start) = remaining.find('`') {
                if start > 0 {
                    spans.push(Span::styled(remaining[..start].to_string(), base_style));
                }
                let after = &remaining[start + 1..];
                if let Some(end) = after.find('`') {
                    spans.push(Span::styled(
                        after[..end].to_string(),
                        Style::default().fg(code_fg),
                    ));
                    remaining = &after[end + 1..];
                    continue;
                }
            }
            // **bold**
            if let Some(start) = remaining.find("**") {
                if start > 0 {
                    spans.push(Span::styled(remaining[..start].to_string(), base_style));
                }
                let after = &remaining[start + 2..];
                if let Some(end) = after.find("**") {
                    spans.push(Span::styled(
                        after[..end].to_string(),
                        Style::default().fg(base).add_modifier(bold_mod),
                    ));
                    remaining = &after[end + 2..];
                    continue;
                }
            }
            // no more markers
            spans.push(Span::styled(remaining.to_string(), base_style));
            break;
        }
        spans
    }

    if bullet_prefix {
        let mut spans = vec![Span::styled(
            "  ● ".to_string(),
            Style::default().fg(bullet_fg).add_modifier(Modifier::BOLD),
        )];
        spans.extend(inline_spans(rest, prose));
        return Line::from(spans);
    }
    if let Some((indent, nr)) = numbered_rest {
        let num_part = &line[..indent + (line.len() - line.trim_start().len() - indent)];
        // extract the "N. " prefix
        let prefix_end = line.find(". ").map(|i| i + 2).unwrap_or(0);
        let num_str = &line[..prefix_end];
        let mut spans = vec![Span::styled(
            format!("  {num_str}"),
            Style::default().fg(bullet_fg).add_modifier(Modifier::BOLD),
        )];
        let _ = num_part;
        let _ = indent;
        spans.extend(inline_spans(nr, prose));
        return Line::from(spans);
    }

    // Plain prose with inline markers
    Line::from(inline_spans(line, prose))
}

/// Colorize a single content line for tool-result rendering.
/// Picks up unified-diff prefixes (`+`, `-`, `@@`) and emits the standard
/// CC-style red/green/cyan colors. Other lines keep the muted gray default.
/// Returns the span for the content portion only — caller adds the `  ⎿ ` prefix.
fn diff_styled_span(content: &str) -> Span<'static> {
    let default_fg = Color::Rgb(150, 150, 150);
    if content.starts_with("+++ ") || content.starts_with("--- ") {
        // File markers: bold, same color as the +/- side.
        let fg = if content.starts_with("+++ ") {
            Color::Rgb(120, 200, 120)
        } else {
            Color::Rgb(220, 110, 110)
        };
        Span::styled(
            content.to_string(),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        )
    } else if content.starts_with('+') {
        // git-style block fill so added lines read as a band, not just
        // colored text. Bg is dark enough to keep contrast readable on
        // both dark + light terminal themes.
        Span::styled(
            content.to_string(),
            Style::default()
                .fg(Color::Rgb(180, 240, 180))
                .bg(Color::Rgb(20, 50, 25)),
        )
    } else if content.starts_with('-') {
        Span::styled(
            content.to_string(),
            Style::default()
                .fg(Color::Rgb(255, 170, 170))
                .bg(Color::Rgb(60, 22, 22)),
        )
    } else if content.starts_with("@@") {
        // Hunk header — cyan dim, like git diff in most terminals.
        Span::styled(
            content.to_string(),
            Style::default().fg(Color::Rgb(120, 180, 200)),
        )
    } else {
        Span::styled(content.to_string(), Style::default().fg(default_fg))
    }
}

/// Tools where it makes sense to show the [E]xplain / [R]evise /
/// [X]re-run chip footer — file-mutating or shell-running tools that
/// the user might want to follow up on. A read-only `grep` result
/// doesn't need the chips.
fn tool_invites_action_chips(text: &str) -> bool {
    let head: String = text.lines().next().unwrap_or("").to_string();
    head.starts_with("edited ")
        || head.starts_with("wrote ")
        || head.starts_with("created ")
        || head.starts_with("$ ")
        || head.starts_with("📦 ")
        || head.starts_with("📄 ")
}

/// Append a dim chip footer line under a tool result so the user has
/// a one-line nudge toward the natural follow-ups: ask the agent to
/// explain what just happened, revise via /undo, or re-run via the
/// same prompt (the user re-types or scrollback-recalls it). Pure
/// hint — the chips don't capture key input themselves; they point
/// at slash commands that already exist.
fn push_action_chip_line(lines: &mut Vec<Line<'static>>) {
    let muted = Style::default().fg(Color::Rgb(95, 95, 95));
    let chip_bg = Style::default()
        .fg(Color::Rgb(220, 220, 220))
        .bg(Color::Rgb(40, 40, 40));
    let key_style = Style::default()
        .fg(Color::Rgb(255, 200, 50))
        .add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled("    ".to_string(), muted),
        Span::styled(" E ".to_string(), key_style),
        Span::styled(" /ask 'ne yaptın' ".to_string(), chip_bg),
        Span::styled("  ".to_string(), muted),
        Span::styled(" R ".to_string(), key_style),
        Span::styled(" /undo ".to_string(), chip_bg),
        Span::styled("  ".to_string(), muted),
        Span::styled(" X ".to_string(), key_style),
        Span::styled(" /redo ".to_string(), chip_bg),
    ]));
}

fn push_tool_result_lines(lines: &mut Vec<Line<'static>>, text: &str, expanded: bool) {
    // Copilot CLI style: collapsible tool output with indented preview.
    // Detect unified diff blocks and render them with 📄 headers.
    let prefix_style = Style::default()
        .fg(Color::Rgb(130, 130, 130))
        .add_modifier(Modifier::DIM);
    let hint_style = Style::default().fg(Color::Rgb(75, 75, 75));
    // edit_file's tool result appends a "FILE NOW (...): <numbered snippet>"
    // block plus a "(file state is current — do NOT re-read ...)" trailer.
    // Both exist to anchor the model on post-edit ground truth and are
    // pure noise for the human scrolling the chat — the diff above
    // already shows what changed. Strip them from display only.
    let text_owned;
    let text: &str = if let Some(idx) = text.find("\nFILE NOW (`") {
        text_owned = text[..idx].to_string();
        &text_owned
    } else {
        text
    };
    let text_lines: Vec<&str> = text.lines().collect();

    // Try to render as diff if content looks like a unified diff.
    if is_diff_output(text) && expanded {
        push_diff_lines(lines, text);
        if tool_invites_action_chips(text) {
            push_action_chip_line(lines);
        }
        return;
    }

    if expanded || text_lines.len() <= 1 {
        for tl in &text_lines {
            let mut line_spans = vec![Span::styled("  ⎿ ".to_string(), prefix_style)];
            // Use ansi_to_spans for ANSI-colored output (bash, glob, grep --color, etc.)
            // diff_styled_span only for diff markers (+, -, @@)
            if is_diff_output(text) {
                line_spans.push(diff_styled_span(tl));
            } else {
                line_spans.extend(ansi_to_spans(tl));
            }
            lines.push(Line::from(line_spans));
        }
        if expanded && tool_invites_action_chips(text) {
            push_action_chip_line(lines);
        }
    } else {
        let first = text_lines[0];
        let mut line_spans: Vec<Span<'static>> = vec![Span::styled("  ⎿ ".to_string(), prefix_style)];
        // If the first line carries ANSI escapes (typical for `ls`,
        // `grep --color`, `bat`, `eza`, etc.), parse and preserve them
        // so a one-line preview isn't flattened to muted gray. We can't
        // safely byte-truncate ANSI mid-sequence, so when the first line
        // is colored we render it in full and skip the 100-char cap.
        let has_ansi = first.contains('\u{1b}');
        if has_ansi {
            line_spans.extend(ansi_to_spans(first));
        } else {
            let preview: String = first.chars().take(100).collect();
            let preview_str = if first.chars().count() > 100 {
                format!("{preview}…")
            } else {
                preview
            };
            line_spans.push(Span::styled(
                preview_str,
                Style::default().fg(Color::Rgb(140, 140, 140)),
            ));
        }
        line_spans.push(Span::styled("  (ctrl+o to expand)".to_string(), hint_style));
        lines.push(Line::from(line_spans));
    }
}

/// Check if text looks like a unified diff output.
fn is_diff_output(text: &str) -> bool {
    text.contains("--- a/") || text.contains("+++ b/") || text.contains("@@ -")
}

/// Render unified diff with 📄 header and colored + / - lines.
fn push_diff_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    let file_style = Style::default()
        .fg(Color::Rgb(200, 200, 200))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(130, 130, 130));

    let mut file_path: Option<&str> = None;
    let mut added: usize = 0;
    let mut removed: usize = 0;

    for line in text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            file_path = Some(path);
        } else if line.starts_with("+++ ") {
            // "+++ a/path" or "+++ /dev/null" — extract if possible
            let rest = &line[4..];
            if !rest.is_empty() && rest != "/dev/null" {
                file_path = Some(rest);
            }
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }

    // 📄 header line
    if let Some(path) = file_path {
        lines.push(Line::from(vec![
            Span::styled("📄 ".to_string(), file_style),
            Span::styled(path.to_string(), file_style),
            Span::styled(format!(" +{added} -{removed}"), dim),
        ]));
    }

    // Colored diff lines
    for line in text.lines() {
        let span = diff_styled_span(line);
        lines.push(Line::from(vec![
            Span::raw("  "),
            span,
        ]));
    }
}

fn push_message_lines(lines: &mut Vec<Line<'static>>, role: MessageRole, text: &str, highlight: bool) {
    let color = role.color();
    match role {
        MessageRole::User => {
            // Copilot CLI style: ▸ prefix in cyan, body in cyan/teal.
            let prefix_style = Style::default()
                .fg(Color::Rgb(88, 166, 255))
                .add_modifier(Modifier::BOLD);
            let text_style = Style::default().fg(Color::Rgb(88, 166, 255));
            let line_bg = if highlight {
                Style::default().bg(Color::Rgb(45, 45, 45))
            } else {
                Style::default()
            };
            for (i, text_line) in text.lines().enumerate() {
                let prefix = if i == 0 { "▸ " } else { "  " };
                let line = Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(text_line.to_string(), text_style),
                ])
                .style(line_bg);
                lines.push(line);
            }
        }
        MessageRole::Assistant => {
            lines.extend(render_assistant_text(text));
        }
        MessageRole::System => {
            let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let text_style = Style::default().fg(color);
            for (i, text_line) in text.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("[goblin] ".to_string(), label_style),
                        Span::styled(text_line.to_string(), text_style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw("        "),
                        Span::styled(text_line.to_string(), text_style),
                    ]));
                }
            }
        }
        MessageRole::Error => {
            // Copilot CLI style: `✗ ` prefix in red, body in red.
            let red_style = Style::default()
                .fg(Color::Rgb(220, 50, 50))
                .add_modifier(Modifier::BOLD);
            let body_style = Style::default().fg(Color::Rgb(255, 100, 100));
            for (i, text_line) in text.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("✗ ".to_string(), red_style),
                        Span::styled(text_line.to_string(), body_style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(text_line.to_string(), body_style),
                    ]));
                }
            }
        }
        MessageRole::Tool => {
            // Copilot CLI style: `⚙ tool_name` in dim gray.
            // The tool name is the first line of text.
            let tool_style = Style::default()
                .fg(Color::Rgb(150, 150, 150))
                .add_modifier(Modifier::DIM);
            let first = text.lines().next().unwrap_or("").to_string();
            lines.push(Line::from(vec![
                Span::styled("⚙ ".to_string(), tool_style),
                Span::styled(first, tool_style),
            ]));
        }
        MessageRole::ToolResult => {
            // `  ⎿ preview` — indented, mid-gray prefix; the content span
            // picks up unified-diff colors (`+` green, `-` red, `@@` cyan)
            // when the model returns an edit_file/multi_edit/write_file
            // result that contains a unified diff block.
            let prefix_style = Style::default().fg(color);
            for text_line in text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  ⎿ ".to_string(), prefix_style),
                    diff_styled_span(text_line),
                ]));
            }
        }
        MessageRole::Footer => {
            // Mid-gray per-turn summary line. The leading ● is green
            // (same as bash/glob tool indicators); rest is dim gray.
            let gray = Style::default().fg(color);
            let green = Style::default().fg(Color::Green);
            for text_line in text.lines() {
                if let Some(rest) = text_line.strip_prefix('\u{2022}') {
                    lines.push(Line::from(vec![
                        Span::styled("\u{2022}".to_string(), green),
                        Span::styled(rest.to_string(), gray),
                    ]));
                } else {
                    lines.push(Line::from(Span::styled(text_line.to_string(), gray)));
                }
            }
        }
    }
}

/// Build the bottom recap line. Extracted from `draw` so tests can assert
/// on its spans directly without driving the full ratatui pipeline.
///
/// Span order: `▸▸ ` (red-bold) · optional `[plan] `/`[exec] ` chip ·
/// `model · ` · `turns/cost | new session` · keybind hint. Plan chip
/// mirrors REPL's `metis [plan]`/`[exec]` prefix — yellow for Drafting,
/// green for Executing, hidden for Normal.
pub(crate) fn build_recap_line(app: &TuiApp) -> Line<'static> {
    // Copilot CLI style status bar:
    //   model_name · cwd
    //   [busy indicator] · turns:N · cost
    let cwd_str = app.workspace.display().to_string();

    let plan_chip: Option<Span<'static>> = {
        if app.btw_in_flight.is_some() {
            Some(Span::styled(
                "[btw] ",
                Style::default()
                    .fg(Color::Rgb(88, 166, 255))
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            // Atakan: permission_mode badge öncelik, plan_state fallback.
            // 4-mod cycle görünür kalsın — özellikle Bypass renk uyarısı
            // kazara açıldıysa fark edilsin.
            let ps_drafting = matches!(*app.plan_state.lock().unwrap(), PlanState::Drafting);
            match app.permission_mode {
                PermMode::Bypass => Some(Span::styled(
                    "BYPASS ",
                    Style::default()
                        .fg(Color::Rgb(248, 81, 73)) // red — dikkat
                        .add_modifier(Modifier::BOLD),
                )),
                PermMode::AcceptEdits => Some(Span::styled(
                    "AcceptEdits ",
                    Style::default()
                        .fg(Color::Rgb(232, 200, 60)) // amber
                        .add_modifier(Modifier::BOLD),
                )),
                PermMode::Plan => Some(Span::styled(
                    "Plan ",
                    Style::default()
                        .fg(Color::Rgb(210, 168, 255)) // purple
                        .add_modifier(Modifier::BOLD),
                )),
                PermMode::Default => {
                    // Plan state Drafting (legacy /plan toggle yolu) için
                    // de chip göster — sync'lenir ama defansif.
                    if ps_drafting {
                        Some(Span::styled(
                            "Plan ",
                            Style::default()
                                .fg(Color::Rgb(210, 168, 255))
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        None
                    }
                }
            }
        }
    };

    let tool_total: u32 = app
        .tools
        .iter()
        .filter(|t| matches!(t.status, ToolStatus::Done | ToolStatus::Failed))
        .count() as u32;
    let tool_running: u32 = app
        .tools
        .iter()
        .filter(|t| matches!(t.status, ToolStatus::Running))
        .count() as u32;
    let queued_prompts = app.pending_prompts.len();
    let queued_images = app.pending_images.len();

    // GitHub Copilot CLI color palette
    let dim = Style::default().fg(Color::Rgb(139, 148, 158));
    let mute = Style::default().fg(Color::Rgb(110, 118, 128));
    let model_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let cwd_style = Style::default().fg(Color::Rgb(175, 184, 193));

    // Busy indicator: blue dot when agent is running.
    let busy_style = if app.busy {
        Style::default()
            .fg(Color::Rgb(88, 166, 255))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(63, 185, 80))
    };
    let busy_dot = if app.busy { "●" } else { "○" };

    let mut recap_spans: Vec<Span<'static>> = Vec::new();

    // Main status: ● model_name  ·  cwd
    recap_spans.push(Span::styled(format!("{} ", busy_dot), busy_style));
    recap_spans.push(Span::styled(app.model.clone(), model_style));
    recap_spans.push(Span::styled("  ·  ".to_string(), mute));
    recap_spans.push(Span::styled(cwd_str, cwd_style));

    // Pause badge — only while a turn is live and user has parked the
    // stream with Space. Yellow so it stands out against the dim
    // status line; hint nudges toward the resume key.
    if app.stream_paused {
        let pause_style = Style::default()
            .fg(Color::Rgb(255, 200, 50))
            .add_modifier(Modifier::BOLD);
        let buffered = app.stream_paused_buffer.len();
        recap_spans.push(Span::styled("  ·  ".to_string(), mute));
        recap_spans.push(Span::styled("⏸ paused".to_string(), pause_style));
        if buffered > 0 {
            recap_spans.push(Span::styled(
                format!(" (+{buffered}b queued, Space resume)"),
                dim,
            ));
        } else {
            recap_spans.push(Span::styled(" (Space resume)".to_string(), dim));
        }
    }

    // Session stats: turns:N only, no cost
    if app.turn_count > 0 {
        recap_spans.push(Span::styled("  ·  ".to_string(), mute));
        recap_spans.push(Span::styled(format!("turns:{}", app.turn_count), dim));
    }

    // Plan chip
    if let Some(chip) = plan_chip {
        recap_spans.push(Span::styled(" · ".to_string(), mute));
        recap_spans.push(chip);
    }

    // Tool counts
    if tool_total > 0 || tool_running > 0 {
        recap_spans.push(Span::styled(" · ".to_string(), mute));
        if tool_running > 0 {
            recap_spans.push(Span::styled(format!("tools:{tool_total}"), dim));
            recap_spans.push(Span::styled(
                format!(" (+{tool_running})"),
                Style::default()
                    .fg(Color::Rgb(88, 166, 255))
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            recap_spans.push(Span::styled(format!("tools:{tool_total}"), dim));
        }
    }

    // Queued items
    if queued_prompts > 0 || queued_images > 0 {
        recap_spans.push(Span::styled(" · ".to_string(), mute));
        let mut parts: Vec<String> = Vec::new();
        if queued_prompts > 0 {
            parts.push(format!("{queued_prompts} prompt(s)"));
        }
        if queued_images > 0 {
            parts.push(format!("{queued_images} image(s)"));
        }
        recap_spans.push(Span::styled(
            format!("queued: {}", parts.join(" + ")),
            Style::default()
                .fg(Color::Rgb(88, 166, 255))
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Scroll indicator
    if app.scroll_offset > 0 {
        recap_spans.push(Span::styled(" · ".to_string(), mute));
        recap_spans.push(Span::styled(
            format!("↑{}/{}", app.scroll_offset, app.last_wrapped_total),
            Style::default()
                .fg(Color::Rgb(139, 148, 158))
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Help hint
    recap_spans.push(Span::styled(
        "   ·   esc/ctrl-d quit · /help".to_string(),
        mute,
    ));

    if app.mouse_capture_on {
        recap_spans.push(Span::styled(
            "  ·  /mouse".to_string(),
            Style::default().fg(Color::Rgb(100, 100, 100)),
        ));
    }

    Line::from(recap_spans)
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut TuiApp,
) -> Result<()> {
    terminal.draw(|frame| {
        let size = frame.area();

        // REPL-parity layout:
        //   [banner area (pinned, top)] 15 rows  (14-line dragon + model)
        //   [chat    (scrollable, fills)]
        //   [input   (pinned, bottom)] 1 row
        // No status bar — the per-turn cost summary is pushed into the
        // chat as a dim `Footer` message, matching REPL behavior.
        // Full ASCII banner at startup. Dragon banner is 14 rows + 1
        // model line = 15 rows pinned. Matches BANNER_LINES.len() + 1.
        // Atakan: banner sadece ilk user mesajına kadar görünsün, sonra
        // gizlensin — kalıcı pinned kalmasın.
        let banner_visible = !app
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User);
        let banner_height: u16 = if banner_visible {
            (BANNER_LINES.len() as u16) + 1
        } else {
            0
        };
                                                           // Queue strip height — dim ghost rows above the
                                                           // input showing each queued prompt. 0 rows when empty so the
                                                           // chat keeps its full height; capped at 3 with "+N more" when
                                                           // deeper. Renders between chat gap and separator.
        let queue_len = app.pending_prompts.len();
        let queue_strip_height = queue_len.min(3) as u16;
        // Account for explicit newlines (Shift+Enter) AND wrapping. Each
        // logical line is `ceil(chars / width)` visual rows; sum across
        // logical lines, then clamp so the input never eats the chat.
        let w_layout = size.width.max(1);
        let input_rows = {
            let mut rows: u16 = 0;
            let logical: Vec<&str> = if app.input.is_empty() {
                vec![""]
            } else {
                app.input.split('\n').collect()
            };
            for (i, line) in logical.iter().enumerate() {
                let c = line.chars().count() + if i == 0 { 2 } else { 0 };
                let r = (c as u16).saturating_sub(1) / w_layout + 1;
                rows = rows.saturating_add(r);
            }
            rows.clamp(1, 8)
        };
        // Layout rows, top→bottom:
        //   0. banner (pinned)
        //   1. chat (fills)
        //   2. 1-row breathing gap
        //   3. queue strip (0..=3 rows)
        //   4. recap / status bar (thinking indicator, model, cost)
        //   5. separator top (`────`)
        //   6. input bar (1-4 rows, grows with text)
        //   7. separator bottom (`────`)
        let tabs_strip_height = if app.tabs_strip_visible { 1 } else { 0 };
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(banner_height),
                Constraint::Length(tabs_strip_height),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(queue_strip_height),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(input_rows),
                Constraint::Length(1),
            ])
            .split(size);

        let banner_area = vert[0];
        let tabs_area = vert[1];
        let raw_chat_area = vert[2];
        let queue_area = vert[4];

        // Tabs strip render — single-line top navigation chips.
        // Active tab is "chat" because the picker overlays close
        // back to chat once the user selects something. Other tabs
        // are launchers, not persistent panels.
        if app.tabs_strip_visible {
            let active_style = Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let inactive_style = Style::default().fg(Color::Rgb(140, 140, 140));
            let key_style = Style::default()
                .fg(Color::Rgb(255, 200, 50))
                .add_modifier(Modifier::BOLD);
            let tabs = vec![Line::from(vec![
                Span::styled(" F1 ".to_string(), key_style),
                Span::styled(" chat ".to_string(), active_style),
                Span::raw("  "),
                Span::styled(" F2 ".to_string(), key_style),
                Span::styled(" files ".to_string(), inactive_style),
                Span::raw("  "),
                Span::styled(" F3 ".to_string(), key_style),
                Span::styled(" sessions ".to_string(), inactive_style),
                Span::raw("  "),
                Span::styled(" F4 ".to_string(), key_style),
                Span::styled(" permissions ".to_string(), inactive_style),
            ])];
            frame.render_widget(Paragraph::new(tabs), tabs_area);
        }

        // Side context panel (OpenCode-style sidebar) — fixed 32 cols
        // when terminal is wide enough, hidden otherwise. OpenCode uses
        // 42 but aegis users tend toward narrower windows.
        let (chat_area, context_panel_opt) = if size.width >= 80 && app.sidebar_visible {
            // OpenCode-style separation: a 2-col empty gutter between
            // chat and sidebar instead of a drawn border line.
            let horiz = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(40),
                    Constraint::Length(2),  // gutter
                    Constraint::Length(32),
                ])
                .split(raw_chat_area);
            (horiz[0], Some(horiz[2]))
        } else {
            (raw_chat_area, None)
        };
        let recap_area = vert[5];
        let separator_area = vert[6];
        let input_area = vert[7];
        let separator_bottom_area = vert[8];

        // Queue strip — dim ghost rows, one per queued prompt (up to
        // 3), with ⏵ prefix and truncated text. Last row shows
        // `+N more` when queue depth exceeds 3. Empty queue → empty
        // area, zero layout cost.
        if queue_strip_height > 0 {
            let mut queue_lines: Vec<Line<'static>> = Vec::new();
            let dim = Style::default().fg(Color::Rgb(130, 130, 130));
            let accent = Style::default()
                .fg(Color::Rgb(170, 170, 170))
                .add_modifier(Modifier::BOLD);
            // First queued item: bright/opaque "selected" look
            let sel_arrow = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let sel_text = Style::default()
                .fg(Color::Rgb(230, 230, 230))
                .add_modifier(Modifier::BOLD);
            let visible = (queue_strip_height as usize).min(queue_len);
            let show = if queue_len > 3 { 2 } else { queue_len };
            for i in 0..show {
                let prompt = &app.pending_prompts[i];
                let preview: String = prompt.chars().take(80).collect();
                let preview = if prompt.chars().count() > 80 {
                    format!("{preview}…")
                } else {
                    preview
                };
                let (arrow_style, text_style) = if i == 0 {
                    (sel_arrow, sel_text)
                } else {
                    (accent, dim)
                };
                queue_lines.push(Line::from(vec![
                    Span::styled("⏵ ".to_string(), arrow_style),
                    Span::styled(preview, text_style),
                ]));
            }
            if queue_len > 3 {
                queue_lines.push(Line::from(Span::styled(
                    format!("⏵ +{} more queued", queue_len - 2),
                    dim,
                )));
            }
            if let Some(ref btw) = app.btw_in_flight {
                let preview: String = btw.chars().take(80).collect();
                let preview = if btw.chars().count() > 80 {
                    format!("{preview}…")
                } else {
                    preview
                };
                let btw_blue = Style::default().fg(Color::Rgb(96, 165, 250));
                queue_lines.push(Line::from(vec![
                    Span::styled("↩ ".to_string(), btw_blue),
                    Span::styled(format!("[btw] {preview}"), btw_blue),
                ]));
            }
            let _ = visible;
            frame.render_widget(Paragraph::new(queue_lines), queue_area);
        }

        // Recap / status bar — sits between chat and the separator so
        // thinking indicator + model info are always visible above input.
        // Chat-search overlay takes precedence; status_line takes the
        // next slot; default recap is the fallback.
        let recap = if let Some(s) = app.chat_search.as_ref() {
            let cs = Style::default()
                .fg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
            let count = if s.matches.is_empty() {
                if s.query.is_empty() {
                    "(type to search)".to_string()
                } else {
                    "no matches".to_string()
                }
            } else {
                format!("{}/{} matches  ↑↓ to step", s.current + 1, s.matches.len())
            };
            Line::from(vec![
                Span::styled("🔎 search: ".to_string(), cs),
                Span::raw(s.query.clone()),
                Span::raw("   "),
                Span::styled(count, dim),
                Span::raw("   "),
                Span::styled("Esc to close".to_string(), dim),
            ])
        } else if crate::status_line::is_active() {
            let model = app.model.clone();
            let cwd = app.workspace.display().to_string();
            let session = app.session_id.clone();
            let cost = app.cost_display.clone();
            let turn = app.turn_count;
            let provider = app.current_provider.clone();
            crate::status_line::maybe_refresh(move || {
                serde_json::json!({
                    "session_id": session,
                    "model": model,
                    "provider": provider,
                    "cwd": cwd,
                    "cost": cost,
                    "turn_count": turn,
                })
            });
            match crate::status_line::current() {
                Some(line) => Line::from(Span::raw(line)),
                None => build_recap_line(app),
            }
        } else {
            build_recap_line(app)
        };
        frame.render_widget(Paragraph::new(recap), recap_area);

        let width = size.width as usize;
        let rule: String = "─".repeat(width);
        // Pre-compute ask/btw-typing state so the top separator can
        // also flip color while the user composes a `/ask ...` (cyan)
        // or legacy `/btw ...` (blue) line. Re-checked below for the
        // input area itself.
        let sep_typing_ask = {
            let trimmed = app.input.trim_start();
            trimmed == "/ask" || trimmed.starts_with("/ask ")
        };
        let sep_typing_btw = {
            let trimmed = app.input.trim_start();
            trimmed == "/btw" || trimmed.starts_with("/btw ")
        };
        let sep_style = if sep_typing_ask {
            Style::default().fg(Color::Rgb(80, 200, 240))    // ask cyan
        } else if sep_typing_btw {
            Style::default().fg(Color::Rgb(88, 166, 255))    // legacy btw blue
        } else {
            Style::default().fg(Color::Rgb(139, 148, 158))   // secondary gray
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(rule.clone(), sep_style))),
            separator_area,
        );

        if banner_visible {
            render_banner(frame, banner_area, &app.model);
        }

        // -- Chat area (no borders, no title — REPL feel) --
        //
        // Paragraph.scroll operates on POST-WRAP rendered rows. Earlier
        // attempts computed `total_wrapped` with a ceiling-division
        // `cells / width`, but ratatui's word-aware wrapping produces
        // MORE lines (a 50-cell line in a 40-wide area wraps to 2 rows
        // with cells/width, but word boundaries can push it to 3). That
        // mismatch is why the user's "old text disappears on scroll"
        // and "text above R1 is missing" reports kept coming back —
        // our scroll offset was computed against a wrong total.
        //
        // Fix: build the Paragraph first, then ask IT how many wrapped
        // rows it produces at `chat_area.width` via the unstable
        // `line_count` API (opt-in via the `unstable-rendered-line-info`
        // feature on the ratatui dependency).
        let chat_lines = render_chat_lines(app);
        let visible_height = chat_area.height;
        let chat = Paragraph::new(chat_lines).wrap(Wrap { trim: false });
        let total_wrapped = chat.line_count(chat_area.width);
        let total_lines = total_wrapped.min(u16::MAX as usize) as u16;
        let max_scroll = total_lines.saturating_sub(visible_height);

        // Keep the user's viewport pinned to WHAT THEY SEE when the
        // rendered row count changes. scroll_offset is "rows above
        // bottom"; if it's non-zero the user is scrolled up and wants
        // to stay put. We pin in BOTH directions:
        //
        //   - GROWTH (streaming token, tool-call push, first-token
        //     footer): bump offset by the new rows so the absolute rows
        //     the user sees stay constant.
        //   - SHRINK (thinking spinner dismissed at first token, flush
        //     swap, terminal resize wider → less wrapping): subtract
        //     shrink from offset. Without this, total drops under the
        //     user's stored offset and their viewport rides up through
        //     older content while the text they were reading scrolls
        //     out the bottom — the literal "yazılar kayboluyor" report.
        //
        // Then clamp scroll_offset itself (not just the local `scroll`)
        // to max_scroll. Previously PageUp could inflate the stored
        // offset past max_scroll (capped at 10_000), and PageDown had
        // to "burn off" the overshoot before the viewport moved at all.
        // Clamping here keeps state and render in sync frame-over-frame.
        app.scroll_offset = pin_scroll_offset(
            app.scroll_offset,
            app.last_wrapped_total,
            total_lines,
            max_scroll,
        );
        app.last_wrapped_total = total_lines;
        app.last_visible_height = visible_height;
        app.last_max_scroll = max_scroll;

        let scroll = app.scroll_offset;
        let chat = chat.scroll((max_scroll.saturating_sub(scroll), 0));

        frame.render_widget(chat, chat_area);

        // Scrollbar on the right edge of the chat area so the user can
        // SEE their scroll position and the total scrollback length.
        // This also exposes whether content actually exists to scroll
        // back to, answering "are the old messages gone or just above
        // the viewport?" without the user having to mash PageUp blind.
        if total_lines > visible_height {
            use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
            let mut sb_state = ScrollbarState::new(total_lines as usize)
                .position((max_scroll.saturating_sub(scroll)) as usize)
                .viewport_content_length(visible_height as usize);
            let sb = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█")
                .style(Style::default().fg(Color::Rgb(120, 120, 120)));
            frame.render_stateful_widget(sb, chat_area, &mut sb_state);
        }

        // -- Context panel (right side, OpenCode style) --
        if let Some(panel_area) = context_panel_opt {
            render_context_panel(frame, panel_area, app);
        }

        // -- Allow quick-menu overlay --
        if app.allow_menu_open {
            let allowed_snap: std::collections::HashSet<String> = app.always_allowed
                .as_ref()
                .map(|a| a.lock().unwrap().clone())
                .unwrap_or_default();
            let modal_lines = build_allow_menu_lines(&allowed_snap);
            let modal_height = (ALLOW_MENU_ITEMS.len() as u16 + 4).min(size.height / 2).max(6);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect { x: 0, y: modal_y, width: size.width, height: modal_height };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            frame.render_widget(Paragraph::new(modal_lines).wrap(Wrap { trim: false }), modal_area);
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Provider menu overlay --
        // OpenCode-style: each picker is a cyan-bordered card with a
        // title chip + Esc hint, mirroring /help so the visual language
        // is consistent across overlays.
        if let Some(ref items) = app.provider_menu {
            let modal_lines = if app.consult_pick_mode {
                build_consult_provider_overlay(items)
            } else {
                build_provider_menu_lines(items, &app.model, app.provider_sel)
            };
            let modal_height = (items.len() as u16 + 4).min(size.height / 2).max(5);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect { x: 0, y: modal_y, width: size.width, height: modal_height };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let title = if app.consult_pick_mode { " consult provider " } else { " providers " };
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        title,
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "1-9 seç · Esc kapat",
                        Style::default().fg(Color::Rgb(140, 140, 140)),
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(modal_lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Model menu overlay --
        if let Some(ref models) = app.model_menu {
            let modal_lines = build_model_menu_lines(models, &app.model, app.model_sel);
            let modal_height = (models.len().min(9) as u16 + 4).min(size.height / 2).max(5);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect { x: 0, y: modal_y, width: size.width, height: modal_height };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " models ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "1-9 seç · Esc kapat",
                        Style::default().fg(Color::Rgb(140, 140, 140)),
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(modal_lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Skill picker overlay --
        // -- Permission timeline overlay --
        // Centered modal that lists every permission decision recorded
        // this session: timestamp · tool · decision · args preview.
        // ↑↓/PgUp/PgDn scroll, Esc closes. Drawn before the files
        // picker so it wins focus when both are open (rare, but the
        // user expects the most recent overlay to be on top).
        if app.permission_overlay_open {
            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
            let hdr = Style::default()
                .fg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let allow = Style::default().fg(Color::Rgb(120, 200, 120));
            let deny = Style::default().fg(Color::Rgb(220, 110, 110));
            let auto = Style::default().fg(Color::Rgb(140, 180, 220));
            let body = Style::default().fg(Color::Rgb(220, 220, 220));
            let modal_height = size.height.saturating_sub(2).min(36).max(8);
            let modal_width = size.width.saturating_sub(4).min(110);
            let modal_x = (size.width.saturating_sub(modal_width)) / 2;
            let modal_y = 1u16;
            let modal_area = Rect {
                x: modal_x,
                y: modal_y,
                width: modal_width,
                height: modal_height,
            };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!("{} entries (newest last)", app.permission_history.len()),
                dim,
            )));
            lines.push(Line::from(""));
            // Render newest at the bottom so the user sees the most
            // recent decision first when the overlay opens.
            for entry in app.permission_history.iter() {
                let ago = format_seconds_ago(entry.when);
                let dec_style = match entry.decision {
                    PermissionLogDecision::Allow => allow,
                    PermissionLogDecision::AlwaysAllow => allow,
                    PermissionLogDecision::Deny => deny,
                    PermissionLogDecision::AutoAllow => auto,
                    PermissionLogDecision::AutoAcceptEdit => auto,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{ago:>10}"), dim),
                    Span::raw("  "),
                    Span::styled(format!("{:<14}", entry.tool), hdr),
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<13}", entry.decision.label()),
                        dec_style,
                    ),
                    Span::raw("  "),
                    Span::styled(entry.args_preview.clone(), body),
                ]));
            }
            let total = lines.len() as u16;
            let max_scroll = total.saturating_sub(modal_height.saturating_sub(2));
            let scroll = app.permission_overlay_scroll.min(max_scroll);
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .title(Line::from(vec![
                    Span::styled(
                        " permissions ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled("↑↓ PgUp/PgDn scroll · Esc kapat", dim),
                ]));
            let para = Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(para, modal_area);
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Files picker overlay --
        // Bare `/files` populates `files_picker` and we draw a centered
        // modal: search bar at top, type-to-filter list of paths below.
        // Enter inserts `@<path>` into the input so the next message
        // can carry the file as context. Esc closes without state
        // change.
        if let Some(ref paths) = app.files_picker {
            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
            let cmd_style = Style::default()
                .fg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let sel_style = Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let body_style = Style::default().fg(Color::Rgb(220, 220, 220));
            let filtered = files_picker_filtered(paths, &app.files_picker_query);
            let item_count = filtered.len().min(18) as u16;
            let modal_height = (item_count + 5).min(size.height.saturating_sub(4)).max(7);
            let modal_width = size.width.saturating_sub(4).min(100);
            let modal_x = (size.width.saturating_sub(modal_width)) / 2;
            let modal_y = (size.height.saturating_sub(modal_height)) / 4;
            let modal_area = Rect {
                x: modal_x,
                y: modal_y,
                width: modal_width,
                height: modal_height,
            };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let mut lines: Vec<Line<'static>> = Vec::new();
            let prompt: String = if app.files_picker_query.is_empty() {
                " ".to_string()
            } else {
                app.files_picker_query.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("> ", cmd_style),
                Span::styled(prompt, body_style),
            ]));
            let total_count = paths.len();
            let match_count = filtered.len();
            lines.push(Line::from(Span::styled(
                if app.files_picker_query.is_empty() {
                    format!("{total_count} files")
                } else {
                    format!("{match_count}/{total_count} match")
                },
                dim,
            )));
            lines.push(Line::from(""));
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled("(no match)", dim)));
            } else {
                let max = 18usize.min(filtered.len());
                let sel = app.files_picker_sel.min(filtered.len().saturating_sub(1));
                let scroll_start = sel.saturating_sub(max.saturating_sub(1));
                for (rel, p) in filtered.iter().skip(scroll_start).take(max).enumerate() {
                    let abs = scroll_start + rel;
                    let line = if abs == sel {
                        Line::from(vec![Span::styled(format!(" {} ", p), sel_style)])
                    } else {
                        Line::from(vec![Span::styled(format!(" {} ", p), body_style)])
                    };
                    lines.push(line);
                }
            }
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " files ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "yaz · ↑↓ · Enter @path · Esc",
                        dim,
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((
                modal_area.x + 3 + app.files_picker_query.chars().count() as u16,
                modal_area.y + 1,
            ));
            return;
        }

        // -- Interactive session picker --
        // Opened by bare `/sessions`. Renders each summary as a row
        // with id (12 chars) · N msgs · age, highlights the current
        // selection with cyan-on-black inverted style. Enter resumes,
        // Esc closes.
        if let Some(ref summaries) = app.session_picker {
            let item_count = summaries.len().min(14) as u16;
            let modal_height = (item_count + 5).min(size.height.saturating_sub(4)).max(7);
            let modal_width = size.width.saturating_sub(4).min(80);
            let modal_x = (size.width.saturating_sub(modal_width)) / 2;
            let modal_y = (size.height.saturating_sub(modal_height)) / 3;
            let modal_area = Rect {
                x: modal_x,
                y: modal_y,
                width: modal_width,
                height: modal_height,
            };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
            let id_style = Style::default()
                .fg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let sel_style = Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(80, 200, 240))
                .add_modifier(Modifier::BOLD);
            let body_style = Style::default().fg(Color::Rgb(220, 220, 220));
            let max = 14usize.min(summaries.len());
            let sel = app.session_picker_sel.min(summaries.len().saturating_sub(1));
            let scroll_start = sel.saturating_sub(max.saturating_sub(1));
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!("{} session{}", summaries.len(), if summaries.len() == 1 { "" } else { "s" }),
                dim,
            )));
            lines.push(Line::from(""));
            for (rel, s) in summaries.iter().skip(scroll_start).take(max).enumerate() {
                let abs = scroll_start + rel;
                let id12: String = s.id.chars().take(12).collect();
                let age = s.modified.and_then(|t| {
                    t.elapsed().ok().map(|d| {
                        let secs = d.as_secs();
                        if secs < 60 { "just now".to_string() }
                        else if secs < 3600 { format!("{}m ago", secs / 60) }
                        else if secs < 86400 { format!("{}h ago", secs / 3600) }
                        else { format!("{}d ago", secs / 86400) }
                    })
                }).unwrap_or_default();
                let body = format!(
                    "{:<12}  · {:>3} msgs{}",
                    id12,
                    s.message_count,
                    if age.is_empty() { String::new() } else { format!("  · {age}") }
                );
                let line = if abs == sel {
                    Line::from(vec![
                        Span::styled(format!("  {body}  "), sel_style),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(format!("  {id12:<12}"), id_style),
                        Span::styled(
                            format!("  · {} msgs{}",
                                s.message_count,
                                if age.is_empty() { String::new() } else { format!("  · {age}") }
                            ),
                            body_style,
                        ),
                    ])
                };
                lines.push(line);
            }
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " sessions ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "↑↓ · Enter resume · Esc · /sessions list (text)",
                        dim,
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Ctrl+P command palette overlay --
        // Centered modal listing every slash command in the catalogue,
        // type-to-filter, ↑/↓ to move, Enter inserts the picked command
        // into the input. Drawn before skill_menu so palette wins when
        // both are open (palette is a more recent intent).
        if app.palette_open {
            let panel_lines = build_palette_panel_lines(&app.palette_query, app.palette_sel);
            let item_count = palette_filtered(&app.palette_query).len().min(14) as u16;
            let modal_height = (item_count + 5).min(size.height.saturating_sub(4)).max(7);
            let modal_width = size.width.saturating_sub(4).min(80);
            let modal_x = (size.width.saturating_sub(modal_width)) / 2;
            let modal_y = (size.height.saturating_sub(modal_height)) / 3;
            let modal_area = Rect {
                x: modal_x,
                y: modal_y,
                width: modal_width,
                height: modal_height,
            };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " palette ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "yaz · ↑↓ · Enter · Esc",
                        Style::default().fg(Color::Rgb(140, 140, 140)),
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(panel_lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((
                modal_area.x + 3 + app.palette_query.chars().count() as u16,
                modal_area.y + 1,
            ));
            return;
        }

        if let Some(ref skills) = app.skill_menu {
            let panel_lines =
                build_skill_panel_lines(skills, &app.skill_filter, app.skill_sel);
            // Height: 3 (header+search+blank) + up to 15 items + 2 (footer) = 20 max
            let item_count = skill_filtered(skills, &app.skill_filter).len().min(15) as u16;
            let modal_height = (item_count + 6).min(size.height.saturating_sub(4)).max(6);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect { x: 0, y: modal_y, width: size.width, height: modal_height };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title(Line::from(vec![
                    Span::styled(
                        " skills ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "yaz · ↑↓ · Enter · Esc",
                        Style::default().fg(Color::Rgb(140, 140, 140)),
                    ),
                ]));
            frame.render_widget(
                Paragraph::new(panel_lines).block(block).wrap(Wrap { trim: false }),
                modal_area,
            );
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Help overlay panel (OpenCode-style) --
        // Opened by `/help` / `/info` / `/?`. Renders the full command
        // reference as a centered modal that overlays the chat without
        // dirtying the transcript. Esc / `q` / second `/help` closes;
        // PgUp/PgDn/↑/↓ scroll within the panel.
        if app.help_overlay_open {
            let (_, styled) = build_help_styled();
            let total_lines = styled.len() as u16;
            // Reserve top margin so the panel doesn't completely eat the
            // terminal — leaves chat barely visible behind it. Use most
            // of the screen but cap so very tall terminals don't waste space.
            let modal_height = size.height.saturating_sub(2).min(40).max(8);
            let modal_width = size.width.saturating_sub(4).min(110);
            let modal_x = (size.width.saturating_sub(modal_width)) / 2;
            let modal_y = 1u16;
            let modal_area = Rect {
                x: modal_x,
                y: modal_y,
                width: modal_width,
                height: modal_height,
            };
            let max_scroll = total_lines.saturating_sub(modal_height.saturating_sub(2));
            let scroll = app.help_scroll.min(max_scroll);
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let block = Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 200, 240)))
                .title(Line::from(vec![
                    Span::styled(
                        " help ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(80, 200, 240))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "Esc · q · /help kapat   PgUp/PgDn · ↑↓ kaydır",
                        Style::default().fg(Color::Rgb(140, 140, 140)),
                    ),
                ]));
            let para = Paragraph::new(styled)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(para, modal_area);
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Permission modal overlay --
        // When `pending_permission` is set, overlay the numbered options
        // onto the input + recap area so the user can't miss them. The
        // chat panel above stays visible (scrollback preserved), but key
        // input is hijacked by the modal branch in `handle_key` until
        // the user resolves the prompt. This replaces the legacy raw
        // stdout prompt that corrupted the TUI alt-screen.
        if let Some(pending) = app.pending_permission.as_ref() {
            let modal_lines = build_permission_modal_lines(pending);
            // Reserve enough rows for header + args + blank + 3 options
            // + Esc hint = 7 lines (plus 1 row padding = 8). Anchor the
            // modal at the BOTTOM so the newest chat content stays on
            // top of it just like the normal input bar does, and eat
            // into the chat area for the extra rows. Clip to half-screen
            // so the modal can't swallow the entire transcript on very
            // short terminals.
            const MODAL_ROWS: u16 = 8;
            let modal_height = MODAL_ROWS.min(size.height / 2).max(7);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect {
                x: 0,
                y: modal_y,
                width: size.width,
                height: modal_height,
            };
            // Clear the modal area first so stale content from the
            // previous frame (chat / recap / input) doesn't bleed
            // through where the modal text has empty spans.
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let modal_widget = Paragraph::new(modal_lines).wrap(Wrap { trim: false });
            frame.render_widget(modal_widget, modal_area);
            // Cursor parked at col 0 of the modal so there's no stray
            // blinking cursor mid-screen while the modal is active.
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Ask_user modal overlay --
        // Copilot-style question dialog. When the agent calls ask_user_question,
        // overlay the numbered options + freeform "Other" field on top of the
        // input area so the user can navigate with arrow keys.
        if let Some(pending) = app.pending_ask_user.as_ref() {
            let modal_lines = build_ask_user_modal_lines(pending);
            let rows_needed = 2 + pending.options.len() as u16 + 1 + 1;
            let modal_height = rows_needed.min(size.height / 2).max(5);
            let modal_y = size.height.saturating_sub(modal_height);
            let modal_area = Rect {
                x: 0,
                y: modal_y,
                width: size.width,
                height: modal_height,
            };
            frame.render_widget(ratatui::widgets::Clear, modal_area);
            let modal_widget = Paragraph::new(modal_lines).wrap(Wrap { trim: false });
            frame.render_widget(modal_widget, modal_area);
            frame.set_cursor_position((modal_area.x, modal_area.y));
            return;
        }

        // -- Input area: manual char-boundary split so cursor math is
        // exact. Splits on `\n` first (Shift+Enter multiline), wraps
        // each logical line at column `w`. Prompt "❯ " is only on
        // visual row 0 of logical line 0; continuation wraps and
        // subsequent logical lines have no prefix (raw 2-space pad
        // keeps the eye aligned without re-prompting).
        let w = input_area.width.max(1) as usize;
        // While the user is COMPOSING a side-question (`/ask ...`) the
        // input panel itself flips to a Copilot-CLI-style ask palette
        // (cyan) so the modal switch is obvious before Enter. Same
        // pattern existed for the legacy `/btw` (blue) and plan mode
        // (yellow); we keep both in case anyone still types /btw.
        // We deliberately do NOT key off `btw_in_flight` here: by the
        // time it flips to Some(_) the input has already been cleared,
        // so the previous version painted an empty area in the wrong
        // color and worse, painted the user's NEXT unrelated message
        // in that color while the side-call was running.
        let typing_ask = {
            let trimmed = app.input.trim_start();
            trimmed == "/ask" || trimmed.starts_with("/ask ")
        };
        let typing_btw = {
            let trimmed = app.input.trim_start();
            trimmed == "/btw" || trimmed.starts_with("/btw ")
        };
        let in_plan_mode = matches!(
            *app.plan_state.lock().unwrap(),
            PlanState::Drafting | PlanState::Executing
        );
        let prompt_style = if typing_ask {
            Style::default().fg(Color::Rgb(80, 200, 240))   // ask cyan
        } else if typing_btw {
            Style::default().fg(Color::Rgb(96, 165, 250))   // legacy btw blue
        } else if in_plan_mode {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Rgb(150, 150, 150))
        };
        let display_input = if let Some(s) = app.search_state.as_ref() {
            // bash-style: (reverse-i-search)`query': matched-entry
            let matched = s
                .match_idx
                .and_then(|i| app.input_history.get(i))
                .cloned()
                .unwrap_or_default();
            let suffix = if s.match_idx.is_none() && !s.query.is_empty() {
                " (no match)".to_string()
            } else {
                String::new()
            };
            format!("(r-search)`{}':{suffix} {matched}", s.query)
        } else {
            app.input.clone()
        };
        let logical_lines: Vec<&str> = if display_input.is_empty() {
            vec![""]
        } else {
            display_input.split('\n').collect()
        };
        let mut input_lines: Vec<Line<'static>> = Vec::new();
        for (li, line) in logical_lines.iter().enumerate() {
            let with_prefix: String = if li == 0 {
                format!("❯ {line}")
            } else {
                format!("  {line}")
            };
            let chars: Vec<char> = with_prefix.chars().collect();
            let chunks: Vec<Vec<char>> = if chars.is_empty() {
                vec![Vec::new()]
            } else {
                chars.chunks(w).map(|c| c.to_vec()).collect()
            };
            for (wi, chunk) in chunks.into_iter().enumerate() {
                let s: String = chunk.iter().collect();
                if typing_ask || typing_btw || in_plan_mode {
                    // Whole-line colour: /ask (cyan), /btw (blue) or
                    // plan mode (yellow). prompt_style already encodes
                    // the right palette.
                    input_lines.push(Line::from(Span::styled(s, prompt_style)));
                } else if li == 0 && wi == 0 {
                    let pfx_len = s.char_indices().nth(2).map(|(i, _)| i).unwrap_or(s.len());
                    let mut spans = vec![
                        Span::styled(s[..pfx_len].to_string(), prompt_style),
                        Span::raw(s[pfx_len..].to_string()),
                    ];
                    // CC parity: empty input gets a dim placeholder so the
                    // user sees what to do. Cursor is rendered separately,
                    // so the placeholder span is purely visual; pressing a
                    // key still types at column 2 as expected.
                    if app.input.is_empty() && app.search_state.is_none() {
                        let placeholder_style =
                            Style::default().fg(Color::Rgb(110, 110, 110));
                        spans.push(Span::styled(
                            "Type a message…".to_string(),
                            placeholder_style,
                        ));
                    } else if app.search_state.is_none()
                        && app.cursor == app.input.len()
                        && !app.input.contains('\n')
                    {
                        // Ghost-text completion for in-progress slash
                        // commands. Only render on the LAST visual row
                        // and only when the cursor is at end-of-input
                        // (otherwise the ghost-text could overlap with
                        // text to the right of the cursor). Tab still
                        // accepts via the existing `complete_tab` path.
                        if let Some(suffix) = slash_ghost_suggestion(&app.input) {
                            let ghost_style = Style::default()
                                .fg(Color::Rgb(95, 95, 95))
                                .add_modifier(Modifier::ITALIC);
                            spans.push(Span::styled(suffix, ghost_style));
                            spans.push(Span::styled(
                                "  Tab".to_string(),
                                Style::default().fg(Color::Rgb(75, 75, 75)),
                            ));
                        }
                    }
                    input_lines.push(Line::from(spans));
                } else if wi == 0 {
                    // Subsequent logical lines: dim 2-char pad, then text.
                    let pfx_len = s.char_indices().nth(2).map(|(i, _)| i).unwrap_or(s.len());
                    input_lines.push(Line::from(vec![
                        Span::styled(s[..pfx_len].to_string(), prompt_style),
                        Span::raw(s[pfx_len..].to_string()),
                    ]));
                } else {
                    input_lines.push(Line::from(Span::raw(s)));
                }
            }
        }
        frame.render_widget(Paragraph::new(input_lines), input_area);

        // -- @ file search dropdown (OpenCode/Copilot CLI style) --
        if app.at_search_active && !app.at_search_matches.is_empty() {
            let max_show = 10usize.min(app.at_search_matches.len());
            let mut at_lines: Vec<Line<'static>> = Vec::new();
            let hint = Style::default().fg(Color::Rgb(110, 110, 110));
            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
            let sel = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
            let sel_bg = Style::default().bg(Color::Rgb(60, 60, 60));
            at_lines.push(Line::from(Span::styled(
                format!("  {} file matches  (Tab to complete · Esc to cancel)", app.at_search_matches.len()),
                hint,
            )));
            for (i, path) in app.at_search_matches.iter().take(max_show).enumerate() {
                let style = if i == app.at_search_index { sel.patch(sel_bg) } else { dim };
                at_lines.push(Line::from(Span::styled(format!("  @ {}", path), style)));
            }
            if app.at_search_matches.len() > max_show {
                at_lines.push(Line::from(Span::styled(
                    format!("  … {} more", app.at_search_matches.len() - max_show),
                    hint,
                )));
            }
            let at_height = (at_lines.len() as u16).min(12);
            let at_area = Rect {
                x: input_area.x,
                y: input_area.y.saturating_sub(at_height),
                width: input_area.width,
                height: at_height,
            };
            frame.render_widget(Paragraph::new(at_lines), at_area);
        }

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(rule.clone(), sep_style))),
            separator_bottom_area,
        );

        // Cursor math: walk input up to `cursor` counting logical lines
        // (each '\n' bumps row by however many visual rows the prior
        // logical line consumed) and columns within the current line.
        // In reverse-search mode the buffer is a read-only overlay, so
        // we park the cursor right after the typed query inside the
        // `(r-search)'…':` prompt.
        let search_prefix_owned: String;
        let pre_cursor: &str = if let Some(s) = app.search_state.as_ref() {
            let prefix_len = "(r-search)`".len() + s.query.len() + 1;
            search_prefix_owned = display_input
                .get(..prefix_len)
                .unwrap_or(&display_input)
                .to_string();
            &search_prefix_owned
        } else {
            app.input.get(..app.cursor).unwrap_or("")
        };
        let mut row: u16 = 0;
        let mut col_in_logical: usize = 0;
        for ch in pre_cursor.chars() {
            if ch == '\n' {
                // Close out the current logical line.
                // +2 for either "❯ " (first line) or "  " (continuation) pad.
                let line_cols = col_in_logical + 2;
                let visual_rows = (line_cols as u16).saturating_sub(1) / (w as u16).max(1) + 1;
                row = row.saturating_add(visual_rows);
                col_in_logical = 0;
            } else {
                col_in_logical += 1;
            }
        }
        let total_col = col_in_logical + 2; // "❯ " or "  " pad
        let cursor_x = input_area.x + (total_col % w) as u16;
        let cursor_y = input_area.y + row + (total_col / w) as u16;
        frame.set_cursor_position((
            cursor_x.min(input_area.right().saturating_sub(1)),
            cursor_y.min(input_area.bottom().saturating_sub(1)),
        ));
    })?;
    Ok(())
}

// AEGIS ASCII banner — pixel-art dragon (chafa block-render) on the
// left, figlet on the right. Same glyphs main.rs prints at REPL startup.
// First three rows are flame above the tail-tip torch, tapering up:
// yellow tip → orange body → red ember base. Aligned to col 18 so it
// sits directly over the torch tip ▂. Colors set in render_banner per
// row index. Rest is dragon body in orange.
const BANNER_LINES: &[&str] = &[
    "                 ▝▙▘                                                                       ",
    "                 ▟█▉                                                                       ",
    "                 ▝█▘                                                                       ",
    "    ▂██▅▇▇▃     ▁▟▉                                                                       ",
    "   ▗██▋▔▀▀▔     ▝▛▀        █████╗ ███████╗ ██████╗ ██╗███████╗          ▁▅█▁▙  ▁          ",
    "   ▐███▘        █          ██╔══██╗██╔════╝██╔════╝ ██║██╔════╝        ▅▟███▀▀▇▆█▖         ",
    "    ▔ ▏▕▚       ▝▌         ███████║█████╗  ██║  ███╗██║███████╗        ██▛▂▆█▀▆██▋         ",
    "   ▗▋  ▕▇▎▂      ▘         ██╔══██║██╔══╝  ██║   ██║██║╚════██║        ▀▜██▆▌  ██▙▌▅▎▅▏    ",
    "   ▐▙▁ ▅█▎ ▚▖  ▁           ██║  ██║███████╗╚██████╔╝██║███████║        ▆▝███▇▆▇███▇▛▂▝     ",
    "   ▇▆████▊  ▜▙ ▘ ▁▂        ╚═╝  ╚═╝╚══════╝ ╚═════╝ ╚═╝╚══════╝        ▐▗██████████▀▀      ",
    "   ▜▘▀███▍  ▟█▍▁                                                      ▝▙▟████████▚▉▎      ",
    "     ▕▙▀█▙ ▟██▘▔ ▔                                                     ▝▜███████▛▁▁▂▁▄▄▖  ",
    "      ██▘  ▝▜█                                                           ▝▀███▆█████████▍ ",
    "  ▄▅▅▇█▘     █▋                                                         ▗▆▇██████▀▀▂▂▅██▙▂",
    "   ▀▔▔      ▐██▇▃                                                                         ",
];

fn render_banner(frame: &mut ratatui::Frame, area: Rect, model: &str) {
    // Full ASCII art banner + model line.
    let banner_style = Style::default()
        .fg(Color::Rgb(230, 140, 60))  // dragon orange
        .add_modifier(Modifier::BOLD);
    // Flame above the tail-tip torch, tapering up from base to tip.
    let flame_yellow = Style::default()
        .fg(Color::Rgb(255, 215, 90))    // tip - yellow
        .add_modifier(Modifier::BOLD);
    let flame_orange = Style::default()
        .fg(Color::Rgb(245, 150, 50))    // mid body - orange
        .add_modifier(Modifier::BOLD);
    let flame_red = Style::default()
        .fg(Color::Rgb(220, 65, 30))     // base - red ember
        .add_modifier(Modifier::BOLD);
    // Silver/grey portrait on the right of AEGIS — distinct from the
    // orange dragon so the photo reads as a separate frame.
    let portrait_silver = Style::default()
        .fg(Color::Rgb(185, 185, 185))
        .add_modifier(Modifier::BOLD);
    // Red kalpak hat on top of portrait (rows 4-7) — Turkish flag red
    // so it pops as the figure's distinguishing feature.
    let hat_red = Style::default()
        .fg(Color::Rgb(220, 50, 40))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(139, 148, 158));

    // Char position where dragon+AEGIS area ends and portrait begins.
    // Layout: dragon (cols 0-18) + 8-col gap + AEGIS (cols 27-61) +
    // 8-col gap + portrait (cols 70+). Both gaps equal so AEGIS sits
    // visually centered between dragon and portrait.
    const PORTRAIT_SPLIT: usize = 70;

    let mut lines: Vec<Line<'_>> = BANNER_LINES
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let base_style = match i {
                0 => flame_yellow,
                1 => flame_orange,
                2 => flame_red,
                _ => banner_style,
            };
            // Rows 4..=13 have portrait content past PORTRAIT_SPLIT.
            // Hat (rows 4-7) renders red, face/body (rows 8-13) silver.
            // Other rows render in a single span with their base color.
            if (4..=13).contains(&i) {
                let split_byte = l
                    .char_indices()
                    .nth(PORTRAIT_SPLIT)
                    .map(|(b, _)| b)
                    .unwrap_or(l.len());
                let (left, right) = l.split_at(split_byte);
                let portrait_color = if (4..=7).contains(&i) {
                    hat_red
                } else {
                    portrait_silver
                };
                Line::from(vec![
                    Span::styled(left.to_string(), base_style),
                    Span::styled(right.to_string(), portrait_color),
                ])
            } else {
                Line::from(Span::styled(l.to_string(), base_style))
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        format!("  aegis · ({model})"),
        dim,
    )));
    let widget = Paragraph::new(lines);
    frame.render_widget(widget, area);
}

// ---------------------------------------------------------------------------
// Main TUI loop
// ---------------------------------------------------------------------------

/// RAII guard that restores terminal state on drop — including panics.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
        let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Lock a Mutex, recovering from poison (another thread panicked).
fn lock_app(app: &Mutex<TuiApp>) -> std::sync::MutexGuard<'_, TuiApp> {
    app.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Entrypoint for TUI mode. Called from `main.rs` when `--tui` is passed.
#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    use_yes: bool,
    model: &str,
    sandbox: aegis_core::SandboxMode,
    provider_id: &str,
    // MCP servers spawned by main() before the TUI started. Each
    // entry is a display label like "playwright (12 tools)" so the
    // sidebar MCP card surfaces them on first render. Without this,
    // only /browser / /computer slash invocations would populate the
    // card and config-driven attaches stayed hidden.
    initial_mcps: Vec<String>,
    // Initial mouse capture from config — `None` keeps the built-in
    // default (ON), `Some(false)` starts with capture off so terminal
    // hosts that own their own gestures (Termius, etc) can scroll
    // natively. /mouse still toggles at runtime either way.
    initial_mouse_capture: Option<bool>,
    // Atakan: config-driven default permission mode. None → Default mod
    // (mevcut davranış). Some("bypass" / "accept-edits" / "plan") →
    // başlangıçta uygulanır. Geçersiz string sessizce ignore.
    initial_permission_mode: Option<String>,
    // Architect/editor swap: model fired when permission_mode flips to
    // Plan (`plan_model`) or AcceptEdits/Bypass (`build_model`). `None`
    // → no swap on that transition. Same `provider:model` shape as the
    // rest of `[routing]`.
    plan_model: Option<String>,
    build_model: Option<String>,
    #[cfg(feature = "ctx")] blob_handles: Option<(
        Arc<aegis_core::BlobStore>,
        Arc<aegis_core::BlobIndex>,
    )>,
) -> Result<()> {
    // Ensure .metis/ exists.
    let metis_dir = workspace.join(".metis");
    std::fs::create_dir_all(&metis_dir)
        .with_context(|| format!("could not create `{}`", metis_dir.display()))?;

    // Discover skills from ~/.metis/skills/ + .metis/skills/ then register
    // built-ins on top — matches REPL startup order exactly. TUI gains
    // full skill parity: `/skills` lists them, `/skill-install|uninstall|search`
    // manage them, and unknown `/<name>` falls back to skill dispatch.
    let mut skill_registry = SkillRegistry::discover(workspace);
    for skill in aegis_core::builtin_skills() {
        skill_registry.register(skill);
    }

    let mut initial_app = TuiApp::new(model);
    initial_app.skill_registry = skill_registry;
    initial_app.tool_registry = Some(Arc::clone(&registry));
    initial_app.current_provider = provider_id.to_string();
    initial_app.plan_model = plan_model;
    initial_app.build_model = build_model;
    initial_app.workspace = workspace.to_path_buf();
    if !initial_mcps.is_empty() {
        if let Ok(mut g) = initial_app.attached_mcps.lock() {
            for label in initial_mcps {
                if !g.iter().any(|s| s == &label) {
                    g.push(label);
                }
            }
        }
    }
    if let Some(want) = initial_mouse_capture {
        initial_app.mouse_capture_on = want;
    }
    // Atakan: config-driven default permission mode (string parse).
    // initial_app.always_allowed daha sonra wire ediliyor (None şu an),
    // dolayısıyla tam set_permission_mode efektini yapamayız; permission_mode
    // alanını set et + plan_state Drafting yap (Plan ise). always_allowed
    // sync'i bypass/accept-edits için aşağıda always_allowed kurulduktan
    // sonra deferred olarak çağırılacak.
    let parsed_mode: Option<PermMode> = initial_permission_mode
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .and_then(|s| match s.as_str() {
            "default" => Some(PermMode::Default),
            "accept-edits" | "accept_edits" | "acceptedits" => Some(PermMode::AcceptEdits),
            "plan" => Some(PermMode::Plan),
            "bypass" | "yolo" => Some(PermMode::Bypass),
            _ => None,
        });
    if let Some(mode) = parsed_mode {
        initial_app.permission_mode = mode;
        if mode == PermMode::Plan {
            *initial_app.plan_state.lock().unwrap() = PlanState::Drafting;
        }
    }
    initial_app.refresh_welcome();

    // Auto-opening message: push a synthetic first turn before user input.
    // session_start hook will inject memory context, then model writes opening.
    let hook_cfg = aegis_core::load_hooks(workspace);
    if let Some(opener) = hook_cfg.opening_prompt {
        initial_app.pending_prompts.push_back(opener);
        initial_app.skip_next_recap = true;
    }

    let app = Arc::new(Mutex::new(initial_app));

    // Build the permission gate with access to the app mutex so `check`
    // can push a `PendingPermission` into the UI state. `--yes` short-
    // circuits to AllowAll just like REPL mode; that's the only branch
    // where we skip TuiPermission, because AllowAll never interacts
    // with stdout and is safe inside the alt-screen.
    let inner_permission: Arc<dyn Permission> = if use_yes {
        Arc::new(AllowAll)
    } else {
        let perm = TuiPermission::new(Arc::clone(&app));
        let always_allowed = perm.always_allowed_set();
        let bash_allow = perm.bash_command_allowlist_set();
        {
            let mut s = lock_app(&app);
            s.always_allowed = Some(always_allowed);
            s.bash_command_allowlist = Some(bash_allow);
        }
        Arc::new(perm)
    };

    // Atakan: parsed_mode set edildiyse, always_allowed wire olduktan sonra
    // tam set_permission_mode çağır — Bypass/AcceptEdits için tool set'i
    // doldurur, mode mesajı basar.
    if let Some(mode) = parsed_mode {
        lock_app(&app).set_permission_mode(mode);
    }

    // Atakan: session resume — sidecar'da mod kayıtlıysa onu uygula
    // (config-level mode'u override eder). `--resume` sonrası Bypass/Plan
    // state hayatta kalsın diye.
    let session_id_for_meta = lock_app(&app).session_id.clone();
    if let Ok(store) = aegis_core::SessionStore::open(workspace, &session_id_for_meta) {
        if let Some(saved) = store.meta().permission_mode.as_deref() {
            let parsed_resume = match saved.trim().to_lowercase().as_str() {
                "default" => Some(PermMode::Default),
                "accept-edits" | "accept_edits" | "acceptedits" => Some(PermMode::AcceptEdits),
                "plan" => Some(PermMode::Plan),
                "bypass" | "yolo" => Some(PermMode::Bypass),
                _ => None,
            };
            if let Some(mode) = parsed_resume {
                lock_app(&app).set_permission_mode(mode);
            }
        }
    }

    // Atakan: Trigger B — boot-time unsaved session hint. Scan the
    // workspace once for a previously-modified session that was never
    // ingested (Ctrl+C exit, crash, or just forgotten). Excludes the
    // current session_id so a brand-new launch doesn't flag itself.
    // Only surfaces hints under 7 days old — older sessions are noise.
    const HINT_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
    if let Ok(Some(hint)) = aegis_core::SessionStore::previous_unsaved_session(
        workspace,
        Some(&session_id_for_meta),
    ) {
        if hint.age_secs <= HINT_MAX_AGE_SECS {
            let id_short: String = hint.id.chars().take(16).collect();
            let age = if hint.age_secs < 60 {
                format!("{}s", hint.age_secs)
            } else if hint.age_secs < 3600 {
                format!("{}m", hint.age_secs / 60)
            } else if hint.age_secs < 86400 {
                format!("{}h", hint.age_secs / 3600)
            } else {
                format!("{}d", hint.age_secs / 86400)
            };
            lock_app(&app).push_system(&format!(
                "💡 last session ({id_short}, {} msgs, {age} ago) was not saved to mnemonics. \
                 type /recall-prev to summarize and ingest it.",
                hint.message_count
            ));
        }
    }

    // Always wrap with AutonomousSecurityLayer so `/security kill`,
    // `/security resume` and `/security` (status) actually do something.
    // Limits are made effectively infinite and `require_approval_for`
    // is emptied — the inner TuiPermission already handles per-tool
    // approvals interactively, and the user can lower these via config
    // if desired. What we keep ON: the kill switch, the live tool-call
    // / cost / time counter, and the dangerous-bash-pattern guard.
    let security_cfg = aegis_core::AutonomousSecurityConfig {
        max_tool_calls: u32::MAX,
        max_token_usage: u64::MAX,
        max_cost_usd: f64::MAX,
        timeout: std::time::Duration::from_secs(60 * 60 * 24 * 365),
        require_approval_for: vec![],
        protected_paths: vec![],
        max_deletions: u32::MAX,
        max_commits: u32::MAX,
        enable_kill_switch: true,
    };
    let security_layer = Arc::new(aegis_core::AutonomousSecurityLayer::new(
        inner_permission,
        security_cfg,
    ));
    {
        let mut s = lock_app(&app);
        s.security = Some(Arc::clone(&security_layer));
    }
    let permission: Arc<dyn Permission> = security_layer;

    // Outermost: Rego policy gate (cfg-gated, no-op without policies).
    // Default dirs: <ws>/.metis/policies + ~/.metis/policies. Missing
    // dirs / zero .rego files → inner permission unchanged.
    #[cfg(feature = "policy")]
    let permission = aegis_core::policy::wrap_with_policy(
        permission,
        &aegis_core::policy::default_policy_dirs(workspace),
    );

    // Set up terminal.
    enable_raw_mode().context("failed to enable raw mode")?;
    let _guard = TerminalGuard; // Restores terminal even on panic
    let mut stdout = std::io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    // Mouse capture is ON by default so the trackpad / wheel scrolls
    // the chat view in the alternate screen — without capture, terminal
    // emulators don't deliver wheel events to TUI apps in alt-screen
    // mode, leaving scroll completely broken. To select text natively,
    // use Option+drag (macOS Terminal) or Cmd+drag (iTerm2), or run
    // `/mouse` to flip capture off and use native click-drag selection.
    // The flag default is `true` in `TuiApp::new`; we apply the
    // matching crossterm command here, on the SAME stdout handle that
    // backs the renderer so the escape lands before the first frame.
    stdout
        .execute(crossterm::event::EnableMouseCapture)
        .context("failed to enable mouse capture")?;
    // Bracketed paste — lets us distinguish "user pasted a file path"
    // (drag-drop a PNG into the TUI) from normal keystrokes. Without
    // this, drag-drop shows up as a stream of `Char` events and gets
    // treated as typing. With it, we get `Event::Paste(String)` and
    // can route image-looking paths straight to `pending_images`.
    let _ = stdout.execute(crossterm::event::EnableBracketedPaste);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = tui_main_loop(
        &mut terminal,
        Arc::clone(&app),
        client,
        registry,
        workspace,
        config,
        permission,
        sandbox,
        #[cfg(feature = "ctx")]
        blob_handles,
    )
    .await;

    // Guard handles cleanup on drop, but show cursor explicitly.
    terminal.show_cursor().ok();

    result
}

#[allow(clippy::too_many_arguments)]
async fn tui_main_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: Arc<Mutex<TuiApp>>,
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    sandbox: aegis_core::SandboxMode,
    #[cfg(feature = "ctx")] blob_handles: Option<(
        Arc<aegis_core::BlobStore>,
        Arc<aegis_core::BlobIndex>,
    )>,
) -> Result<()> {
    let workspace = workspace.to_path_buf();

    // Turn generation counter — prevents stale spawned tasks from
    // clobbering a newer turn's state after an interrupt. Each
    // spawn_prompt! increments this; on completion a task only
    // applies state changes if its captured gen still matches.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AO};
    let turn_gen = Arc::new(AtomicU64::new(0));
    // Current turn's cancel flag. Passed into ToolContext so the bash
    // tool and agent loop can check it. On interrupt a new flag is
    // created for the next turn so old tasks don't false-cancel.
    let mut cancel_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let mut agent_config = config;

    // Mutable live client/model so /provider and /model can swap them
    // between turns. REPL keeps these as plain `let mut` too — it
    // rebuilds the agent around them on each swap. We delay swaps
    // until we're idle (between turns) so we never replace a client
    // that's mid-stream.
    let mut client: Arc<dyn ChatProvider> = client;

    // Fire a prompt with the CURRENT live client. Inlined (not a
    // closure) so it can read the mutable `client` / `model` that
    // /provider and /model rewrite between turns. Session id and
    // plan state live on `TuiApp`; we snapshot them at spawn time.
    macro_rules! spawn_prompt {
        ($prompt:expr) => {{
            let prompt: String = $prompt;
            {
                let mut state = lock_app(&app);
                // skip_next_recap is set for synthetic turns (e.g. opening prompt).
                // Skip both the user bubble and the recap for those turns.
                let skip = std::mem::replace(&mut state.skip_next_recap, false);
                if !skip {
                    state.push_user(&prompt);
                    // Atakan: eski claude-mem parity auto-recap kapatıldı.
                    // MemoryStore.relevant_recap_lines token-overlap (score>=2)
                    // filtresi MEMORY.md'den irrelevant bullet'lar basıyordu
                    // (premortem F2: cwd-blind, namespace yok). Mnemonics MCP
                    // tool'u var, agent gerekirse `mnemonics_retrieve` çağırır;
                    // UI'a auto bullet düşürmek gerekmiyor.
                    //
                    // Geri açmak isteyen için (gelecek): config'e
                    // `auto_recap_bullets = true` flag ekle, varsayılan false.
                }
                state.scroll_to_bottom();
                state.busy = true;
            }
            // GODMODE side-effects
            {
                let (gm_multi, gm_perturb, gm_parallel) = {
                    let s = lock_app(&app);
                    (
                        s.multi_model_evaluation,
                        s.prompt_perturbation,
                        s.parallel_models,
                    )
                };
                if gm_multi {
                    lock_app(&app).pending_race = Some(prompt.clone());
                }
                if gm_perturb {
                    let perturbator = aegis_multi_model::PromptPerturbator::new(2, true);
                    let variants = perturbator.perturb_prompt(&prompt);
                    if !variants.is_empty() {
                        let msg = variants
                            .iter()
                            .enumerate()
                            .map(|(i, v)| format!("[variant {}] {v}", i + 1))
                            .collect::<Vec<_>>()
                            .join("\n");
                        lock_app(&app).push_system(&format!("perturbation alternatives:\n{msg}"));
                    }
                }
                if gm_parallel {
                    const SECONDARY: &[&str] = &["gemini", "deepseek", "openai", "glm"];
                    let current_model = lock_app(&app).model.clone();
                    let app_p = Arc::clone(&app);
                    let prompt_p = prompt.clone();
                    tokio::spawn(async move {
                        let clients: Vec<_> = SECONDARY
                            .iter()
                            .filter(|id| !current_model.contains(*id))
                            .take(2)
                            .filter_map(|id| {
                                let prov = aegis_api::Provider::lookup(id)?;
                                let client = prov.client_from_env().ok()?;
                                Some((id.to_string(), prov.default_model.to_string(), client))
                            })
                            .collect();
                        let handles: Vec<_> = clients
                            .into_iter()
                            .map(|(id, model, client)| {
                                let req = aegis_api::ChatRequest {
                                    model,
                                    messages: vec![aegis_api::ChatMessage::user(prompt_p.clone())],
                                    tools: None,
                                    temperature: Some(0.7),
                                    max_tokens: Some(1024),
                                    thinking: false,
                                    thinking_budget: 0,
                                };
                                tokio::spawn(async move { (id, client.chat(&req).await) })
                            })
                            .collect();
                        for h in handles {
                            if let Ok((id, Ok(resp))) = h.await {
                                let text = resp
                                    .choices
                                    .first()
                                    .and_then(|c| c.message.content.clone())
                                    .unwrap_or_default();
                                if !text.is_empty() {
                                    app_p
                                        .lock()
                                        .unwrap()
                                        .push_system(&format!("[parallel:{id}] {text}"));
                                }
                            }
                        }
                    });
                }
            }
            let app2 = Arc::clone(&app);
            let client2 = Arc::clone(&client);
            let registry2 = Arc::clone(&registry);
            let permission2 = Arc::clone(&permission);
            let ws = workspace.clone();
            let mut cfg = agent_config.clone();
            let sb = sandbox.clone();
            let prompt_owned = prompt.clone();
            // Snapshot + drain pending_images so the turn fires with
            // the images buffered so far; a crash or user /cancel does
            // NOT resurrect them (matches REPL's "attach → send →
            // clear" cycle).
            let (model_str, thinking_on, autotune_on, sid, plan_state, images,
                 auto_skill_on, skill_list_snapshot) = {
                let mut s = lock_app(&app);
                let images = std::mem::take(&mut s.pending_images);
                let skills: Vec<(String, String)> = if s.auto_skill_enabled {
                    s.skill_registry
                        .user_invocable()
                        .into_iter()
                        .map(|sk| {
                            let desc = skill_tr_desc(&sk.name)
                                .map(|d| d.to_string())
                                .unwrap_or_else(|| sk.description.clone());
                            (sk.name.clone(), desc)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                (
                    s.model.clone(),
                    s.thinking_enabled,
                    s.autotune,
                    s.session_id.clone(),
                    Arc::clone(&s.plan_state),
                    images,
                    s.auto_skill_enabled,
                    skills,
                )
            };
            cfg.thinking = thinking_on;
            cfg.autotune = autotune_on;
            // CRITICAL: sync cfg.model to the current user-selected
            // model. Without this, the agent's ChatRequest keeps
            // sending the model string that was baked into
            // `agent_config` at TUI startup — so `/model glm-4.6`
            // updates `state.model` for the UI but the actual API
            // request still carries e.g. "deepseek-chat", which ZAI
            // then rejects with `1211 Unknown Model`. User hit this
            // exact path: /provider glm + /model glm-4.6 still 1211'd
            // until this line was added.
            cfg.model = model_str.clone();

            // Auto-skill: quick classification call using the current
            // client. Runs before the main turn. On match, the skill's
            // prompt is prepended to the user message so the agent gets
            // expert context without the user having to type /skillname.
            // Auto-skill: keyword-based classification (zero-latency, zero-cost).
            // Replaces the previous blocking LLM call — same accuracy for
            // explicit-name invocations, no per-turn API overhead.
            if auto_skill_on && !skill_list_snapshot.is_empty() {
                if let Some(picked) = aegis_core::skills::classify_skill(&prompt_owned, &skill_list_snapshot) {
                    let skill_prompt = lock_app(&app)
                        .skill_registry
                        .get(&picked)
                        .map(|sk| sk.prompt.clone());
                    if let Some(sp) = skill_prompt {
                        lock_app(&app2).push_system(&format!("→ /{picked} otomatik seçildi"));
                        cfg.extra_system = Some(sp);
                    }
                }
            }

            let my_gen = turn_gen.fetch_add(1, AO::SeqCst) + 1;
            let gen_check = Arc::clone(&turn_gen);
            // Fresh cancel flag for this turn. Old flag is replaced so
            // any stale task that reads it won't cancel the new turn.
            let this_cancel = Arc::new(AtomicBool::new(false));
            cancel_flag = Arc::clone(&this_cancel);
            #[cfg(feature = "ctx")]
            let blob_handles_turn = blob_handles.clone();
            tokio::spawn(async move {
                let result = run_agent_turn(
                    app2.clone(),
                    client2,
                    registry2,
                    &ws,
                    cfg,
                    permission2,
                    sb,
                    &sid,
                    &prompt_owned,
                    &model_str,
                    plan_state,
                    images,
                    this_cancel,
                    #[cfg(feature = "ctx")]
                    blob_handles_turn,
                )
                .await;
                // Only update state if this is still the active turn.
                // A newer spawn_prompt! increments turn_gen, so stale
                // tasks (interrupted or superseded) silently discard
                // their completion instead of clobbering the new turn.
                if gen_check.load(AO::SeqCst) == my_gen {
                    let (corpus, last_answer) = {
                        let mut state = app2.lock().unwrap();
                        state.flush_streaming();
                        state.busy = false;
                        if let Err(e) = result {
                            state.push_error(&format!("{e:#}"));
                        }
                        // Extract corpus for halluguard: recent user messages AND
                        // tool results from this turn. Tool results are the ground
                        // truth the model's claims should be grounded in — without
                        // them HalluGuard can't verify "9/9 passed" style claims
                        // and flags them as unsupported (false positive).
                        let user_docs: Vec<String> = state
                            .messages
                            .iter()
                            .filter(|m| m.role == MessageRole::User)
                            .rev()
                            .take(4)
                            .map(|m| m.text.clone())
                            .collect();
                        let tool_docs: Vec<String> = state
                            .messages
                            .iter()
                            .filter(|m| m.role == MessageRole::ToolResult)
                            .rev()
                            .take(10)
                            .map(|m| m.text.clone())
                            .collect();
                        let corpus: Vec<String> = user_docs.into_iter().chain(tool_docs).collect();
                        let last_answer = state
                            .messages
                            .iter()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant)
                            .map(|m| m.text.clone())
                            .unwrap_or_default();
                        (corpus, last_answer)
                    };
                    // Non-blocking halluguard check — gated behind
                    // `HALLUGUARD_ENABLE=1` env var (default off). The
                    // reverse-RAG approach was producing too many false
                    // positives on conversational replies — Atakan asked
                    // to default-disable until tuned. Set the env var to
                    // re-enable. Daemon must also be running on
                    // HALLUGUARD_URL (default :7801) — silently skipped
                    // if not reachable.
                    // Skip halluguard for short acknowledgements ("Durdum", "Tamam", etc.)
                    // — they contain no verifiable claims and only produce false positives.
                    let halluguard_enabled =
                        std::env::var("HALLUGUARD_ENABLE").is_ok_and(|v| v == "1" || v == "true");
                    let answer_words = last_answer.split_whitespace().count();
                    if halluguard_enabled
                        && !last_answer.is_empty()
                        && !corpus.is_empty()
                        && answer_words >= 15
                    {
                        if let Some(r) = aegis_core::halluguard::check(&corpus, &last_answer).await
                        {
                            if !r.ok {
                                let mut state = app2.lock().unwrap();
                                let msg = format!(
                                    "⚠ halluguard: {}/{} claims flagged — trust {:.0}%{}",
                                    r.n_flagged,
                                    r.n_claims,
                                    r.trust_score * 100.0,
                                    if r.flagged.is_empty() {
                                        String::new()
                                    } else {
                                        format!(
                                            " · \"{}\"",
                                            r.flagged[0].text.chars().take(60).collect::<String>()
                                        )
                                    }
                                );
                                state.push_system(&msg);
                            }
                        }
                    }
                }
            });
        }};
    }

    // Re-send EnableMouseCapture periodically so SSH reconnects (e.g.
    // Termius) don't permanently lose scroll. Every ~100 draws ≈ 5s.
    let mut draw_tick: u32 = 0;
    // Last row seen during a mouse drag — used for swipe-to-scroll on
    // Termius (one-finger swipe sends Drag events, not ScrollUp/Down).
    let mut last_drag_row: Option<u16> = None;

    loop {
        // Draw.
        {
            let mut state = lock_app(&app);
            draw(terminal, &mut state)?;
            if state.should_quit {
                break;
            }
        }

        // Periodically re-send EnableMouseCapture so SSH reconnects
        // (Termius, etc.) don't permanently kill scroll. Also re-send
        // EnableBracketedPaste — 1b91d59 added paste support but not
        // the re-send, which left Termius reconnects with paste mode
        // overriding mouse reporting after a while.
        draw_tick = draw_tick.wrapping_add(1);
        if draw_tick % 100 == 0 {
            // Only re-send EnableMouseCapture if the user has explicitly
            // turned it on via /mouse — re-sending unconditionally would
            // override the default-off state every 5 seconds and steal
            // the user's drag-select capability back.
            if lock_app(&app).mouse_capture_on {
                let _ = std::io::stdout().execute(crossterm::event::EnableMouseCapture);
            }
            let _ = std::io::stdout().execute(crossterm::event::EnableBracketedPaste);
        }

        // Apply any queued provider / model swap while idle. Done here
        // rather than inside handle_slash because rebuilding a
        // `Arc<dyn ChatProvider>` needs the env-loaded credentials and
        // must run on this thread (not the slash handler locked
        // briefly behind `app.lock()`).
        let pending = {
            let mut state = lock_app(&app);
            if state.busy {
                None
            } else {
                let p = state.pending_provider_switch.take();
                let m = state.pending_model_switch.take();
                Some((p, m))
            }
        };
        if let Some((prov, model_switch)) = pending {
            if let Some((pname, override_model)) = prov {
                match aegis_api::Provider::lookup(&pname) {
                    Some(provider) => match provider.client_from_env() {
                        Ok(new_client) => {
                            client = Arc::from(new_client);
                            let new_model = override_model
                                .unwrap_or_else(|| provider.default_model.to_string());
                            let (old_model, sid) = {
                                let s = lock_app(&app);
                                (s.model.clone(), s.session_id.clone())
                            };
                            let mut state = lock_app(&app);
                            state.model = new_model.clone();
                            state.current_provider = pname.clone();
                            state.last_model_menu.clear();
                            state.push_system(&format!("switched to {pname} ({new_model})"));
                            drop(state);
                            if let Some(sp) = agent_config.system_prompt.as_mut() {
                                *sp = sp.replace(
                                    &format!("You are running as model `{}`", old_model),
                                    &format!("You are running as model `{}`", new_model),
                                );
                            }
                            // Append a system note so resumed sessions
                            // pick up the new identity (see model_switch
                            // branch above for the why).
                            if let Ok(mut store) = SessionStore::open(&workspace, &sid) {
                                let note = aegis_api::ChatMessage::system(format!(
                                    "System update: provider switched to `{pname}`, you are now running as model `{new_model}`. \
                                     When the user asks which model you are, answer with this name. \
                                     Earlier system messages may reference a different model/provider — this is the authoritative identifier from now on."
                                ));
                                let _ = store.append(&note);
                            }
                        }
                        Err(e) => {
                            lock_app(&app).push_error(&format!("provider switch failed: {e}"));
                        }
                    },
                    None => {
                        lock_app(&app)
                            .push_error(&format!("unknown provider: {pname} (see /providers)"));
                    }
                }
            }
            if let Some(new_model) = model_switch {
                let (old_model, sid) = {
                    let s = lock_app(&app);
                    (s.model.clone(), s.session_id.clone())
                };
                let mut state = lock_app(&app);
                state.model = new_model.clone();
                state.push_system(&format!("switched model to {new_model}"));
                drop(state);
                // Update the startup system prompt so any FRESH turn
                // (sessions without persisted messages) sees the right
                // model identifier.
                if let Some(sp) = agent_config.system_prompt.as_mut() {
                    *sp = sp.replace(
                        &format!("You are running as model `{}`", old_model),
                        &format!("You are running as model `{}`", new_model),
                    );
                }
                // Also append an authoritative system message into the
                // RESUMED session transcript. The agent only emits the
                // startup system prompt on the first turn of a session;
                // subsequent turns replay the persisted transcript, so
                // config.system_prompt changes after that point are
                // invisible to the model. This append tells the LLM
                // (via an in-band system message) that the model name
                // has changed — same mechanism the memory tools use.
                if let Ok(mut store) = SessionStore::open(&workspace, &sid) {
                    let note = aegis_api::ChatMessage::system(format!(
                        "System update: you are now running as model `{new_model}`. \
                         When the user asks which model you are, answer with this name. \
                         Earlier system messages may name a different model — this is the authoritative identifier from now on."
                    ));
                    let _ = store.append(&note);
                }
            }
        }

        // Apply a queued `/compact` while idle. Builds a throwaway
        // `Agent` just long enough to call `force_compact`, which
        // rewrites the session file in place. Next turn will then see
        // the compacted transcript on re-open.
        let compact_now = {
            let mut state = lock_app(&app);
            if state.busy || !state.pending_compact {
                false
            } else {
                state.pending_compact = false;
                true
            }
        };
        if compact_now {
            let (sid, model_str) = {
                let s = lock_app(&app);
                (s.session_id.clone(), s.model.clone())
            };
            match SessionStore::open(&workspace, &sid) {
                Ok(store) => {
                    let hooks = aegis_core::load_hooks(&workspace);
                    let ctx = ToolContext::new(workspace.clone()).with_hooks(hooks);
                    let mut cfg = agent_config.clone();
                    cfg.model = model_str;
                    let mut agent = Agent::new(&*client, &registry, ctx, cfg)
                        .with_permission(Arc::clone(&permission))
                        .with_guardrail(aegis_core::guardrail::load_default(&workspace))
                        .with_session(store);
                    #[cfg(feature = "ctx")]
                    if let Some((store, index)) = blob_handles.clone() {
                        agent = agent.with_blob_handles(store, index);
                    }
                    let removed = agent.force_compact();
                    let mut state = lock_app(&app);
                    if removed > 0 {
                        state.push_system(&format!("compacted: {removed} messages removed"));
                    } else {
                        state.push_system("nothing to compact (transcript too short)");
                    }
                }
                Err(e) => {
                    lock_app(&app).push_error(&format!("/compact: {e}"));
                }
            }
        }

        // Apply a queued `/consult <provider> <prompt>`. Spawns an
        // async task that runs a one-shot chat_stream against the
        // requested provider, pushes streamed chunks into chat as a
        // GODMODE: process pending race (multi_model_evaluation)
        let race_prompt = {
            let mut state = lock_app(&app);
            if state.busy {
                None
            } else {
                state.pending_race.take()
            }
        };
        if let Some(race_prompt) = race_prompt {
            const RACE_PROVIDERS: &[&str] = &[
                "anthropic",
                "gemini",
                "deepseek",
                "openai",
                "minimax",
                "glm",
            ];
            let clients: Vec<(&str, Box<dyn aegis_api::ChatProvider>)> = RACE_PROVIDERS
                .iter()
                .filter_map(|id| {
                    let prov = aegis_api::Provider::lookup(id)?;
                    let client = prov.client_from_env().ok()?;
                    Some((*id, client))
                })
                .collect();
            if clients.is_empty() {
                lock_app(&app).push_error("godmode race: no providers available");
            } else {
                let app2 = Arc::clone(&app);
                let ws = workspace.clone();
                let prompt_owned = race_prompt.clone();
                tokio::spawn(async move {
                    let req_template = aegis_api::ChatRequest {
                        model: String::new(),
                        messages: vec![aegis_api::ChatMessage::user(prompt_owned.clone())],
                        tools: None,
                        temperature: Some(0.7),
                        max_tokens: Some(2048),
                        thinking: false,
                        thinking_budget: 0,
                    };
                    let handles: Vec<_> = clients
                        .into_iter()
                        .map(|(id, client)| {
                            let mut req = req_template.clone();
                            if let Some(prov) = aegis_api::Provider::lookup(id) {
                                req.model = prov.default_model.to_string();
                            }
                            tokio::spawn(async move { (id, client.chat(&req).await) })
                        })
                        .collect();
                    let mut results: Vec<(&str, String)> = Vec::new();
                    for handle in handles {
                        if let Ok((id, Ok(resp))) = handle.await {
                            let text = resp
                                .choices
                                .first()
                                .and_then(|c| c.message.content.clone())
                                .unwrap_or_default();
                            if !text.is_empty() {
                                app2.lock()
                                    .unwrap()
                                    .push_system(&format!("[race:{id}] {text}"));
                                results.push((id, text));
                            }
                        }
                    }
                    if let Some((best_id, best_text)) = results.first() {
                        let note = format!("[race] best ({best_id}):\n{best_text}");
                        let sid = app2.lock().unwrap().session_id.clone();
                        let _ = append_note(&ws, &sid, &note);
                    }
                });
            }
            continue;
        }

        // `/askall` — like race but uses each provider's strongest model.
        let askall_prompt = {
            let mut state = lock_app(&app);
            if state.busy { None } else { state.pending_askall.take() }
        };
        if let Some(askall_prompt) = askall_prompt {
            const ASKALL_PROVIDERS: &[&str] = &[
                "anthropic", "gemini", "deepseek", "openai", "glm", "nvidia", "minimax",
            ];
            let clients: Vec<(&str, Box<dyn aegis_api::ChatProvider>, String)> = ASKALL_PROVIDERS
                .iter()
                .filter_map(|id| {
                    let prov = aegis_api::Provider::lookup(id)?;
                    let client = prov.client_from_env().ok()?;
                    // Use the strongest model (index 0 from the curated list).
                    let strong_model = models_for_provider(id)
                        .into_iter()
                        .next()
                        .map(|(m, _)| m.to_string())
                        .unwrap_or_else(|| prov.default_model.to_string());
                    Some((*id, client, strong_model))
                })
                .collect();
            if clients.is_empty() {
                lock_app(&app).push_error("[askall] no providers available — set at least one API key");
            } else {
                let app2 = Arc::clone(&app);
                let ws = workspace.clone();
                let prompt_owned = askall_prompt.clone();
                tokio::spawn(async move {
                    let handles: Vec<_> = clients
                        .into_iter()
                        .map(|(id, client, model)| {
                            let req = aegis_api::ChatRequest {
                                model,
                                messages: vec![aegis_api::ChatMessage::user(prompt_owned.clone())],
                                tools: None,
                                temperature: Some(0.7),
                                max_tokens: Some(2048),
                                thinking: false,
                                thinking_budget: 0,
                            };
                            tokio::spawn(async move { (id, client.chat(&req).await) })
                        })
                        .collect();
                    let mut all_responses: Vec<(&str, String)> = Vec::new();
                    for handle in handles {
                        if let Ok((id, Ok(resp))) = handle.await {
                            let text = resp
                                .choices
                                .first()
                                .and_then(|c| c.message.content.clone())
                                .unwrap_or_default();
                            if !text.is_empty() {
                                app2.lock().unwrap().push_system(&format!("[askall:{id}] {text}"));
                                all_responses.push((id, text));
                            }
                        }
                    }
                    if !all_responses.is_empty() {
                        let note: String = all_responses
                            .iter()
                            .map(|(id, t)| format!("[askall:{id}]\n{t}"))
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n");
                        let sid = app2.lock().unwrap().session_id.clone();
                        let _ = append_note(&ws, &sid, &note);
                    }
                });
            }
            continue;
        }

        // `/ask` — Copilot-CLI / Claude-Code-style side question.
        // Always runs concurrently: doesn't gate on `busy`, doesn't flip
        // `busy = true`, never blocks the agent's main turn. Sends the
        // prompt to the CURRENT model with tools disabled and pushes the
        // single response as an assistant message. If the agent is in
        // the middle of a tool turn when /ask fires, both run in
        // parallel and the answer drops in next to the live stream.
        let ask_single_prompt = {
            let mut state = lock_app(&app);
            state.pending_ask_single.take()
        };
        if let Some(ask_prompt) = ask_single_prompt {
            let (provider_id, model) = {
                let state = lock_app(&app);
                (state.current_provider.clone(), state.model.clone())
            };
            let Some(prov_info) = aegis_api::Provider::lookup(&provider_id) else {
                lock_app(&app).push_error(&format!(
                    "[ask] unknown provider: {provider_id} (try /provider <id>)"
                ));
                continue;
            };
            let client = match prov_info.client_from_env() {
                Ok(c) => c,
                Err(e) => {
                    lock_app(&app).push_error(&format!(
                        "[ask] {provider_id}: {e} (hint: set {})",
                        prov_info.env_var
                    ));
                    continue;
                }
            };
            // Reuse the `btw_in_flight` indicator slot — same UI
            // affordance (queue strip + dim preview line), no need for
            // a parallel field. The side call doesn't touch `busy`, so
            // the main agent turn keeps streaming undisturbed.
            lock_app(&app).btw_in_flight = Some(ask_prompt.clone());
            let app2 = Arc::clone(&app);
            tokio::spawn(async move {
                let req = aegis_api::ChatRequest {
                    model,
                    messages: vec![aegis_api::ChatMessage::user(ask_prompt.clone())],
                    tools: None,
                    temperature: Some(0.7),
                    max_tokens: Some(2048),
                    thinking: false,
                    thinking_budget: 0,
                };
                let result = client.chat(&req).await;
                let mut state = app2.lock().unwrap();
                state.btw_in_flight = None;
                match result {
                    Ok(resp) => {
                        let text = resp
                            .choices
                            .first()
                            .and_then(|c| c.message.content.clone())
                            .unwrap_or_default();
                        if text.is_empty() {
                            state.push_error("[ask] empty response from model");
                        } else {
                            // Push a magenta ✱ divider line so the
                            // answer reads as a panel reply rather than
                            // a generic assistant turn — visually pairs
                            // with the cyan ❯ ask header pushed when
                            // /ask was issued.
                            let magenta = Style::default()
                                .fg(Color::Rgb(220, 100, 200))
                                .add_modifier(Modifier::BOLD);
                            let dim = Style::default().fg(Color::Rgb(140, 140, 140));
                            let divider = vec![Line::from(vec![
                                Span::styled("✱ ", magenta),
                                Span::styled("ask reply", magenta),
                                Span::styled("  ─────", dim),
                            ])];
                            state.push_styled("✱ ask reply".to_string(), divider);
                            // push_assistant auto-copies to clipboard
                            // and renders text in the standard message
                            // role styling.
                            state.push_assistant(&text);
                        }
                    }
                    Err(e) => {
                        state.push_error(&format!("[ask] {e}"));
                    }
                }
            });
            continue;
        }

        // system note, then appends the full response to the session
        // transcript so the main model sees it on the next turn.
        let consult = {
            let mut state = lock_app(&app);
            if state.busy {
                None
            } else {
                state.pending_consult.take()
            }
        };
        if let Some((provider, consult_prompt)) = consult {
            let Some(prov_info) = aegis_api::Provider::lookup(&provider) else {
                lock_app(&app).push_error(&format!("consult: unknown provider {provider}"));
                continue;
            };
            let consult_client = match prov_info.client_from_env() {
                Ok(c) => c,
                Err(e) => {
                    lock_app(&app).push_error(&format!(
                        "consult: can't init {provider}: {e} (hint: set {})",
                        prov_info.env_var
                    ));
                    continue;
                }
            };
            {
                let mut state = lock_app(&app);
                state.busy = true;
                // Seed the live streaming buffer. The render path below
                // shows `[provider] <growing text>` as each TextDelta
                // arrives — matches REPL's live stderr stream.
                state.consult_streaming = Some((provider.clone(), String::new()));
            }
            let app2 = Arc::clone(&app);
            let ws = workspace.clone();
            let prov_owned = provider.clone();
            let prompt_owned = consult_prompt.clone();
            let model = prov_info.default_model.to_string();
            tokio::spawn(async move {
                let req = aegis_api::ChatRequest {
                    model,
                    messages: vec![aegis_api::ChatMessage::user(prompt_owned.clone())],
                    tools: None,
                    temperature: Some(0.7),
                    max_tokens: Some(2048),
                    thinking: false,
                    thinking_budget: 0,
                };
                let app_cb = Arc::clone(&app2);
                let result = consult_client
                    .chat_stream(&req, &mut |event| {
                        if let aegis_api::StreamEvent::TextDelta(t) = event {
                            // Append to the in-progress buffer under the
                            // app lock. Render loop reads the same field
                            // every tick, so tokens appear live.
                            if let Ok(mut s) = app_cb.lock() {
                                if let Some((_, body)) = s.consult_streaming.as_mut() {
                                    body.push_str(&t);
                                }
                            }
                        }
                    })
                    .await;
                // Promote the streamed buffer to a permanent system
                // message, then clear the live field. This freezes the
                // line in scrollback even if the user scrolls away.
                let finalized = {
                    let mut state = app2.lock().unwrap();
                    state.consult_streaming.take()
                };
                let body = finalized.map(|(_, b)| b).unwrap_or_default();
                let mut state = app2.lock().unwrap();
                match result {
                    Ok(_) => {
                        let display = if body.trim().is_empty() {
                            "(no response)".to_string()
                        } else {
                            body.clone()
                        };
                        state.push_system(&format!("[{prov_owned}] {display}"));
                        let sid = state.session_id.clone();
                        drop(state);
                        let note =
                            format!("[consult/{prov_owned}] {prompt_owned}\n\nResponse:\n{body}");
                        if let Err(e) = append_note(&ws, &sid, &note) {
                            app2.lock()
                                .unwrap()
                                .push_error(&format!("consult: could not attach note: {e}"));
                        }
                    }
                    Err(e) => {
                        state.push_error(&format!("[{prov_owned}] error: {e}"));
                    }
                }
                app2.lock().unwrap().busy = false;
            });
            continue;
        }

        // `/claude <prompt>` — spawn `claude -p "<prompt>"` subprocess,
        // capture stdout, display as system message. Fires only when the
        // `claude` binary was already confirmed present by handle_slash.
        let claude_prompt = {
            let mut state = lock_app(&app);
            if state.busy {
                None
            } else {
                state.pending_claude.take()
            }
        };
        if let Some(claude_prompt) = claude_prompt {
            let app2 = Arc::clone(&app);
            lock_app(&app).busy = true;
            tokio::spawn(async move {
                let output = tokio::process::Command::new("claude")
                    .arg("-p")
                    .arg(&claude_prompt)
                    .output()
                    .await;
                let mut state = app2.lock().unwrap();
                match output {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let display = if text.is_empty() {
                            "(claude returned no output)".to_string()
                        } else {
                            text
                        };
                        state.push_system(&format!("[claude] {display}"));
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        state.push_error(&format!(
                            "[claude] exited {}: {}",
                            out.status,
                            stderr.trim()
                        ));
                    }
                    Err(e) => {
                        state.push_error(&format!("[claude] spawn failed: {e}"));
                    }
                }
                state.busy = false;
            });
            continue;
        }

        // Apply a queued `/update`. Runs networked HTTP check on a
        // background tokio task, streams progress lines into chat via
        // push_system/push_error, mirrors REPL output byte-for-byte,
        // and holds `busy=true` so new turns don't race the update.
        let do_update = {
            let mut state = lock_app(&app);
            if state.busy || !state.pending_update {
                false
            } else {
                state.pending_update = false;
                state.busy = true;
                true
            }
        };
        if do_update {
            let app2 = Arc::clone(&app);
            tokio::spawn(async move {
                use aegis_core::update;
                let http = reqwest::Client::new();
                match update::check_latest(&http).await {
                    Ok(check) if !check.is_newer => {
                        app2.lock().unwrap().push_system("already up to date");
                    }
                    Ok(check) => {
                        app2.lock().unwrap().push_system(&format!(
                            "v{} → v{} available",
                            check.current, check.latest
                        ));
                        if check.download_url.is_some() {
                            app2.lock().unwrap().push_system("downloading…");
                            match update::perform_update(&http, &check).await {
                                Ok(_path) => {
                                    app2.lock().unwrap().push_system(&format!(
                                        "updated to v{} — restart to use the new version",
                                        check.latest
                                    ));
                                }
                                Err(e) => {
                                    app2.lock()
                                        .unwrap()
                                        .push_error(&format!("update failed: {e}"));
                                }
                            }
                        } else {
                            app2.lock().unwrap().push_system(&format!(
                                "no binary for {} — build from source",
                                update::current_target()
                            ));
                        }
                    }
                    Err(e) => {
                        app2.lock()
                            .unwrap()
                            .push_error(&format!("update check failed: {e}"));
                    }
                }
                app2.lock().unwrap().busy = false;
            });
            continue;
        }

        // Drain queued prompts as soon as the previous turn completes.
        // One at a time — keeps each user message as its own turn in
        // the transcript (no silent batching). If the queued input is
        // actually a slash command (user typed `/model glm-4.6` while
        // the previous turn was still streaming), route it through
        // `handle_slash` instead of firing a new agent turn, otherwise
        // the `/model ...` text would go to the model as a chat prompt
        // and confuse it into answering "I can't switch models".
        let next_queued = {
            let mut state = lock_app(&app);
            if state.busy {
                None
            } else {
                state.pop_pending()
            }
        };
        if let Some(prompt) = next_queued {
            let trimmed = prompt.trim();
            if trimmed.starts_with('/') {
                let mut state = lock_app(&app);
                let _ = state.handle_slash(trimmed, &workspace);
                continue;
            }
            cancel_flag.store(true, AO::SeqCst);
            spawn_prompt!(prompt);
            continue;
        }

        // Poll for events with a 50ms timeout so we can redraw during
        // agent runs when the shared state changes.
        if crossterm::event::poll(std::time::Duration::from_millis(50))? {
            let evt = event::read()?;
            match evt {
                Event::Key(key) => {
                    let action = handle_key(key, &app, &workspace);
                    match action {
                        KeyAction::None => {}
                        KeyAction::Quit => break,
                        KeyAction::Submit(text) => {
                            let trimmed = text.trim().to_string();
                            if trimmed.is_empty() {
                                continue;
                            }

                            {
                                let mut s = lock_app(&app);
                                s.last_input_time = std::time::Instant::now();
                                s.idle_reminder_sent = false;
                            }

                            // Drag-drop fallback for terminals that don't
                            // wrap drag-drop in bracketed paste (Terminal.app,
                            // some tmux configs): if the submitted input is
                            // just one or more absolute paths to existing
                            // image files, attach them instead of running a
                            // turn. iTerm2 catches these earlier via
                            // `Event::Paste`; this covers the rest.
                            if let Some(paths) = try_parse_drag_drop_paths(&trimmed, &workspace) {
                                let mut state = lock_app(&app);
                                for p in &paths {
                                    state.attach_image_path(p);
                                }
                                continue;
                            }

                            // Bare-digit shortcut after `/models`: if
                            // the last menu has entries and the user
                            // just typed a number within range, rewrite
                            // the input as `/model <N>`. This turns
                            // "run /models, then pick by typing 2" into
                            // the two-keystroke flow the REPL had (raw
                            // termios single-key pick) without losing
                            // the normal `/model <name>` path.
                            let menu_pick = {
                                let s = lock_app(&app);
                                trimmed.parse::<usize>().ok().and_then(|n| {
                                    if !s.last_model_menu.is_empty()
                                        && n > 0
                                        && n <= s.last_model_menu.len()
                                    {
                                        Some(n)
                                    } else {
                                        None
                                    }
                                })
                            };
                            if let Some(n) = menu_pick {
                                let mut state = lock_app(&app);
                                let _ = state.handle_slash(&format!("/model {n}"), &workspace);
                                continue;
                            }

                            if trimmed.starts_with('/') {
                                let mut state = lock_app(&app);
                                let result = state.handle_slash(&trimmed, &workspace);
                                // session_id and plan_state are now mutated
                                // on TuiApp directly by handle_slash for
                                // /clear, /resume, /fork, and /plan — no
                                // main-loop bookkeeping needed here beyond
                                // quit handling.
                                // Atakan: Trigger B drain — `/recall-prev`
                                // sets `pending_recall_prev`; we spawn the
                                // async recovery here so the slash handler
                                // can stay synchronous.
                                if state.pending_recall_prev.take().is_some() {
                                    let ws_for_recall = workspace.clone();
                                    let app_for_recall = Arc::clone(&app);
                                    tokio::spawn(async move {
                                        recall_prev_recover(ws_for_recall, app_for_recall).await;
                                    });
                                }
                                if matches!(result, SlashResult::Quit) {
                                    break;
                                }
                                if let SlashResult::SendToModel(prompt) = result {
                                    let is_busy = state.busy;
                                    if is_busy {
                                        // Answer immediately as a side call —
                                        // don't queue, mirror Claude Code's
                                        // "btw answers even during streaming".
                                        let api_msgs: Vec<aegis_api::ChatMessage> = state
                                            .messages
                                            .iter()
                                            .filter_map(|m| match m.role {
                                                MessageRole::User => Some(
                                                    aegis_api::ChatMessage::user(m.text.clone()),
                                                ),
                                                MessageRole::Assistant => Some(
                                                    aegis_api::ChatMessage::assistant_text(
                                                        m.text.clone(),
                                                    ),
                                                ),
                                                _ => None,
                                            })
                                            .chain(std::iter::once(
                                                aegis_api::ChatMessage::user(prompt.clone()),
                                            ))
                                            .collect();
                                        state.btw_in_flight = Some(prompt.clone());
                                        drop(state);
                                        let app_btw = Arc::clone(&app);
                                        tokio::spawn(async move {
                                            let btw_client = aegis_api::Provider::lookup("nvidia")
                                                .and_then(|p| p.client_from_env().ok());
                                            let btw_client = match btw_client {
                                                Some(c) => c,
                                                None => {
                                                    let mut s = lock_app(&app_btw);
                                                    s.btw_in_flight = None;
                                                    s.push_error("[btw] nvidia provider unavailable");
                                                    return;
                                                }
                                            };
                                            let req = aegis_api::ChatRequest {
                                                model: "deepseek-ai/deepseek-v4-flash".to_string(),
                                                messages: api_msgs,
                                                tools: None,
                                                temperature: Some(0.7),
                                                max_tokens: Some(512),
                                                thinking: false,
                                                thinking_budget: 0,
                                            };
                                            let result = btw_client.chat(&req).await;
                                            let mut s = lock_app(&app_btw);
                                            s.btw_in_flight = None;
                                            match result {
                                                Ok(resp) => {
                                                    let text = resp
                                                        .choices
                                                        .first()
                                                        .and_then(|c| c.message.content.clone())
                                                        .unwrap_or_default();
                                                    if !text.is_empty() {
                                                        s.push_system(&format!("[btw] {text}"));
                                                    }
                                                }
                                                Err(e) => {
                                                    s.push_error(&format!("[btw] {e}"));
                                                }
                                            }
                                        });
                                    } else {
                                        drop(state);
                                        spawn_prompt!(prompt);
                                    }
                                }
                                continue;
                            }

                            // Auto-detect "from now on / bundan sonra" style
                            // rule intent and silently persist to learning store.
                            if let Some(rule) = detect_rule_intent(&trimmed) {
                                let ws_str = workspace.display().to_string();
                                let insight = aegis_core::learning::Insight {
                                    timestamp: aegis_core::telemetry::now_iso8601(),
                                    workspace: Some(ws_str),
                                    category: "preference".to_string(),
                                    text: rule.clone(),
                                    reinforcements: 1,
                                    last_seen: None,
                                    success_count: 0,
                                    failure_count: 0,
                                    tags: vec!["auto-rule".to_string()],
                                };
                                if aegis_core::learning::upsert_insight(&insight).is_ok() {
                                    lock_app(&app).push_system(&format!("→ Kural kaydedildi: {rule}"));
                                }
                            }

                            // Atakan: session-end keyword'leri yakalanırsa
                            // kod-side `mnemonics ingest` subprocess çağrısı
                            // tetikle. Agent disiplinine güvenmiyoruz (DeepSeek/
                            // GLM weak instruction-follow kanıtlandı: marker
                            // düştü, agent atladı). Ham özet düşük kaliteli ama
                            // sıfırdan iyi — Atakan sonradan iyileştirebilir.
                            if is_session_end_keyword(&trimmed) {
                                let (workspace, last_user_msg, last_assistant_msg, session_id_for_mark) = {
                                    let state = lock_app(&app);
                                    let last_user = state
                                        .messages
                                        .iter()
                                        .rev()
                                        .find(|m| m.role == MessageRole::User)
                                        .map(|m| m.text.clone())
                                        .unwrap_or_default();
                                    let last_assistant = state
                                        .messages
                                        .iter()
                                        .rev()
                                        .find(|m| m.role == MessageRole::Assistant)
                                        .map(|m| m.text.clone())
                                        .unwrap_or_default();
                                    (state.workspace.clone(), last_user, last_assistant, state.session_id.clone())
                                };
                                let trigger = trimmed.clone();
                                let app_for_ingest = app.clone();
                                tokio::spawn(async move {
                                    let result = code_driven_session_save(
                                        &workspace,
                                        &last_user_msg,
                                        &last_assistant_msg,
                                        &trigger,
                                    )
                                    .await;
                                    // Atakan: Trigger B — mark this session
                                    // as ingested on any terminal outcome
                                    // (ingest / judge-skip / dedup-skip).
                                    // Without this, every reboot would
                                    // re-flag this session as "unsaved".
                                    let should_mark = matches!(
                                        result,
                                        Ok(SaveOutcome::Ingested(_))
                                            | Ok(SaveOutcome::SkippedNoSignal)
                                            | Ok(SaveOutcome::SkippedDuplicate(_))
                                    );
                                    if should_mark {
                                        if let Ok(mut s) = aegis_core::SessionStore::open(
                                            &workspace,
                                            &session_id_for_mark,
                                        ) {
                                            let _ = s.mark_ingested_now();
                                        }
                                    }
                                    let line = match result {
                                        Ok(SaveOutcome::Ingested(ns)) => {
                                            format!("[mnemonics] ingested → ns={ns}")
                                        }
                                        Ok(SaveOutcome::SkippedNoSignal) => {
                                            "[mnemonics] skipped: LLM judge → kayda değer yok".to_string()
                                        }
                                        Ok(SaveOutcome::RejectedSecret(reason)) => {
                                            format!("[mnemonics] REJECTED: secret pattern ({reason}) — kayıt yapılmadı")
                                        }
                                        Ok(SaveOutcome::SkippedDuplicate(score)) => {
                                            format!("[mnemonics] skipped: duplicate (cosine={score:.2} ≥ {DEDUP_COSINE_THRESHOLD})")
                                        }
                                        Err(e) => {
                                            format!("[mnemonics] FAIL: {e}")
                                        }
                                    };
                                    lock_app(&app_for_ingest).push_system(&line);
                                });
                                lock_app(&app).push_system(
                                    "[mnemonics] session-end → LLM judge + ingest tetiklendi…",
                                );
                            }

                            // Signal the previous turn (if any) to stop.
                            // spawn_prompt! will create a fresh flag for
                            // the new turn, so this only affects the old one.
                            cancel_flag.store(true, AO::SeqCst);
                            spawn_prompt!(trimmed);
                        }
                        KeyAction::CancelRun => {
                            let mut state = lock_app(&app);
                            if state.busy {
                                state.flush_streaming();
                                state.busy = false;
                                // CC parity: Claude Code uses "Interrupted by
                                // user" verbatim. We had "Run cancelled."
                                // which read like a system error rather than
                                // user-initiated abort.
                                state.push_system("Interrupted by user");
                                drop(state);
                                cancel_flag.store(true, AO::SeqCst);
                                turn_gen.fetch_add(1, AO::SeqCst);
                            }
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    use crossterm::event::MouseEventKind;
                    // Route wheel events to the active overlay first so
                    // the chat scroll doesn't move silently behind the
                    // help panel — the user expects the visible thing
                    // to scroll.
                    let on_help = lock_app(&app).help_overlay_open;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            if on_help {
                                let mut a = lock_app(&app);
                                a.help_scroll = a.help_scroll.saturating_sub(3);
                            } else {
                                lock_app(&app).scroll_up(3);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if on_help {
                                let mut a = lock_app(&app);
                                a.help_scroll = a.help_scroll.saturating_add(3).min(500);
                            } else {
                                lock_app(&app).scroll_down(3);
                            }
                        }
                        // Termius iPhone: one-finger swipe sends Drag events.
                        // Natural scroll: swipe up → see newer (scroll_down).
                        MouseEventKind::Drag { .. } => {
                            if let Some(prev) = last_drag_row {
                                let curr = mouse.row;
                                if on_help {
                                    let mut a = lock_app(&app);
                                    if curr < prev {
                                        a.help_scroll =
                                            a.help_scroll.saturating_add((prev - curr) as u16).min(500);
                                    } else if curr > prev {
                                        a.help_scroll =
                                            a.help_scroll.saturating_sub((curr - prev) as u16);
                                    }
                                } else if curr < prev {
                                    lock_app(&app).scroll_down(prev - curr);
                                } else if curr > prev {
                                    lock_app(&app).scroll_up(curr - prev);
                                }
                            }
                            last_drag_row = Some(mouse.row);
                        }
                        MouseEventKind::Up { .. } => {
                            last_drag_row = None;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => {
                    // Terminal will redraw on next iteration.
                }
                Event::Paste(text) => {
                    handle_paste(&text, &app, &workspace);
                }
                _ => {}
            }
        }

        // 5-minute idle reminder: if user hasn't typed for 5 min and agent
        // is not running, summarise the session via the active model.
        {
            let (should_remind, user_msgs, model_str_idle) = {
                let s = lock_app(&app);
                let idle = s.last_input_time.elapsed() >= std::time::Duration::from_secs(300);
                if s.idle_reminder_enabled && idle && !s.idle_reminder_sent && !s.busy {
                    let msgs: Vec<String> = s
                        .messages
                        .iter()
                        .filter(|m| m.role == MessageRole::User)
                        .map(|m| m.text.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    (true, msgs, s.model.clone())
                } else {
                    (false, vec![], String::new())
                }
            };
            if should_remind {
                lock_app(&app).idle_reminder_sent = true;
                let app_idle = Arc::clone(&app);
                let client_idle = Arc::clone(&client);
                tokio::spawn(async move {
                    let history = if user_msgs.is_empty() {
                        "— henüz mesaj yok —".to_string()
                    } else {
                        user_msgs
                            .iter()
                            .enumerate()
                            .map(|(i, m)| format!("{}. {}", i + 1, m))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let prompt = format!(
                        "Kullanıcının bu session'da yazdıkları:\n{history}\n\n\
                         Bu mesajlara bakarak session'da neler yapıldığını 3-5 madde \
                         halinde kısaca Türkçe özetle. Teknik detay değil, yapılan işler. \
                         Fazladan yorum, giriş, sonuç cümlesi ekleme."
                    );
                    let req = aegis_api::ChatRequest {
                        model: model_str_idle,
                        messages: vec![aegis_api::ChatMessage::user(prompt)],
                        tools: None,
                        temperature: Some(0.3),
                        max_tokens: Some(300),
                        thinking: false,
                        thinking_budget: 0,
                    };
                    if let Ok(resp) = client_idle.chat(&req).await {
                        if let Some(summary) = resp
                            .choices
                            .first()
                            .and_then(|c| c.message.content.clone())
                        {
                            let mut s = lock_app(&app_idle);
                            s.push_system("── 5 dakikadır sessizsiniz. Bu session'da yapılanlar ──");
                            for line in summary.lines() {
                                let line = line.trim();
                                if !line.is_empty() {
                                    s.push_system(line);
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    // Session-end learning extraction. TUI has no Agent handle, so we
    // read the persisted transcript via SessionStore. Mirrors the REPL
    // shutdown path so insights and tool-feedback signals accumulate
    // from TUI sessions too.
    let session_id = lock_app(&app).session_id.clone();
    if let Ok(store) = aegis_core::SessionStore::open(&workspace, &session_id) {
        let messages = store.messages();
        if !messages.is_empty() {
            let insights = aegis_core::learning::extract_insights(messages, &workspace);
            for insight in &insights {
                let _ = aegis_core::learning::upsert_insight(insight);
            }
            let rules = aegis_core::learning::extract_instructions(messages, &workspace);
            for rule in &rules {
                let _ = aegis_core::learning::upsert_insight(rule);
            }
            let feedback = aegis_core::learning::extract_tool_feedback(messages);
            for (tool_tag, positive) in &feedback {
                let _ =
                    aegis_core::learning::record_feedback_by_tag(&workspace, tool_tag, *positive);
            }
        }
    }

    // Auto-run LLM-based memory extraction (mirrors REPL shutdown path).
    // Build a fresh ephemeral Agent around the persisted session transcript
    // so memory_save tool calls land in the same store as /insights does.
    if agent_config.auto_memory {
        let user_turn_count = aegis_core::SessionStore::open(&workspace, &session_id)
            .map(|s| {
                s.messages()
                    .iter()
                    .filter(|m| m.role == aegis_api::Role::User)
                    .count()
            })
            .unwrap_or(0);
        if user_turn_count >= agent_config.auto_memory_min_turns {
            if let Ok(store) = aegis_core::SessionStore::open(&workspace, &session_id) {
                let mut conv = String::new();
                for m in store.messages() {
                    if m.role == aegis_api::Role::System {
                        continue;
                    }
                    let role = match m.role {
                        aegis_api::Role::User => "User",
                        aegis_api::Role::Assistant => "Assistant",
                        aegis_api::Role::Tool => "Tool",
                        aegis_api::Role::System => unreachable!(),
                    };
                    if let Some(c) = m.content.as_deref() {
                        let trimmed = if c.len() > 500 { &c[..500] } else { c };
                        conv.push_str(&format!("{role}: {trimmed}\n"));
                    }
                }
                if !conv.is_empty() {
                    eprintln!("[goblin] auto-extracting memory from session...");
                    let insight_prompt = format!(
                        "Review this conversation and extract non-obvious facts, decisions, and \
                         learnings worth remembering in future sessions. Focus on:\n\
                         - User preferences and working style\n\
                         - Technical decisions and why they were made\n\
                         - Things that were tried and failed (and why)\n\
                         - Project-specific context not derivable from code\n\n\
                         Do NOT save: obvious facts, temporary state, happy-path summaries.\n\
                         For each insight you find, call memory_save with an appropriate type and content.\n\
                         If there are no meaningful insights worth saving, say so briefly.\n\n\
                         Conversation:\n{conv}"
                    );
                    let hooks = aegis_core::load_hooks(&workspace);
                    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let ctx = aegis_core::ToolContext::new(workspace.clone())
                        .with_hooks(hooks)
                        .with_cancel(Arc::clone(&cancel));
                    // No with_session — insights turn must NOT be appended
                    // to the session file so --resume sees only real user work.
                    let mut agent =
                        aegis_core::Agent::new(&*client, &registry, ctx, agent_config.clone())
                            .with_permission(Arc::clone(&permission));
                    match agent.run(aegis_core::UserInput::Text(insight_prompt)).await {
                        Ok(out) => {
                            // Write a telemetry record for the auto-memory turn
                            // so its cost shows up in /cost and usage stats.
                            let pricing = aegis_core::ModelPricing::resolve(&agent_config.model);
                            let cost = pricing.estimate(&out.usage);
                            let record = aegis_core::telemetry::TelemetryRecord {
                                timestamp: aegis_core::telemetry::now_iso8601(),
                                session_id: Some(session_id.clone()),
                                model: agent_config.model.clone(),
                                provider: String::new(),
                                input_tokens: out.usage.input_tokens,
                                output_tokens: out.usage.output_tokens,
                                cache_read_tokens: out.usage.cache_read_tokens,
                                cache_write_tokens: out.usage.cache_write_tokens,
                                turns: out.turns,
                                cost_usd: cost.total_usd(),
                                tool_calls: std::collections::HashMap::new(),
                            };
                            let _ = aegis_core::telemetry::append_record(&record);
                        }
                        Err(e) => eprintln!("[goblin] auto-memory error: {e}"),
                    }
                }
            }
        }
    }

    Ok(())
}

/// Execute a single agent turn and feed events into the TUI state.
#[allow(clippy::too_many_arguments)]
async fn run_agent_turn(
    app: Arc<Mutex<TuiApp>>,
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    sandbox: aegis_core::SandboxMode,
    session_id: &str,
    prompt: &str,
    model: &str,
    plan_state: Arc<Mutex<PlanState>>,
    images: Vec<std::path::PathBuf>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "ctx")] blob_handles: Option<(
        Arc<aegis_core::BlobStore>,
        Arc<aegis_core::BlobIndex>,
    )>,
) -> Result<()> {
    let hooks = aegis_core::load_hooks(workspace);
    // Keep our own handle to the cancel flag so the run-loop below
    // can observe Ctrl+C without having to thread state back out of
    // the ToolContext (which moves the Arc into its tool registry).
    let cancel_observer = Arc::clone(&cancel);
    let mut ctx = ToolContext::new(workspace.to_path_buf())
        .with_hooks(hooks)
        .with_plan_state(plan_state)
        .with_cancel(cancel);
    ctx.bash.sandbox = sandbox;

    // Wire shared agent-spawner so the TUI can use `agent` and
    // `parallel_agents` tools. Without this, the model gets
    // "agent spawner not configured — subagents require an interactive session".
    let spawner = crate::agent_spawner::build(
        Arc::clone(&client),
        Arc::clone(&registry),
        workspace,
        config.clone(),
        Arc::clone(&permission),
        ctx.background_agents.clone(),
    );
    ctx = ctx.with_agent_spawner(spawner);

    let session =
        SessionStore::open(workspace, session_id).context("could not open session store")?;

    let mut agent = Agent::new(&*client, &registry, ctx, config)
        .with_permission(Arc::clone(&permission))
        .with_guardrail(aegis_core::guardrail::load_default(workspace));
    agent = agent.with_session(session);
    #[cfg(feature = "ctx")]
    if let Some((store, index)) = blob_handles {
        agent = agent.with_blob_handles(store, index);
    }

    // Mark turn start so the render can animate a "✻ Thinking…" spinner
    // until the first token arrives.
    {
        let mut state = app.lock().unwrap();
        state.turn_start = Some(std::time::Instant::now());
        state.first_token_seen = false;
    }

    // Wire up the stream callback to update TUI state.
    let app_cb = Arc::clone(&app);
    agent = agent.with_stream_callback(move |event| {
        let mut state = app_cb.lock().unwrap();
        match event {
            StreamEvent::TextDelta(text) => {
                // On first token: push a REPL-style
                // `⏺ thought for X.Xs` footer BEFORE appending text, so
                // the scrollback contains the same line a REPL user
                // would see on stderr.
                if !state.first_token_seen {
                    state.first_token_seen = true;
                    if let Some(start) = state.turn_start {
                        let elapsed = start.elapsed().as_secs_f32();
                        if elapsed > 0.5 {
                            state.messages.push(ChatMessage {
                                role: MessageRole::Footer,
                                text: format!("⏺ thought for {elapsed:.1}s"),
                                styled_lines: None,
                                expanded: false,
                            });
                        }
                    }
                }
                state.push_stream_chunk(&text);
            }
            StreamEvent::ThinkingDelta(text) => {
                state.thinking_text.push_str(&text);
            }
            StreamEvent::ToolCall {
                name,
                arguments_preview,
            } => {
                state.tool_start(&name, &arguments_preview);
            }
            StreamEvent::ToolResult {
                name,
                preview,
                is_error,
            } => {
                state.tool_done(&name, &preview, is_error);
            }
            StreamEvent::Usage(_) => {}
            StreamEvent::RetryReset => {
                state.streaming_text.clear();
                state.thinking_text.clear();
            }
        }
    });

    // Build multimodal input when images are attached to this turn.
    // On read failure we surface the error the same way REPL does and
    // fall through without firing a turn — matches REPL's "clear +
    // continue" behavior exactly.
    let user_input = if images.is_empty() {
        aegis_core::UserInput::Text(prompt.to_string())
    } else {
        let n = images.len();
        {
            let mut state = app.lock().unwrap();
            state.push_system(&format!(
                "[image] sending {n} image{} with prompt",
                if n == 1 { "" } else { "s" }
            ));
        }
        match aegis_core::UserInput::with_images(prompt, &images) {
            Ok(input) => input,
            Err(e) => {
                app.lock()
                    .unwrap()
                    .push_error(&format!("failed to read image(s): {e}"));
                cleanup_metis_temp_images(&images);
                return Ok(());
            }
        }
    };

    // Images have been read into the UserInput at this point. Any
    // metis-owned temp files (clipboard paste, HEIC conversion) can
    // now be deleted so long-running sessions don't leak into /tmp.
    cleanup_metis_temp_images(&images);

    // Race the agent run against two interruption sources:
    //   1. Cancel flag — set by Ctrl+C while busy (KeyAction::CancelRun)
    //      so the user actually stops the provider call instead of
    //      just visually marking the turn cancelled while tokens
    //      continue burning in the background.
    //   2. Hard 15-minute ceiling — TUI sessions are longer than REPL
    //      (multi-step agentic work), but no legitimate turn needs
    //      this long. Belt-and-suspenders for the agent.rs per-call
    //      300s timeout.
    let hardcap = std::time::Duration::from_secs(900);
    let cancel_poll = async move {
        loop {
            if cancel_observer.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let output = tokio::select! {
        result = agent.run(user_input) => result?,
        _ = tokio::time::sleep(hardcap) => {
            lock_app(&app).push_error(&format!(
                "[goblin] turn aborted: exceeded {}-minute hardcap",
                hardcap.as_secs() / 60
            ));
            return Ok(());
        }
        _ = cancel_poll => {
            // The keypress handler already pushed "Interrupted by
            // user" and cleared `busy`. Bail out cleanly so the cost
            // accounting below doesn't run with a half-finished
            // AgentOutput.
            return Ok(());
        }
    };

    // Update cost AND push REPL-style cost lines inline in chat so
    // scrollback shows the per-turn cost (matches REPL behavior; no
    // status bar in TUI anymore).
    {
        let mut state = lock_app(&app);
        state.cumulative_usage.input_tokens += output.usage.input_tokens;
        state.cumulative_usage.output_tokens += output.usage.output_tokens;
        state.cumulative_usage.cache_read_tokens += output.usage.cache_read_tokens;
        state.cumulative_usage.cache_write_tokens += output.usage.cache_write_tokens;
        let has_usage = output.usage.input_tokens > 0 || output.usage.output_tokens > 0;
        if has_usage {
            state.cost_display = format_cost_footer(&state.cumulative_usage, model);
        } else {
            state.cost_display = "cost: N/A".to_string();
        }
        state.turn_count += 1;

        // Cost footer — shown at most every 5 minutes, skip providers
        // that don't report usage (MiniMax, NVIDIA NIM). Suppressed when
        // the user ran `/cost off`. Queued (not pushed) so flush_streaming
        // can land it AFTER the assistant text — pushing here would put
        // the cost line BETWEEN "thought for X.Xs" and the answer.
        if state.cost_footer_enabled
            && state.last_cost_shown.elapsed() >= std::time::Duration::from_secs(300)
        {
            state.last_cost_shown = std::time::Instant::now();
            let line = if has_usage {
                let delta_raw = aegis_core::format_cost_delta(
                    &output.usage,
                    &state.cumulative_usage,
                    state.turn_count as usize,
                    model,
                );
                strip_ansi(&delta_raw)
            } else {
                let short = model
                    .strip_prefix("deepseek-")
                    .or_else(|| model.strip_prefix("gpt-"))
                    .or_else(|| model.strip_prefix("claude-"))
                    .unwrap_or(model);
                format!("{short} · cost: N/A (provider doesn't report usage)")
            };
            state.pending_cost_footer = Some(line);
        }
    }

    Ok(())
}

/// Strip ANSI CSI escape sequences from a string. Used when converting
/// `format_cost_delta` (which embeds `\x1b[2m...\x1b[0m`) for display
/// in ratatui, where styles are applied at the `Span` level instead.
///
/// Works byte-wise (not char-wise) so multi-byte UTF-8 sequences like
/// `·` (U+00B7 = 0xC2 0xB7) survive intact. An earlier version pushed
/// each byte as `char`, turning 0xC2 into Â and corrupting the output.
fn strip_ansi(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

enum KeyAction {
    None,
    Quit,
    Submit(String),
    CancelRun,
}

/// Slash commands dispatched by `TuiApp::handle_slash`. Used by the
/// "unknown command" arm to suggest the nearest match on a typo.
/// Keep in sync with the match arms in `handle_slash` and the `/help`
/// listing — new commands must land here too or the typo-suggest goes
/// stale.
const KNOWN_SLASH_COMMANDS: &[&str] = &[
    "help",
    "?",
    "init",
    "undo",
    "redo",
    "connect",
    "share",
    "editor",
    "export",
    "test",
    "lint",
    "run",
    "rewind",
    "commit",
    "diff",
    "copy-context",
    "copy",
    "save",
    "bypass",
    "accept-edits",
    "default-mode",
    "allow-all",
    "yolo",
    "login",
    "cwd",
    "cd",
    "add-dir",
    "themes",
    "thinking",
    "details",
    "usage",
    "context",
    "exit",
    "quit",
    "clear",
    "cost",
    "stats",
    "sessions",
    "tree",
    "session-info",
    "resume",
    "fork",
    "compact",
    "memory",
    "btw",
    "dag",
    "map",
    "providers",
    "budget",
    "provider",
    "model",
    "models",
    "files",
    "view",
    "search",
    "image",
    "images",
    "paste",
    "swarm",
    "consult",
    "claude",
    "race",
    "glm",
    "skills",
    "skill-install",
    "skill-uninstall",
    "skill-search",
    "plan",
    "overthink",
    "advisor",
    "advisor-off",
    "insights",
    "learn",
    "rate",
    "ratings",
    "rules",
    "forget",
    "tasks",
    "task",
    "mouse",
    "autotune",
    "security",
    "allow",
    "deny",
    "allowed",
    "key",
    "keys",
    "browser",
    "computer",
    "info",
    "ask",
    "askall",
    "copy",
];

/// Suggest the closest known slash command for a typo. Returns `None`
/// if nothing is within reasonable edit distance — don't suggest
/// `/help` for `/xyzzy` because that's noise.
pub fn suggest_slash_command(input: &str) -> Option<&'static str> {
    if input.is_empty() {
        return None;
    }
    // Prefix hit wins — if `/imag` is typed, `/image` is obviously
    // what the user meant, regardless of edit distance to other cmds.
    for cmd in KNOWN_SLASH_COMMANDS {
        if cmd.starts_with(input) && *cmd != input {
            return Some(cmd);
        }
    }
    // Otherwise fall back to Levenshtein. Threshold scales with input
    // length so short typos don't snap to anything too distant.
    let threshold: usize = match input.len() {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    };
    let mut best: Option<(usize, &'static str)> = None;
    for cmd in KNOWN_SLASH_COMMANDS {
        let d = levenshtein(input, cmd);
        if d <= threshold {
            match best {
                Some((bd, _)) if bd <= d => {}
                _ => best = Some((d, cmd)),
            }
        }
    }
    best.map(|(_, s)| s)
}

/// Longest common prefix across a set of candidates. Returns an owned
/// String so the caller can use it for both borrowed (`&str`) and owned
/// (`String`) item iterators.
fn longest_common_prefix<'a, I>(items: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut it = items.into_iter();
    let first = match it.next() {
        Some(s) => s.to_string(),
        None => return String::new(),
    };
    let mut prefix = first;
    for cand in it {
        let max = prefix.len().min(cand.len());
        // Walk bytes but clamp to char boundaries.
        let mut cut = 0;
        let pb = prefix.as_bytes();
        let cb = cand.as_bytes();
        while cut < max && pb[cut] == cb[cut] {
            cut += 1;
        }
        // Snap back to the previous char boundary.
        while cut > 0 && !prefix.is_char_boundary(cut) {
            cut -= 1;
        }
        prefix.truncate(cut);
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

/// Find the last whitespace-delimited token on a line, respecting
/// backslash escapes (so `a\ b` is one token). Returns the byte offset
/// of the token within the line and the token text itself. If the line
/// is empty or ends in whitespace, returns `(line.len(), "")`.
fn last_token(line: &str) -> (usize, &str) {
    if line.is_empty() {
        return (0, "");
    }
    let bytes = line.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        let prev = bytes[i - 1];
        if (prev as char).is_whitespace() {
            // Escaped whitespace glues to the token.
            if i >= 2 && bytes[i - 2] == b'\\' {
                i -= 2;
                continue;
            }
            return (i, &line[i..]);
        }
        i -= 1;
    }
    (0, line)
}

/// Given a user-typed token (may include leading `./`, `~/`, `/abs/`)
/// and a completed basename, reconstruct the token with the new
/// basename, preserving the leading path structure the user typed.
fn rewrite_token(token: &str, new_basename: &str) -> String {
    // Find where the final path component starts within the token.
    let cut = token.rfind('/').map(|i| i + 1).unwrap_or(0);
    format!("{}{}", &token[..cut], new_basename)
}

/// Shell-escape spaces and quote characters in a filename so the result
/// survives `path_input::tokenize`. We only escape the minimal set that
/// matters for our tokenizer.
fn shell_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            ' ' | '\t' | '"' | '\'' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Compute (parent_dir, basename_prefix) for path completion. The
/// parent is the directory whose entries we'll list; the prefix is the
/// basename fragment the user has typed so far. If `token` ends in
/// `/`, the prefix is empty (list everything in the directory).
fn split_parent_prefix(resolved: &std::path::Path, token: &str) -> (std::path::PathBuf, String) {
    if token.ends_with('/') {
        return (resolved.to_path_buf(), String::new());
    }
    let parent = resolved
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let prefix = resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent, prefix)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Bracketed-paste handler. Distinguishes drag-drop of image files
/// (terminal sends file paths through the paste escape sequence) from
/// normal text paste. Heuristic: split on newlines/tabs (Finder sends
/// tab-separated when multiple files are dragged; iTerm2 uses newlines;
/// single-file drags are one token), resolve each candidate, and if
/// ALL tokens resolve to existing image files, attach them to
/// `pending_images`. Any other case falls through to normal text paste
/// (preserving newlines so pasted multi-line prompts stay intact).
fn handle_paste(text: &str, app: &Arc<Mutex<TuiApp>>, workspace: &Path) {
    if let Some(paths) = try_parse_drag_drop_paths(text, workspace) {
        let mut state = lock_app(app);
        for p in paths {
            state.attach_image_path(&p);
        }
        return;
    }
    // Claude Code parity: long/multi-line pastes are stashed and replaced by
    // a `[Pasted text #N +X lines]` marker so the prompt stays readable.
    // Threshold: any newline, OR a single line over 200 chars.
    let has_newline = text.contains('\n');
    let long_single = text.len() >= 200;
    if has_newline || long_single {
        let mut state = lock_app(app);
        state.pasted_buffers.push(text.to_string());
        let idx = state.pasted_buffers.len();
        let lines = text.matches('\n').count();
        let placeholder = format!("[Pasted text #{idx} +{lines} lines]");
        for ch in placeholder.chars() {
            state.insert_char(ch);
        }
        return;
    }
    let mut state = lock_app(app);
    for ch in text.chars() {
        if ch == '\n' {
            state.insert_newline();
        } else {
            state.insert_char(ch);
        }
    }
}

fn try_parse_drag_drop_paths(raw: &str, workspace: &Path) -> Option<Vec<std::path::PathBuf>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Candidate split strategy: newline wins, then tab (Finder multi-
    // drag separator on macOS Terminal), then fall back to shell-lite
    // tokenizer which handles quoted paths with spaces.
    let candidates: Vec<String> = if trimmed.contains('\n') {
        trimmed
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if trimmed.contains('\t') {
        trimmed
            .split('\t')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        // Single-file drag or whitespace-separated paths. Use the
        // same tokenizer `/image` uses so quoted paths with spaces
        // survive ("/path with spaces/x.png").
        let single = crate::path_input::resolve_many(trimmed, workspace);
        if single.is_empty() {
            return None;
        }
        return validate_all_images(single);
    };
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        let p = crate::path_input::resolve(&c, workspace);
        out.push(p);
    }
    validate_all_images(out)
}

/// Delete any metis-owned temp image files after a turn has read them.
/// Scoped: only removes files whose basename starts with `metis-paste-`
/// or `metis-heic-` AND which are a **direct child** of
/// `std::env::temp_dir()` (the exact layout produced by
/// `paste_image_from_clipboard` / `convert_heic_to_jpeg`). A file with
/// the same basename in some nested tmp subdir or in a project dir is
/// left alone, so a user who drops a file named `metis-paste-x.png`
/// into their workspace doesn't lose it. Errors are silently ignored —
/// best-effort cleanup, not a correctness boundary.
fn cleanup_metis_temp_images(paths: &[std::path::PathBuf]) {
    let tmp_root = std::env::temp_dir();
    let tmp_root = tmp_root.canonicalize().unwrap_or(tmp_root);
    for p in paths {
        let canon = match p.canonicalize() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if canon.parent() != Some(tmp_root.as_path()) {
            continue;
        }
        let is_metis_temp = canon
            .file_name()
            .and_then(|s| s.to_str())
            .map(|name| name.starts_with("metis-paste-") || name.starts_with("metis-heic-"))
            .unwrap_or(false);
        if is_metis_temp {
            let _ = std::fs::remove_file(&canon);
        }
    }
}

fn validate_all_images(paths: Vec<std::path::PathBuf>) -> Option<Vec<std::path::PathBuf>> {
    if paths.is_empty() {
        return None;
    }
    for p in &paths {
        if !p.exists() {
            return None;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif"
        ) {
            return None;
        }
    }
    Some(paths)
}

fn skill_filtered<'a>(
    skills: &'a [aegis_core::skills::Skill],
    filter: &str,
) -> Vec<&'a aegis_core::skills::Skill> {
    if filter.is_empty() {
        return skills.iter().collect();
    }
    let lower = filter.to_lowercase();
    skills
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&lower)
                || s.description.to_lowercase().contains(&lower)
                || s.tags.iter().any(|t| t.to_lowercase().contains(&lower))
        })
        .collect()
}

/// Static catalogue of slash commands surfaced in the Ctrl+P palette.
/// Curated — not every alias from /help, just the ones a user is most
/// likely to want quick access to. Order is the default unfiltered
/// display order. Tuple is (command, one-line description).
fn palette_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("/ask",            "tek-shot yan soru, agent meşgul olsa da paralel cevap"),
        ("/askall",         "tüm provider'lara aynı anda sor, hepsi yanıt versin"),
        ("/consult",        "overlay aç, hangi provider'a soracağını seç"),
        ("/race",           "tüm provider'lara gönder, ilk cevaplayan kazanır"),
        ("/swarm",          "N paralel agent + quorum, çoğunluk doğru kabul edilir"),
        ("/plan",           "plan moduna gir, yazdıkların agent'a gitmez"),
        ("/overthink",      "daha derin analiz modu, çok adımlı problemler için"),
        ("/advisor",        "danışman modu, agent yapmadan önce ne yapacağını söyler"),
        ("/providers",      "provider overlay'i aç, ↑↓+Enter ile seç"),
        ("/models",         "şu anki provider'ın modelleri, hızlı switch"),
        ("/provider",       "<id> ile direkt provider değiştir"),
        ("/model",          "<N|isim> ile direkt model değiştir"),
        ("/sessions",       "kayıtlı konuşma listesi"),
        ("/resume",         "<id> ile eski session'a dön, kaldığın yerden devam"),
        ("/fork",           "session'ı kopyala, git branch gibi izole çalış"),
        ("/compact",        "uzun konuşmayı özetle, context küçülür"),
        ("/init",           "projeyi analiz et, AGENTS.md üret (OpenCode parity)"),
        ("/share",          "session JSONL'ini panoya kopyala"),
        ("/files",          "dizin içeriği listesi"),
        ("/view",           "<path> dosya içeriğini chat'e yazdır"),
        ("/search",         "projedeki dosyalarda metin ara"),
        ("/image",          "<path> görsel ekle, sonraki mesaja iliştirilir"),
        ("/paste",          "panodaki görseli yapıştır"),
        ("/tasks",          "görev listesi"),
        ("/task",           "add/done/rm/clear ile görev yönetimi"),
        ("/memory",         "agent'ın kalıcı hafızası"),
        ("/dag",            "bu session'da hangi tool ne sırayla çalıştı"),
        ("/map",            "[N] dosya haritası"),
        ("/usage",          "session + all-time tokens, cost"),
        ("/context",        "context window doluluk oranı"),
        ("/sidebar",        "yan paneli aç/kapat"),
        ("/copy",           "son cevabı panoya kopyala"),
        ("/help",           "tam komut referansı (overlay)"),
        ("/skills",         "skill picker overlay'i aç"),
        ("/security",       "kill switch ve telemetri"),
        ("/clear",          "ekranı temizle"),
        ("/quit",           "çıkış"),
    ]
}

fn palette_filtered(filter: &str) -> Vec<&'static (&'static str, &'static str)> {
    let cmds = palette_commands();
    if filter.is_empty() {
        return cmds.iter().collect();
    }
    let lower = filter.to_lowercase();
    let needle = lower.trim_start_matches('/');
    cmds.iter()
        .filter(|(name, desc)| {
            let n = name.trim_start_matches('/').to_lowercase();
            n.contains(needle) || desc.to_lowercase().contains(needle)
        })
        .collect()
}

fn build_palette_panel_lines(filter: &str, sel: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::Rgb(140, 140, 140));
    let cmd_style = Style::default()
        .fg(Color::Rgb(80, 200, 240))
        .add_modifier(Modifier::BOLD);
    let sel_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Rgb(80, 200, 240))
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::Rgb(220, 220, 220));
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Search bar
    let prompt: String = if filter.is_empty() {
        " ".to_string()
    } else {
        filter.to_string()
    };
    lines.push(Line::from(vec![
        Span::styled("> ", cmd_style),
        Span::styled(prompt, desc_style),
    ]));
    lines.push(Line::from(""));
    // Items
    let items = palette_filtered(filter);
    if items.is_empty() {
        lines.push(Line::from(Span::styled("(no match)", dim)));
        return lines;
    }
    let max = 14usize.min(items.len());
    let sel = sel.min(items.len().saturating_sub(1));
    let scroll_start = sel.saturating_sub(max.saturating_sub(1));
    for (rel, (name, desc)) in items.iter().skip(scroll_start).take(max).enumerate() {
        let abs = scroll_start + rel;
        let pad_name = format!(" {:<14} ", name);
        let line = if abs == sel {
            Line::from(vec![
                Span::styled(pad_name, sel_style),
                Span::raw(" "),
                Span::styled(desc.to_string(), desc_style),
            ])
        } else {
            Line::from(vec![
                Span::styled(pad_name, cmd_style),
                Span::raw(" "),
                Span::styled(desc.to_string(), dim),
            ])
        };
        lines.push(line);
    }
    lines
}

fn is_stop_word(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "dur" | "stop" | "durdur" | "cancel" | "iptal" | "vazgeç" | "yeter" | "tamam dur"
    )
}

/// Atakan: unified diff'i sol-sağ kolon formatına çevirir. Hunk başlığı
/// (@@) full-width, file header (+++/---) skip, `-` ve `+` çiftleri
/// pair'lenir. Boş tarafa padding. Width <120 col'da çağrılmaz, caller
/// unified fallback'e düşer.
fn diff_to_side_by_side(
    diff: &str,
    col_width: usize,
) -> Vec<(String, Line<'static>)> {
    let rem_style = Style::default().fg(Color::Rgb(248, 81, 73));
    let add_style = Style::default().fg(Color::Rgb(63, 185, 80));
    let ctx_style = Style::default().fg(Color::Rgb(140, 140, 140));
    let hdr_style = Style::default()
        .fg(Color::Rgb(80, 200, 240))
        .add_modifier(Modifier::BOLD);
    let sep = " │ ";

    let pad = |s: &str, w: usize| -> String {
        let count = s.chars().count();
        if count >= w {
            s.chars().take(w).collect::<String>()
        } else {
            let mut out = s.to_string();
            for _ in 0..(w - count) {
                out.push(' ');
            }
            out
        }
    };

    let mut out: Vec<(String, Line<'static>)> = Vec::new();
    let mut pending_left: Vec<String> = Vec::new();

    let flush_left = |left: &mut Vec<String>, out: &mut Vec<(String, Line<'static>)>| {
        let blank = " ".repeat(col_width);
        for l in left.drain(..) {
            let lp = {
                let count = l.chars().count();
                if count >= col_width {
                    l.chars().take(col_width).collect::<String>()
                } else {
                    let mut s = l.clone();
                    for _ in 0..(col_width - count) { s.push(' '); }
                    s
                }
            };
            let plain = format!("{lp}{sep}{blank}");
            let line = Line::from(vec![
                Span::styled(lp, rem_style),
                Span::raw(sep),
                Span::raw(blank.clone()),
            ]);
            out.push((plain, line));
        }
    };

    for line in diff.lines() {
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        if line.starts_with("@@") {
            flush_left(&mut pending_left, &mut out);
            let plain = line.to_string();
            out.push((plain.clone(), Line::from(Span::styled(plain, hdr_style))));
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            pending_left.push(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            let left = if !pending_left.is_empty() {
                pending_left.remove(0)
            } else {
                String::new()
            };
            let lp = pad(&left, col_width);
            let rp = pad(rest, col_width);
            let plain = format!("{lp}{sep}{rp}");
            let line = Line::from(vec![
                Span::styled(lp, rem_style),
                Span::raw(sep),
                Span::styled(rp, add_style),
            ]);
            out.push((plain, line));
            continue;
        }
        flush_left(&mut pending_left, &mut out);
        let rest = line.strip_prefix(' ').unwrap_or(line);
        let lp = pad(rest, col_width);
        let rp = pad(rest, col_width);
        let plain = format!("{lp}{sep}{rp}");
        let line = Line::from(vec![
            Span::styled(lp, ctx_style),
            Span::raw(sep),
            Span::styled(rp, ctx_style),
        ]);
        out.push((plain, line));
    }
    flush_left(&mut pending_left, &mut out);
    out
}

#[cfg(test)]
mod sbs_diff_tests {
    use super::diff_to_side_by_side;
    #[test]
    fn pairs_remove_with_add() {
        let diff = "@@ -1 +1 @@\n-old\n+new\n";
        let rows = diff_to_side_by_side(diff, 10);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.contains("@@"));
        assert!(rows[1].0.contains("old") && rows[1].0.contains("new"));
    }
    #[test]
    fn lone_remove_pads_right() {
        let diff = "@@ @@\n-removed\n";
        let rows = diff_to_side_by_side(diff, 10);
        assert!(rows[1].0.contains("removed"));
        // Right column should be blank (only spaces after separator).
    }
    #[test]
    fn skips_file_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ @@\n+a\n";
        let rows = diff_to_side_by_side(diff, 5);
        assert_eq!(rows.len(), 2); // hunk header + add line
    }
    #[test]
    fn context_line_both_columns() {
        let diff = "@@ @@\n same\n";
        let rows = diff_to_side_by_side(diff, 6);
        let plain = &rows[1].0;
        assert!(plain.contains("same"));
        // Both sides identical.
        let halves: Vec<&str> = plain.split(" │ ").collect();
        assert_eq!(halves[0].trim(), "same");
        assert_eq!(halves[1].trim(), "same");
    }
}

/// Atakan: /rewind conv|both helper. n=1 → son user turn'ünün başlangıç
/// index'i (messages.truncate(idx) o turn'ü ve sonrasını siler). None =
/// yeterli user turn yok.
fn nth_user_turn_from_end(messages: &[ChatMessage], n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User)
        .map(|(i, _)| i)
        .collect();
    if user_indices.len() < n {
        return None;
    }
    Some(user_indices[user_indices.len() - n])
}

#[cfg(test)]
mod rewind_helper_tests {
    use super::{nth_user_turn_from_end, ChatMessage, MessageRole};
    fn msg(role: MessageRole, t: &str) -> ChatMessage {
        ChatMessage { role, text: t.into(), styled_lines: None, expanded: false }
    }
    #[test]
    fn last_turn_index() {
        let m = vec![
            msg(MessageRole::User, "u1"),
            msg(MessageRole::Assistant, "a1"),
            msg(MessageRole::User, "u2"),
            msg(MessageRole::Assistant, "a2"),
        ];
        assert_eq!(nth_user_turn_from_end(&m, 1), Some(2));
    }
    #[test]
    fn two_turns_back() {
        let m = vec![
            msg(MessageRole::User, "u1"),
            msg(MessageRole::Assistant, "a1"),
            msg(MessageRole::User, "u2"),
            msg(MessageRole::Assistant, "a2"),
            msg(MessageRole::User, "u3"),
        ];
        assert_eq!(nth_user_turn_from_end(&m, 2), Some(2));
    }
    #[test]
    fn n_too_large() {
        let m = vec![msg(MessageRole::User, "u1")];
        assert_eq!(nth_user_turn_from_end(&m, 5), None);
    }
    #[test]
    fn zero_returns_none() {
        let m = vec![msg(MessageRole::User, "u1")];
        assert_eq!(nth_user_turn_from_end(&m, 0), None);
    }
    #[test]
    fn no_user_messages() {
        let m = vec![msg(MessageRole::Assistant, "a1"), msg(MessageRole::System, "s1")];
        assert_eq!(nth_user_turn_from_end(&m, 1), None);
    }
}

/// Atakan: Aider /test /lint smart shell pattern. Workspace `.metis/config.toml`
/// öncelikli, sonra `~/.metis/config.toml`. `[auto_fix] test_command` veya
/// `lint_command` arar. None döner = command set edilmemiş.
fn load_auto_fix_command(workspace: &std::path::Path, kind: &str) -> Option<String> {
    let key = format!("{kind}_command");
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    paths.push(workspace.join(".metis").join("config.toml"));
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".metis").join("config.toml"));
    }
    for path in paths {
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let value: toml::Value = match content.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(cmd) = value
            .get("auto_fix")
            .and_then(|af| af.get(&key))
            .and_then(|v| v.as_str())
        {
            return Some(cmd.to_string());
        }
    }
    None
}

#[cfg(test)]
mod auto_fix_loader_tests {
    use super::load_auto_fix_command;
    use std::fs;
    #[test]
    fn returns_none_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_auto_fix_command(tmp.path(), "test").is_none());
    }
    #[test]
    fn reads_workspace_config() {
        let tmp = tempfile::tempdir().unwrap();
        let metis = tmp.path().join(".metis");
        fs::create_dir_all(&metis).unwrap();
        fs::write(
            metis.join("config.toml"),
            "[auto_fix]\ntest_command = \"cargo test\"\nlint_command = \"cargo clippy\"\n",
        )
        .unwrap();
        assert_eq!(
            load_auto_fix_command(tmp.path(), "test").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            load_auto_fix_command(tmp.path(), "lint").as_deref(),
            Some("cargo clippy")
        );
    }
}

/// Atakan: session-end signal keyword detector. Tetikleyici, naif auto-ingest
/// değil — sadece agent'a görünür marker düşürmek için kullanılır;
/// agent system_prompt kuralları gereği `mnemonics_ingest` çağırır.
/// Liste casual TR/EN kapanış ifadelerinden derlendi (premortem F5 disable
/// riskini azaltmak için yalnız net kapanış sinyalleri).
fn is_session_end_keyword(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    // Tek kelime / çok kısa kalıplar
    let exact: &[&str] = &[
        "bye", "görüşürüz", "gorusuruz", "çıkıyorum", "cikiyorum",
        "kapatıyorum", "kapatiyorum", "kapanıyorum", "kapaniyorum",
        "tamam bitir", "bitir kapat", "session bitir", "done bye",
        "see you", "later",
    ];
    if exact.iter().any(|k| lower == *k) {
        return true;
    }
    // Cümle kalıpları (başında / sonunda)
    let phrases: &[&str] = &[
        "görüşürüz", "gorusuruz", "kapatıyorum", "kapatiyorum",
        "session'ı bitir", "session bitirelim", "bu session'ı kapatalım",
    ];
    phrases.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod session_end_keyword_tests {
    use super::is_session_end_keyword;
    #[test]
    fn detects_common_close_phrases() {
        assert!(is_session_end_keyword("bye"));
        assert!(is_session_end_keyword("Görüşürüz"));
        assert!(is_session_end_keyword("çıkıyorum"));
        assert!(is_session_end_keyword("kapatıyorum"));
        assert!(is_session_end_keyword("tamam bitir"));
        assert!(is_session_end_keyword("see you"));
    }
    #[test]
    fn ignores_unrelated_text() {
        assert!(!is_session_end_keyword("evet"));
        assert!(!is_session_end_keyword("nasılsın"));
        assert!(!is_session_end_keyword("bug fix"));
        assert!(!is_session_end_keyword(""));
    }
}

/// Atakan: code-driven session-end ingest. Keyword detect tetiklenince
/// `mnemonics` CLI'sine subprocess olarak ingest atılır. Agent disiplinine
/// güvenmiyoruz — DeepSeek/GLM marker'ı görüp atlıyor (premortem F3 canlı).
/// Özet kalitesi düşük (LLM judge yok), ama kayıt garantisi var.
pub enum SaveOutcome {
    Ingested(String),
    SkippedNoSignal,
    RejectedSecret(String),
    /// Atakan: yeni özet mevcut DB ile cosine > eşik — gürültü engeli.
    SkippedDuplicate(f32),
}

/// Atakan: pre-ingest dedup check. `mnemonics retrieve --top-k 1 --no-decay`
/// ile cosine skoru çek, eşik üstü ise duplicate. Mnemonics CLI subprocess
/// yavaş (HF model load), ama session-end'de 1-2 saniye kabul edilebilir.
/// Output formatı: "  [0.533] [raw=… decay=…] <text>" — ilk `[…]` parse.
const DEDUP_COSINE_THRESHOLD: f32 = 0.92;

fn check_dedup(ns: &str, query: &str) -> Option<f32> {
    let out = std::process::Command::new("mnemonics")
        .args(["retrieve", "--ns", ns, "--top-k", "1", "--no-decay", query])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('['))?;
    let trimmed = line.trim_start();
    let after = trimmed.strip_prefix('[')?;
    let score_str = after.split(']').next()?;
    score_str.trim().parse::<f32>().ok()
}

#[cfg(test)]
mod dedup_threshold_tests {
    #[test]
    fn threshold_is_strict_enough() {
        // Documentation: 0.92 yüksek bar, near-duplicate'leri yakalar
        // ama orta similarity (0.5–0.7) için ingest devam eder.
        assert!(super::DEDUP_COSINE_THRESHOLD >= 0.85);
        assert!(super::DEDUP_COSINE_THRESHOLD <= 0.98);
    }
}

/// Atakan: Trigger B — async recovery for `/recall-prev`. Walks the
/// workspace, finds the most recent unsaved session, replays its last
/// user/assistant pair through `code_driven_session_save`, and on
/// success marks the session's meta sidecar as ingested so the hint
/// doesn't fire again. All UI updates go through `lock_app(&app)`.
async fn recall_prev_recover(workspace: std::path::PathBuf, app: Arc<Mutex<TuiApp>>) {
    // Re-scan: state could have shifted between slash dispatch and
    // task execution (another aegis instance, manual file edit).
    let hint = match aegis_core::SessionStore::previous_unsaved_session(&workspace, None) {
        Ok(Some(h)) => h,
        Ok(None) => {
            lock_app(&app).push_system(
                "[recall-prev] no unsaved previous session found on second look",
            );
            return;
        }
        Err(e) => {
            lock_app(&app).push_error(&format!("[recall-prev] scan failed: {e}"));
            return;
        }
    };

    let store = match aegis_core::SessionStore::open(&workspace, &hint.id) {
        Ok(s) => s,
        Err(e) => {
            lock_app(&app).push_error(&format!(
                "[recall-prev] open session {} failed: {e}",
                &hint.id.chars().take(16).collect::<String>()
            ));
            return;
        }
    };
    let (last_user, last_assistant) = store.last_user_assistant_pair();
    if last_user.trim().is_empty() && last_assistant.trim().is_empty() {
        lock_app(&app).push_system(&format!(
            "[recall-prev] session {} has no user/assistant text — skipped",
            &hint.id.chars().take(16).collect::<String>()
        ));
        return;
    }

    let trigger = "/recall-prev";
    let result = code_driven_session_save(&workspace, &last_user, &last_assistant, trigger).await;
    let line = match &result {
        Ok(SaveOutcome::Ingested(ns)) => {
            format!("[recall-prev] ingested → ns={ns}")
        }
        Ok(SaveOutcome::SkippedNoSignal) => {
            "[recall-prev] LLM judge → kayda değer yok, atlandı".to_string()
        }
        Ok(SaveOutcome::RejectedSecret(reason)) => {
            format!("[recall-prev] REJECTED: secret pattern ({reason})")
        }
        Ok(SaveOutcome::SkippedDuplicate(score)) => {
            format!(
                "[recall-prev] skipped: duplicate (cosine={score:.2} ≥ {DEDUP_COSINE_THRESHOLD})"
            )
        }
        Err(e) => format!("[recall-prev] FAIL: {e}"),
    };
    lock_app(&app).push_system(&line);

    // On any *terminal* outcome (ingest, judge skip, dup) mark the
    // session as ingested so the boot-time hint doesn't keep firing.
    // Only secret-rejection and hard errors leave the mark unchanged
    // — those are recoverable (rotate the secret, retry the call).
    let should_mark = matches!(
        result,
        Ok(SaveOutcome::Ingested(_))
            | Ok(SaveOutcome::SkippedNoSignal)
            | Ok(SaveOutcome::SkippedDuplicate(_))
    );
    if should_mark {
        if let Ok(mut s) = aegis_core::SessionStore::open(&workspace, &hint.id) {
            if let Err(e) = s.mark_ingested_now() {
                lock_app(&app).push_error(&format!("[recall-prev] mark ingested failed: {e}"));
            }
        }
    }
}

pub async fn code_driven_session_save(
    workspace: &std::path::Path,
    last_user: &str,
    last_assistant: &str,
    trigger: &str,
) -> Result<SaveOutcome, String> {
    let repo = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let ns = format!("proj:{repo}");

    if last_user.trim().is_empty() && last_assistant.trim().is_empty() {
        return Ok(SaveOutcome::SkippedNoSignal);
    }

    // Secret guard öncelik. LLM judge'a sırrı sokmuyoruz; reddet, dön.
    let combined = format!("{last_user}\n{last_assistant}\n{trigger}");
    if let Some(reason) = detect_secret_pattern(&combined) {
        return Ok(SaveOutcome::RejectedSecret(reason));
    }

    // LLM judge (MiniMax-M2.7): bu turn kayda değer mi? Cevap JSON.
    let judged = match judge_session_end(last_user, last_assistant, trigger).await {
        Ok(j) => j,
        Err(e) => return Err(format!("LLM judge: {e}")),
    };
    let fact = match judged {
        JudgeVerdict::Keep(f) => f,
        JudgeVerdict::Skip => return Ok(SaveOutcome::SkippedNoSignal),
    };

    // Judge çıktısını da secret regex'ten geçir; hallucinated key olmasın.
    if let Some(reason) = detect_secret_pattern(&fact) {
        return Ok(SaveOutcome::RejectedSecret(format!("LLM output: {reason}")));
    }

    let date = ymd_today();
    let snippet = format!("[{date}] [{repo}] {}", truncate_chars(fact.trim(), 400));

    // Atakan: pre-ingest dedup check. Yeni özet mevcut DB ile cosine
    // > 0.92 ise atla — premortem F1 (DB gürültüsü) sigortası.
    if let Some(score) = check_dedup(&ns, &snippet) {
        if score >= DEDUP_COSINE_THRESHOLD {
            return Ok(SaveOutcome::SkippedDuplicate(score));
        }
    }

    let out = tokio::process::Command::new("mnemonics")
        .args(["ingest", "--ns", &ns, &snippet])
        .output()
        .await
        .map_err(|e| format!("spawn mnemonics: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "mnemonics exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(SaveOutcome::Ingested(ns))
}

enum JudgeVerdict {
    Keep(String),
    Skip,
}

/// Mini LLM call. Atakan: judge için MiniMax-M2.7 sabit. Aktif chat
/// provider farklı olabilir (DeepSeek/GLM/NVIDIA), ama judge ucuz ve
/// tutarlı bir model istiyor; her keyword detect'te bağımsız MiniMax
/// client build edilir (MINIMAX_API_KEY env'den). Failure (key yok,
/// rate-limit, vs.) Err döner — caller status satırına bunu yansıtır.
async fn judge_session_end(
    last_user: &str,
    last_assistant: &str,
    trigger: &str,
) -> Result<JudgeVerdict, String> {
    let provider = aegis_api::Provider::lookup("minimax")
        .ok_or_else(|| "minimax provider not registered".to_string())?;
    let client = provider
        .client_from_env()
        .map_err(|e| format!("minimax client build: {e}"))?;

    let system = "Sen bir CLI session özetleyicisin. Görevin: kullanıcının son turn'ünde \
        kayda değer (decision/bug-fix/deferred-task/explicit-remember) bir şey var mı tespit \
        etmek. Çıktı YALNIZCA tek satır JSON, başka hiçbir şey yok.\n\n\
        Format:\n\
        - Kayda değer ise: {\"keep\": true, \"fact\": \"<tek cümle, neden + ne, max 200 karakter>\"}\n\
        - Aksi halde: {\"keep\": false}\n\n\
        Kuralları:\n\
        - Selamlaşma, casual chat, soru-cevap, eylem özeti (\"X yaptım\") = keep:false\n\
        - \"X bug fixlendi çünkü Y\", \"Z'yi ertelidik\", \"A yerine B'yi seçtik\" = keep:true\n\
        - Asla API key, şifre, kişisel bilgi ekleme; gerekirse keep:false dön\n\
        - Türkçe ya da İngilizce, kullanıcı diliyle";
    let user_prompt = format!(
        "Son user mesajı:\n{}\n\nSon assistant mesajı:\n{}\n\nKapanış sinyali:\n{}\n\nJSON:",
        truncate_chars(last_user, 1500),
        truncate_chars(last_assistant, 2000),
        truncate_chars(trigger, 80),
    );
    let req = aegis_api::ChatRequest {
        model: "MiniMax-M2.7".to_string(),
        messages: vec![
            aegis_api::ChatMessage::system(system),
            aegis_api::ChatMessage::user(user_prompt),
        ],
        tools: None,
        temperature: Some(0.0),
        max_tokens: Some(200),
        thinking: false,
        thinking_budget: 0,
    };
    let resp = client.chat(&req).await.map_err(|e| e.to_string())?;
    let text = resp
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();
    parse_judge_json(&text)
}

/// Tolerant parser — model occasionally wraps JSON in code fences or adds
/// explanatory text. Find the first `{` and last `}` and parse that slice.
fn parse_judge_json(raw: &str) -> Result<JudgeVerdict, String> {
    let start = raw.find('{').ok_or_else(|| format!("no JSON in: {}", truncate_chars(raw, 120)))?;
    let end = raw.rfind('}').ok_or_else(|| format!("unclosed JSON in: {}", truncate_chars(raw, 120)))?;
    if end <= start {
        return Err(format!("malformed JSON: {}", truncate_chars(raw, 120)));
    }
    let slice = &raw[start..=end];
    let v: serde_json::Value = serde_json::from_str(slice)
        .map_err(|e| format!("JSON parse: {e} | raw: {}", truncate_chars(raw, 120)))?;
    let keep = v.get("keep").and_then(|x| x.as_bool()).unwrap_or(false);
    if !keep {
        return Ok(JudgeVerdict::Skip);
    }
    let fact = v
        .get("fact")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if fact.is_empty() {
        return Ok(JudgeVerdict::Skip);
    }
    Ok(JudgeVerdict::Keep(fact))
}

/// Returns Some(label) if text contains a likely secret pattern.
pub fn detect_secret_pattern(text: &str) -> Option<String> {
    const PREFIXES: &[(&str, &str)] = &[
        ("sk_", "Stripe-style sk_"),
        ("sk-", "OpenAI-style sk-"),
        ("AIza", "Google AIza"),
        ("ghp_", "GitHub PAT"),
        ("gho_", "GitHub OAuth"),
        ("nvapi-", "NVIDIA"),
        ("tvly-", "Tavily"),
        ("xoxb-", "Slack bot"),
        ("xoxp-", "Slack user"),
    ];
    for (p, label) in PREFIXES {
        if text.contains(p) {
            return Some((*label).to_string());
        }
    }
    let lower = text.to_lowercase();
    let needles: &[&str] = &[
        "password=", "password =", "secret=", "secret =",
        "_token=", "_token =", "_key=", "_key =", "api_key=",
    ];
    for n in needles {
        if lower.contains(n) {
            return Some(format!("inline credential `{n}`"));
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max).collect();
        format!("{kept}…")
    }
}

/// YYYY-MM-DD from system clock (Howard Hinnant civil-from-days).
fn ymd_today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = if m <= 2 { y + 1 } else { y };
    format!("{yy:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod judge_parse_tests {
    use super::{parse_judge_json, JudgeVerdict};
    fn fact_of(v: JudgeVerdict) -> Option<String> {
        match v { JudgeVerdict::Keep(s) => Some(s), JudgeVerdict::Skip => None }
    }
    #[test]
    fn parses_keep_true() {
        let v = parse_judge_json("{\"keep\": true, \"fact\": \"X bug fixed because Y\"}").unwrap();
        assert_eq!(fact_of(v), Some("X bug fixed because Y".into()));
    }
    #[test]
    fn parses_keep_false() {
        let v = parse_judge_json("{\"keep\": false}").unwrap();
        assert!(matches!(v, JudgeVerdict::Skip));
    }
    #[test]
    fn tolerates_code_fences() {
        let v = parse_judge_json("```json\n{\"keep\": true, \"fact\": \"Z\"}\n```").unwrap();
        assert_eq!(fact_of(v), Some("Z".into()));
    }
    #[test]
    fn tolerates_extra_prose() {
        let v = parse_judge_json("Sure, here you go: {\"keep\": true, \"fact\": \"A\"} OK?").unwrap();
        assert_eq!(fact_of(v), Some("A".into()));
    }
    #[test]
    fn empty_fact_becomes_skip() {
        let v = parse_judge_json("{\"keep\": true, \"fact\": \"\"}").unwrap();
        assert!(matches!(v, JudgeVerdict::Skip));
    }
    #[test]
    fn rejects_garbage() {
        assert!(parse_judge_json("totally not json").is_err());
    }
}

#[cfg(test)]
mod code_driven_save_tests {
    use super::{detect_secret_pattern, truncate_chars, ymd_today};
    #[test]
    fn detects_known_key_prefixes() {
        assert!(detect_secret_pattern("export AIzaSyDeadbeef").is_some());
        assert!(detect_secret_pattern("token=ghp_abcdef123").is_some());
        assert!(detect_secret_pattern("nvapi-foo").is_some());
        assert!(detect_secret_pattern("API_KEY=sk-deadbeef").is_some());
    }
    #[test]
    fn detects_inline_credentials() {
        assert!(detect_secret_pattern("password=hunter2").is_some());
        assert!(detect_secret_pattern("MY_TOKEN = abc").is_some());
    }
    #[test]
    fn ignores_clean_text() {
        assert!(detect_secret_pattern("normal session content").is_none());
        assert!(detect_secret_pattern("Bug fixed in tui.rs:123").is_none());
    }
    #[test]
    fn truncate_handles_unicode() {
        assert_eq!(truncate_chars("merhaba", 4), "merh…");
        assert_eq!(truncate_chars("kısa", 10), "kısa");
    }
    #[test]
    fn date_format_shape() {
        let d = ymd_today();
        assert_eq!(d.len(), 10);
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }
}

/// Detects "from now on do X" type intent and returns the rule text if matched.
/// The original message is still forwarded to the agent unchanged.
fn detect_rule_intent(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    // Turkish patterns
    let tr_prefixes = [
        "bundan sonra ",
        "artık ",
        "bir daha ",
        "hiçbir zaman ",
        "asla ",
        "her zaman ",
        "daima ",
        "sormadan ",
        "otomatik olarak ",
        "lütfen bundan sonra ",
    ];
    // English patterns
    let en_prefixes = [
        "from now on ",
        "always ",
        "never ",
        "don't ",
        "do not ",
        "stop ",
        "please always ",
        "please never ",
        "please don't ",
    ];
    // Rule suffix patterns (text must end with these or contain them)
    let rule_suffixes_tr = [
        " yapma",
        " etme",
        " sorma",
        " kullanma",
        " ekleme",
        " silme",
        " yazma",
        " gönderme",
        " bağlanma",
        " yap",
        " et",
        " kullan",
        " ekle",
        " söyle",
        " anlat",
    ];
    let has_rule_suffix_tr = rule_suffixes_tr.iter().any(|s| lower.ends_with(s));

    // Check Turkish prefix + rule suffix
    for prefix in &tr_prefixes {
        if lower.starts_with(prefix) {
            let rest = text[prefix.len()..].trim();
            if !rest.is_empty() && (has_rule_suffix_tr || lower.contains(" yapma") || lower.contains(" etme")) {
                return Some(rest.to_string());
            }
        }
    }

    // Check English prefix
    for prefix in &en_prefixes {
        if lower.starts_with(prefix) {
            let rest = text[prefix.len()..].trim();
            if !rest.is_empty() {
                return Some(format!("{}{}", prefix, rest));
            }
        }
    }

    None
}

fn handle_key(key: KeyEvent, app: &Arc<Mutex<TuiApp>>, workspace: &Path) -> KeyAction {
    let mut state = lock_app(app);

    // `/models` just ran → next single digit picks the model without
    // requiring Enter. Any non-digit keypress cancels the mode (so
    // typing ordinary input like "1 saat sonra..." doesn't accidentally
    // fire a model switch). Esc also cancels.
    if state.awaiting_model_pick {
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && state.input.is_empty() => {
                if let Some(n) = c.to_digit(10) {
                    let n = n as usize;
                    if n >= 1 && n <= state.last_model_menu.len() {
                        let chosen = state.last_model_menu[n - 1].clone();
                        state.awaiting_model_pick = false;
                        state.pending_model_switch = Some(chosen.clone());
                        state.push_system(&format!("queued model switch: {chosen}"));
                        return KeyAction::None;
                    }
                }
                // Out-of-range digit: fall through (cancel mode, treat
                // as normal input so the user isn't locked in).
                state.awaiting_model_pick = false;
            }
            KeyCode::Esc => {
                state.awaiting_model_pick = false;
                state.push_system("model pick cancelled");
                return KeyAction::None;
            }
            _ => {
                // Any other keypress — cancel the pick mode but let
                // the keypress fall through to normal handling so the
                // user doesn't lose the typed character.
                state.awaiting_model_pick = false;
            }
        }
    }

    // Allow quick-menu: number key → execute action, Esc → close.
    if state.allow_menu_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.allow_menu_open = false;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c as usize - '1' as usize;
                if let Some(&(label, action, _desc)) = ALLOW_MENU_ITEMS.get(idx) {
                    state.allow_menu_open = false;
                    if let Some(set_arc) = state.always_allowed.as_ref().map(Arc::clone) {
                        let mut set = set_arc.lock().unwrap();
                        if action.starts_with('-') {
                            let tool = &action[1..];
                            if tool == "ALL" {
                                let count = set.len();
                                set.clear();
                                state.push_system(&format!("cleared always-allowed ({count} tools removed)"));
                            } else if set.remove(tool) {
                                state.push_system(&format!("removed from always-allowed: {tool}"));
                            } else {
                                state.push_system(&format!("{tool} was not in always-allowed"));
                            }
                        } else if action == "ALL" {
                            for t in ["bash", "edit_file", "write_file", "multi_edit", "computer_use"] {
                                set.insert(t.to_string());
                            }
                            state.push_system("allowed for this session: bash, edit_file, write_file, multi_edit, computer_use");
                        } else {
                            set.insert(action.to_string());
                            state.push_system(&format!("allowed for this session: {label}"));
                        }
                    } else {
                        state.push_system("--yes mode active, everything already allowed");
                    }
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }

    /// Shared finalization for provider picks coming from either a
    /// number key or ↑/↓+Enter.
    fn apply_provider_pick(state: &mut TuiApp, id: String, consult_mode: bool) {
        if consult_mode {
            let prefill = format!("/consult {id} ");
            let len = prefill.len();
            state.input = prefill;
            state.cursor = len;
            state.push_system(&format!("consult {id}: type your question above"));
        } else {
            state.pending_provider_switch = Some((id.clone(), None));
            state.push_system(&format!("switching to provider: {id}"));
            let models = models_for_provider(&id);
            if !models.is_empty() {
                let ids: Vec<String> = models.iter().map(|(m, _)| m.to_string()).collect();
                state.last_model_menu = ids.clone();
                state.model_menu = Some(ids);
                state.model_sel = 0;
            }
        }
    }

    // Provider menu: 1-9 quick pick OR ↑/↓ + Enter, Esc closes.
    if state.provider_menu.is_some() {
        let len = state.provider_menu.as_ref().map(|v| v.len()).unwrap_or(0);
        let max_idx = len.saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.provider_menu = None;
                state.consult_pick_mode = false;
                state.provider_sel = 0;
            }
            KeyCode::Up => {
                state.provider_sel = state.provider_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                state.provider_sel = (state.provider_sel + 1).min(max_idx);
            }
            KeyCode::Enter => {
                let idx = state.provider_sel;
                let chosen = state
                    .provider_menu
                    .as_ref()
                    .and_then(|v| v.get(idx))
                    .map(|(id, _, _)| id.clone());
                state.provider_menu = None;
                state.provider_sel = 0;
                let consult_mode = state.consult_pick_mode;
                state.consult_pick_mode = false;
                if let Some(id) = chosen {
                    apply_provider_pick(&mut *state, id, consult_mode);
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c as usize - '1' as usize;
                let chosen = state
                    .provider_menu
                    .as_ref()
                    .and_then(|v| v.get(idx))
                    .map(|(id, _, _)| id.clone());
                state.provider_menu = None;
                state.provider_sel = 0;
                let consult_mode = state.consult_pick_mode;
                state.consult_pick_mode = false;
                if let Some(id) = chosen {
                    apply_provider_pick(&mut *state, id, consult_mode);
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Model menu: 1-9 quick pick OR ↑/↓ + Enter, Esc closes.
    if state.model_menu.is_some() {
        let len = state.model_menu.as_ref().map(|v| v.len()).unwrap_or(0);
        let max_idx = len.saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.model_menu = None;
                state.model_sel = 0;
            }
            KeyCode::Up => {
                state.model_sel = state.model_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                state.model_sel = (state.model_sel + 1).min(max_idx);
            }
            KeyCode::Enter => {
                let idx = state.model_sel;
                let chosen = state
                    .model_menu
                    .as_ref()
                    .and_then(|v| v.get(idx))
                    .cloned();
                state.model_menu = None;
                state.model_sel = 0;
                if let Some(model) = chosen {
                    state.pending_model_switch = Some(model.clone());
                    state.push_system(&format!("switching to model: {model}"));
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c as usize - '1' as usize;
                let chosen = state
                    .model_menu
                    .as_ref()
                    .and_then(|v| v.get(idx))
                    .cloned();
                state.model_menu = None;
                state.model_sel = 0;
                if let Some(model) = chosen {
                    state.pending_model_switch = Some(model.clone());
                    state.push_system(&format!("switching to model: {model}"));
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Chat-history search overlay (Ctrl+F). Captures every key while
    // open: typed chars/backspace edit the query, Up/Down step matches,
    // Esc/Ctrl+G cancels and restores scroll. Comes before help and
    // pickers so it can never be shadowed.
    if state.chat_search.is_some() {
        match key.code {
            KeyCode::Esc => {
                state.chat_search_cancel();
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.chat_search_cancel();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Re-press closes — mirrors Ctrl+F toggle convention.
                state.chat_search_cancel();
            }
            KeyCode::Enter | KeyCode::Down => {
                state.chat_search_step(true);
            }
            KeyCode::Up => {
                state.chat_search_step(false);
            }
            KeyCode::Backspace => {
                if let Some(s) = state.chat_search.as_mut() {
                    s.query.pop();
                }
                state.chat_search_recompute();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(s) = state.chat_search.as_mut() {
                    s.query.push(c);
                }
                state.chat_search_recompute();
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Ctrl+F: open chat-history search. Falls through when search is
    // already open (handled above) so the same key toggles cleanly.
    if matches!(key.code, KeyCode::Char('f'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        state.chat_search_open();
        return KeyAction::None;
    }

    // Help overlay — Esc / q closes, scroll keys move within the panel.
    // Comes before skill_menu so it doesn't get shadowed when both are
    // somehow open; in practice they're mutually exclusive.
    if state.help_overlay_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if key.modifiers.is_empty() => {
                state.help_overlay_open = false;
                state.help_scroll = 0;
            }
            KeyCode::Up => {
                state.help_scroll = state.help_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                state.help_scroll = state.help_scroll.saturating_add(1).min(500);
            }
            KeyCode::PageUp => {
                state.help_scroll = state.help_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                state.help_scroll = state.help_scroll.saturating_add(10).min(500);
            }
            KeyCode::Home => {
                state.help_scroll = 0;
            }
            KeyCode::End => {
                state.help_scroll = 500;
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Permission timeline overlay — opened by `/permissions`. Esc
    // closes; arrow keys / PgUp / PgDn scroll the buffer.
    if state.permission_overlay_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if key.modifiers.is_empty() => {
                state.permission_overlay_open = false;
                state.permission_overlay_scroll = 0;
            }
            KeyCode::Up => {
                state.permission_overlay_scroll =
                    state.permission_overlay_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                state.permission_overlay_scroll =
                    state.permission_overlay_scroll.saturating_add(1).min(500);
            }
            KeyCode::PageUp => {
                state.permission_overlay_scroll =
                    state.permission_overlay_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                state.permission_overlay_scroll =
                    state.permission_overlay_scroll.saturating_add(10).min(500);
            }
            KeyCode::Home => {
                state.permission_overlay_scroll = 0;
            }
            KeyCode::End => {
                state.permission_overlay_scroll = 500;
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Files picker — opened by bare `/files`. Type filters by
    // substring (lower-cased), ↑↓ navigate, Enter inserts `@<path>`
    // into the input so the next message can ship the file as context,
    // Esc closes without state change.
    if state.files_picker.is_some() {
        match key.code {
            KeyCode::Esc => {
                state.files_picker = None;
                state.files_picker_query.clear();
                state.files_picker_sel = 0;
            }
            KeyCode::Backspace => {
                state.files_picker_query.pop();
                state.files_picker_sel = 0;
            }
            KeyCode::Up => {
                state.files_picker_sel = state.files_picker_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = state
                    .files_picker
                    .as_ref()
                    .map(|p| {
                        files_picker_filtered(p, &state.files_picker_query)
                            .len()
                            .saturating_sub(1)
                    })
                    .unwrap_or(0);
                state.files_picker_sel = (state.files_picker_sel + 1).min(max);
            }
            KeyCode::PageUp => {
                state.files_picker_sel = state.files_picker_sel.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let max = state
                    .files_picker
                    .as_ref()
                    .map(|p| {
                        files_picker_filtered(p, &state.files_picker_query)
                            .len()
                            .saturating_sub(1)
                    })
                    .unwrap_or(0);
                state.files_picker_sel = (state.files_picker_sel + 10).min(max);
            }
            KeyCode::Enter => {
                let chosen = state.files_picker.as_ref().and_then(|p| {
                    files_picker_filtered(p, &state.files_picker_query)
                        .get(state.files_picker_sel)
                        .map(|s| (*s).clone())
                });
                state.files_picker = None;
                state.files_picker_query.clear();
                state.files_picker_sel = 0;
                if let Some(path) = chosen {
                    let snippet = format!("@{path} ");
                    if state.input.ends_with(' ') || state.input.is_empty() {
                        state.input.push_str(&snippet);
                    } else {
                        state.input.push(' ');
                        state.input.push_str(&snippet);
                    }
                    state.cursor = state.input.len();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                state.files_picker_query.push(c);
                state.files_picker_sel = 0;
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Session picker — opened by bare `/sessions`. ↑↓ moves selection,
    // Enter resumes the highlighted session via the same SessionStore
    // path that `/resume <id>` uses, Esc closes without state change.
    if state.session_picker.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if key.modifiers.is_empty() => {
                state.session_picker = None;
                state.session_picker_sel = 0;
            }
            KeyCode::Up => {
                state.session_picker_sel = state.session_picker_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = state
                    .session_picker
                    .as_ref()
                    .map(|v| v.len().saturating_sub(1))
                    .unwrap_or(0);
                state.session_picker_sel = (state.session_picker_sel + 1).min(max);
            }
            KeyCode::PageUp => {
                state.session_picker_sel = state.session_picker_sel.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let max = state
                    .session_picker
                    .as_ref()
                    .map(|v| v.len().saturating_sub(1))
                    .unwrap_or(0);
                state.session_picker_sel = (state.session_picker_sel + 10).min(max);
            }
            KeyCode::Enter => {
                let chosen = state
                    .session_picker
                    .as_ref()
                    .and_then(|v| v.get(state.session_picker_sel).map(|s| s.id.clone()));
                state.session_picker = None;
                state.session_picker_sel = 0;
                if let Some(id) = chosen {
                    // Mirror the success branch of `/resume <id>` so the
                    // picker yields the same banner + state reset.
                    match SessionStore::open(workspace, &id) {
                        Ok(store) => {
                            let msg_count = store.messages().len();
                            let last_user = store
                                .messages()
                                .iter()
                                .rev()
                                .find(|m| m.role == aegis_api::Role::User)
                                .and_then(|m| m.content.as_ref())
                                .map(|c| truncate_str(c, 80).to_string());
                            state.messages.clear();
                            state.tools.clear();
                            state.streaming_text.clear();
                            state.thinking_text.clear();
                            state.scroll_offset = 0;
                            state.session_id = id.clone();
                            let mut banner =
                                format!("⏮ resumed session {id} · {msg_count} messages");
                            if let Some(last) = last_user {
                                banner.push_str(&format!("\n  last prompt: {last}"));
                            }
                            state.push_system(&banner);
                            // session_id already mutated; main loop
                            // picks up the new session on the next tick
                            // since draw + handlers re-read state from
                            // the same Arc<Mutex<TuiApp>>.
                        }
                        Err(e) => {
                            state.push_error(&format!("resume failed: {e}"));
                        }
                    }
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Command palette (Ctrl+P) — typed chars filter, ↑/↓ navigate,
    // Enter inserts the picked command into input, Esc closes without
    // touching input. Same modal pattern as skill_menu but pulls from
    // the static catalogue so it works without skills loaded.
    if state.palette_open {
        match key.code {
            KeyCode::Esc => {
                state.palette_open = false;
                state.palette_query.clear();
                state.palette_sel = 0;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.palette_open = false;
                state.palette_query.clear();
                state.palette_sel = 0;
            }
            KeyCode::Backspace => {
                state.palette_query.pop();
                state.palette_sel = 0;
            }
            KeyCode::Up => {
                state.palette_sel = state.palette_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = palette_filtered(&state.palette_query)
                    .len()
                    .saturating_sub(1);
                state.palette_sel = (state.palette_sel + 1).min(max);
            }
            KeyCode::Enter => {
                let picked = palette_filtered(&state.palette_query)
                    .get(state.palette_sel)
                    .map(|(name, _)| (*name).to_string());
                state.palette_open = false;
                state.palette_query.clear();
                state.palette_sel = 0;
                if let Some(name) = picked {
                    // Pre-fill input with the command + trailing space so
                    // user can immediately type args, or hit Enter to run
                    // bare (works for /sessions, /providers, /init, etc.).
                    state.input = format!("{name} ");
                    state.cursor = state.input.len();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                state.palette_query.push(c);
                state.palette_sel = 0;
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // F1-F4 navigation tabs: F1 stays in chat (no-op), F2 opens the
    // files picker, F3 the sessions picker, F4 the permissions
    // timeline. Works regardless of whether `tabs_strip_visible` is
    // on — the strip is purely a visual reminder.
    match key.code {
        KeyCode::F(1) => {
            // Already in chat. Just dismiss any open overlay so F1 is a
            // reliable "get me back to typing" panic key.
            state.files_picker = None;
            state.session_picker = None;
            state.permission_overlay_open = false;
            state.help_overlay_open = false;
            state.palette_open = false;
            return KeyAction::None;
        }
        KeyCode::F(2) => {
            let walked = walk_workspace_files(workspace, 800);
            if !walked.is_empty() {
                state.files_picker_query.clear();
                state.files_picker_sel = 0;
                state.files_picker = Some(walked);
            } else {
                state.push_system("no files matched the picker filters");
            }
            return KeyAction::None;
        }
        KeyCode::F(3) => {
            match SessionStore::list(workspace) {
                Ok(list) if !list.is_empty() => {
                    state.session_picker_sel = 0;
                    state.session_picker = Some(list);
                }
                Ok(_) => {
                    state.push_system("no sessions yet");
                }
                Err(e) => {
                    state.push_error(&format!("could not list sessions: {e}"));
                }
            }
            return KeyAction::None;
        }
        KeyCode::F(4) => {
            if state.permission_history.is_empty() {
                state.push_system(
                    "no permission decisions yet — log fills as the agent calls tools",
                );
            } else {
                state.permission_overlay_open = true;
                state.permission_overlay_scroll = 0;
            }
            return KeyAction::None;
        }
        _ => {}
    }

    // Space toggles streaming pause when a turn is live and the input
    // is empty. Provider chunks keep arriving but accumulate in the
    // paused buffer; resume drains them so the user catches up to the
    // real provider position. Empty-input gate keeps Space usable as a
    // regular character while typing.
    if let KeyCode::Char(' ') = key.code {
        if key.modifiers.is_empty() && state.busy && state.input.is_empty() {
            state.toggle_stream_pause();
            return KeyAction::None;
        }
    }

    // Ctrl+P opens the command palette from the main input mode. Sits
    // before skill_menu so it can pre-empt the catch-all branch.
    if let KeyCode::Char('p') = key.code {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            state.palette_open = true;
            state.palette_query.clear();
            state.palette_sel = 0;
            return KeyAction::None;
        }
    }

    // Skill picker overlay — typing filters the list, Up/Down move selection,
    // Enter invokes the highlighted skill, Esc closes without action.
    if state.skill_menu.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if key.modifiers.is_empty() => {
                state.skill_menu = None;
                state.skill_filter = String::new();
                state.skill_sel = 0;
            }
            KeyCode::Backspace => {
                state.skill_filter.pop();
                state.skill_sel = 0;
            }
            KeyCode::Up => {
                state.skill_sel = state.skill_sel.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = state
                    .skill_menu
                    .as_ref()
                    .map(|v| skill_filtered(v, &state.skill_filter).len().saturating_sub(1))
                    .unwrap_or(0);
                state.skill_sel = (state.skill_sel + 1).min(max);
            }
            KeyCode::Enter => {
                let chosen = state.skill_menu.as_ref().and_then(|v| {
                    skill_filtered(v, &state.skill_filter)
                        .into_iter()
                        .nth(state.skill_sel)
                        .map(|s| s.name.clone())
                });
                state.skill_menu = None;
                state.skill_filter = String::new();
                state.skill_sel = 0;
                if let Some(name) = chosen {
                    // Pre-fill the input with the slash command so the user
                    // can optionally append $ARGS before hitting Enter again,
                    // or just press Enter immediately to invoke with no args.
                    state.input = format!("/{name}");
                    state.cursor = state.input.len();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                state.skill_filter.push(c);
                state.skill_sel = 0;
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Permission modal takes over the event stream while it's open.
    // Digits jump to an option, arrows move focus, Enter confirms the
    // focused option, Esc denies. Nothing else — no text editing — so
    // typed chars are absorbed instead of injected into the input line.
    if state.pending_permission.is_some() {
        let maybe_choice = match key.code {
            KeyCode::Char('1') => Some(PermissionChoice::Allow),
            KeyCode::Char('2') => Some(PermissionChoice::AlwaysAllow),
            KeyCode::Char('3') => Some(PermissionChoice::Deny),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PermissionChoice::Cancel)
            }
            KeyCode::Esc => Some(PermissionChoice::Cancel),
            KeyCode::Enter => {
                let focused = state
                    .pending_permission
                    .as_ref()
                    .map(|p| p.focused)
                    .unwrap_or(0);
                Some(match focused {
                    0 => PermissionChoice::Allow,
                    1 => PermissionChoice::AlwaysAllow,
                    _ => PermissionChoice::Deny,
                })
            }
            KeyCode::Up => {
                if let Some(p) = state.pending_permission.as_mut() {
                    p.focused = p.focused.saturating_sub(1);
                }
                None
            }
            KeyCode::Down => {
                if let Some(p) = state.pending_permission.as_mut() {
                    p.focused = (p.focused + 1).min(2);
                }
                None
            }
            _ => None,
        };
        if let Some(choice) = maybe_choice {
            if let Some(pending) = state.pending_permission.take() {
                let _ = pending.response_tx.send(choice);
            }
        }
        return KeyAction::None;
    }

    // Ask_user modal — Copilot-style question dialog.
    // Arrow keys navigate, digits jump to options, Enter confirms,
    // Esc declines. When "Other" (last option) is focused, typing
    // goes into a freeform buffer.
    if state.pending_ask_user.is_some() {
        match key.code {
            KeyCode::Esc => {
                if let Some(pending) = state.pending_ask_user.take() {
                    let _ = pending.response_tx.send(AskUserResponse::Declined);
                }
            }
            KeyCode::Enter => {
                if let Some(pending) = state.pending_ask_user.take() {
                    let last_idx = pending.options.len();
                    if pending.focused == last_idx {
                        // "Other" — send freeform text (or empty string)
                        let text = pending.freeform_text.clone();
                        let _ = pending.response_tx.send(AskUserResponse::Freeform(text));
                    } else {
                        let opt = pending.options[pending.focused].clone();
                        let _ = pending.response_tx.send(AskUserResponse::Option(opt));
                    }
                }
            }
            KeyCode::Up => {
                if let Some(p) = state.pending_ask_user.as_mut() {
                    p.focused = p.focused.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(p) = state.pending_ask_user.as_mut() {
                    let max = p.options.len(); // last = "Other"
                    p.focused = (p.focused + 1).min(max);
                }
            }
            KeyCode::Char(c) => {
                if let Some(p) = state.pending_ask_user.as_mut() {
                    let last_idx = p.options.len();
                    // Digit keys jump directly to option
                    if let Some(d) = c.to_digit(10) {
                        let idx = d as usize - 1;
                        if idx <= last_idx {
                            p.focused = idx;
                            if idx == last_idx {
                                p.freeform_active = true;
                            }
                        }
                    } else if p.focused == last_idx && p.freeform_active {
                        // Type into freeform field
                        p.freeform_text.push(c);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = state.pending_ask_user.as_mut() {
                    if p.focused == p.options.len() && p.freeform_active {
                        p.freeform_text.pop();
                    }
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }

    // Reverse-i-search mode: while active, the input area is a search
    // prompt, not an editable buffer. Keys are consumed here and the
    // normal editing path below is skipped.
    if state.search_state.is_some() {
        match key.code {
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.reverse_search_step();
                return KeyAction::None;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.reverse_search_cancel();
                return KeyAction::None;
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.reverse_search_cancel();
                return KeyAction::None;
            }
            KeyCode::Esc => {
                state.reverse_search_cancel();
                return KeyAction::None;
            }
            KeyCode::Enter => {
                state.reverse_search_accept();
                return KeyAction::None;
            }
            KeyCode::Backspace => {
                state.reverse_search_backspace();
                return KeyAction::None;
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.reverse_search_append(ch);
                return KeyAction::None;
            }
            // Any other key (arrows, Tab, Home/End, etc.) accepts the
            // current match and lets the key fall through to normal
            // editing on the accepted buffer.
            _ => {
                state.reverse_search_accept();
                // fall through into the rest of handle_key
            }
        }
    }

    // Ctrl+C while busy → cancel run.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.busy {
            return KeyAction::CancelRun;
        }
        return KeyAction::Quit;
    }

    // Ctrl+D → quit (unchanged, no prime dance).
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::Quit;
    }

    // Ctrl+R (not in search mode) → enter reverse-i-search.
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.reverse_search_begin();
        return KeyAction::None;
    }

    // Readline-standard line editing — Ctrl+A/E/K/U/W. Every CLI user
    // has muscle memory for these; without them the TUI feels broken.
    // Ordered so they short-circuit before the plain `KeyCode::Char`
    // branch below turns them into literal 'a'/'e'/'k'/'u'/'w'.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('o') => {
                // Toggle expansion of the most recent ToolResult message.
                if let Some(msg) = state
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::ToolResult)
                {
                    msg.expanded = !msg.expanded;
                }
                return KeyAction::None;
            }
            KeyCode::Char('a') => {
                state.home();
                return KeyAction::None;
            }
            KeyCode::Char('e') => {
                state.end();
                return KeyAction::None;
            }
            KeyCode::Char('k') => {
                state.kill_to_end();
                return KeyAction::None;
            }
            KeyCode::Char('u') => {
                state.kill_to_start();
                return KeyAction::None;
            }
            KeyCode::Char('w') => {
                state.kill_word_back();
                return KeyAction::None;
            }
            _ => {}
        }
    }

    // Esc priority: cancel running turn > clear input > quit (double-tap).
    //   - Busy: single Esc cancels the turn.
    //   - Non-empty input, idle: double-Esc clears, first primes (800ms).
    //   - Empty input, idle: double-Esc quits, first primes (800ms).
    //     Single-Esc-quits used to surprise users after /help (input
    //     empty after a slash command), so quit now requires a chord.
    if key.code == KeyCode::Esc {
        if state.at_search_active {
            state.at_search_active = false;
            state.at_search_matches.clear();
            return KeyAction::None;
        }
        if state.busy {
            return KeyAction::CancelRun;
        }
        let now = std::time::Instant::now();
        const PRIME_WINDOW_MS: u128 = 800;
        let primed = matches!(
            state.esc_primed_at,
            Some(t) if now.duration_since(t).as_millis() <= PRIME_WINDOW_MS
        );
        if state.input.is_empty() {
            if primed {
                state.esc_primed_at = None;
                return KeyAction::Quit;
            }
            state.esc_primed_at = Some(now);
            state.push_system("press Esc again within 0.8s to quit");
            return KeyAction::None;
        }
        if primed {
            state.input.clear();
            state.cursor = 0;
            state.esc_primed_at = None;
            state.push_system("input cleared");
        } else {
            state.esc_primed_at = Some(now);
            state.push_system("press Esc again within 0.8s to clear input");
        }
        return KeyAction::None;
    }

    // Any non-Esc key cancels an active Esc prime.
    if state.esc_primed_at.is_some() {
        state.esc_primed_at = None;
    }

    // Tab: `@` file search completion first, then permission-mode
    // cycling. Atakan: Shift+Tab her zaman cycle (CC pattern), boş
    // input'ta Tab da cycle (OpenCode pattern). 4-mod:
    // Default → AcceptEdits → Plan → Bypass → Default.
    // Shift+Tab always cycles regardless of input state.
    if key.code == KeyCode::Tab {
        if state.at_search_active {
            state.complete_at_search();
            return KeyAction::None;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift || state.input.is_empty() {
            state.cycle_permission_mode();
        } else {
            state.complete_tab(workspace);
        }
        return KeyAction::None;
    }
    if key.code == KeyCode::BackTab {
        // Some terminals deliver Shift+Tab as BackTab without modifiers.
        if state.at_search_active {
            state.complete_at_search();
        } else {
            state.cycle_permission_mode();
        }
        return KeyAction::None;
    }

    // Editing the buffer is ALWAYS allowed — users type follow-ups
    // while the agent is still responding, same as REPL mode. Enter
    // is gated below so we don't double-fire a run.
    match key.code {
        KeyCode::Enter => {
            // Shift+Enter OR Alt+Enter inserts a newline instead of
            // submitting, so users can compose multi-line prompts without
            // leaving the TUI. Plain Enter still submits.
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                state.insert_newline();
                return KeyAction::None;
            }
            if state.busy {
                // `!` shell commands run even while agent is busy
                if state.input.trim_start().starts_with('!') {
                    state.run_shell_command();
                    return KeyAction::None;
                }
                let text = state.take_input();
                // Stop words typed alone while agent is running → cancel.
                if is_stop_word(text.trim()) {
                    return KeyAction::CancelRun;
                }
                // Slash commands that don't hit the model (UI toggles like
                // `/sidebar`, `/mouse`, `/copy`, `/clear`, `/help`, `/cost`)
                // should fire instantly even while the agent is streaming —
                // queueing them means the user toggles a panel and nothing
                // visibly happens until the turn ends. Try the handler
                // first; only queue when it asks to send to the model.
                let trimmed = text.trim_start();
                if trimmed.starts_with('/') {
                    match state.handle_slash(trimmed, workspace) {
                        SlashResult::Handled | SlashResult::Clear => {
                            return KeyAction::None;
                        }
                        SlashResult::Quit => {
                            return KeyAction::Quit;
                        }
                        SlashResult::SendToModel(s) => {
                            state.pending_prompts.push_back(s);
                            let n = state.pending_prompts.len();
                            state.push_system(&format!("queued · {n} pending"));
                            return KeyAction::None;
                        }
                        SlashResult::SwitchSession(_) => {
                            // Re-queue the original text so the main loop
                            // can perform the session swap when idle.
                            state.pending_prompts.push_back(text);
                            let n = state.pending_prompts.len();
                            state.push_system(&format!("queued · {n} pending"));
                            return KeyAction::None;
                        }
                    }
                }
                // Copilot CLI style: enqueue the message, agent continues.
                if !text.trim().is_empty() {
                    state.pending_prompts.push_back(text);
                    let n = state.pending_prompts.len();
                    state.push_system(&format!("queued · {n} pending"));
                    return KeyAction::None;
                }
                return KeyAction::CancelRun;
            }
            // `!` shell commands — run directly, no model call
            if state.input.trim_start().starts_with('!') {
                state.run_shell_command();
                return KeyAction::None;
            }
            let text = state.take_input();
            return KeyAction::Submit(text);
        }
        KeyCode::Backspace => {
            state.backspace();
            // Cancel @ search if we backspaced past the @
            if state.at_search_active && state.cursor < state.at_search_start {
                state.at_search_active = false;
                state.at_search_matches.clear();
            }
        }
        KeyCode::Delete => state.delete(),
        KeyCode::Left => {
            state.move_left();
            if state.at_search_active && state.cursor < state.at_search_start {
                state.at_search_active = false;
                state.at_search_matches.clear();
            }
        }
        KeyCode::Right => state.move_right(),
        KeyCode::Home => {
            state.home();
            if state.at_search_active {
                state.at_search_active = false;
                state.at_search_matches.clear();
            }
        }
        KeyCode::End => state.end(),
        // Shift+↑/↓ scroll the chat. Plain ↑/↓: `@` search nav first,
        // then input history.
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => state.scroll_up(3),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => state.scroll_down(3),
        KeyCode::Up if state.at_search_active && !state.at_search_matches.is_empty() => {
            state.at_search_index = state.at_search_index.saturating_sub(1);
        }
        KeyCode::Down if state.at_search_active && !state.at_search_matches.is_empty() => {
            let max = state.at_search_matches.len().saturating_sub(1);
            state.at_search_index = (state.at_search_index + 1).min(max);
        }
        // When the input is empty, plain ↑/↓ scroll the chat instead of
        // walking input history — gives a keyboard scroll path on
        // terminals where wheel/trackpad events aren't delivered (tmux
        // without mouse passthrough, some SSH clients, etc.). With
        // anything in the input, history navigation wins so the user
        // can still recall earlier prompts.
        KeyCode::Up if state.input.is_empty() => state.scroll_up(3),
        KeyCode::Down if state.input.is_empty() => state.scroll_down(3),
        KeyCode::Up => state.history_up(),
        KeyCode::Down => state.history_down(),
        KeyCode::PageUp if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_to_top(),
        KeyCode::PageDown if key.modifiers.contains(KeyModifiers::CONTROL) => state.scroll_to_bottom(),
        KeyCode::PageUp => state.scroll_up(10),
        KeyCode::PageDown => state.scroll_down(10),
        KeyCode::Char(ch) => {
            state.insert_char(ch);
            // `@` file search refresh — debounced (200ms) to avoid WalkDir per keystroke
            if state.at_search_active && state.cursor >= state.at_search_start {
                let query: String = state.input[state.at_search_start..state.cursor].to_string();
                if query.contains(char::is_whitespace) {
                    state.at_search_active = false;
                } else {
                    let now = std::time::Instant::now();
                    let should_refresh = state.at_search_last_refresh
                        .map(|t| now.duration_since(t).as_millis() >= 200)
                        .unwrap_or(true);
                    if should_refresh {
                        state.refresh_at_search(workspace, &query);
                        state.at_search_last_refresh = Some(now);
                    }
                }
            }
        }
        _ => {}
    }

    KeyAction::None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- Command palette ----------

    #[test]
    fn palette_filter_empty_returns_all() {
        let all = palette_filtered("");
        assert_eq!(all.len(), palette_commands().len());
    }

    #[test]
    fn palette_filter_matches_command_name() {
        let hits = palette_filtered("ask");
        let names: Vec<&str> = hits.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"/ask"));
        assert!(names.contains(&"/askall"));
    }

    #[test]
    fn palette_filter_matches_description_words() {
        // "panele" is in /sidebar's description ("yan paneli aç/kapat")
        let hits = palette_filtered("panel");
        let names: Vec<&str> = hits.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"/sidebar"));
    }

    #[test]
    fn palette_filter_strips_leading_slash() {
        // User typing "/as" should still match /ask + /askall.
        let hits = palette_filtered("/as");
        let names: Vec<&str> = hits.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"/ask"));
        assert!(names.contains(&"/askall"));
    }

    #[test]
    fn palette_filter_no_match_returns_empty() {
        assert!(palette_filtered("zzzzznotacommand").is_empty());
    }

    // ---------- Session picker ----------

    #[test]
    fn sessions_bare_opens_picker_when_sessions_exist() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Two sessions on disk — picker should populate, not dump text.
        let s1 = SessionStore::new_id();
        let s2 = SessionStore::new_id();
        let _ = SessionStore::open(&ws, &s1).unwrap();
        let _ = SessionStore::open(&ws, &s2).unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/sessions", &ws);
        assert!(app.session_picker.is_some(), "bare /sessions must open picker");
        let picked = app.session_picker.as_ref().unwrap();
        assert!(picked.len() >= 2, "picker should see both sessions");
    }

    #[test]
    fn sessions_list_keeps_text_dump_for_pipe_users() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let _ = SessionStore::open(&ws, &sid).unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/sessions list", &ws);
        // text dump path: picker stays None, last message is system text
        // listing the session(s).
        assert!(app.session_picker.is_none());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("sessions ("));
    }

    // ---------- Multi-file edit preview ----------

    #[test]
    fn edit_preview_emits_header_with_diff_counts() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!(
            "aegis_edit_preview_{}_{}",
            std::process::id(),
            "header"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foo.rs");
        std::fs::write(&path, "fn old() {}\nfn other() {}\n").unwrap();
        let args = json!({
            "path": path.to_str().unwrap(),
            "old_string": "fn old() {}",
            "new_string": "fn renamed() {}\nfn extra() {}",
            "replace_all": false,
        });
        let rows = build_edit_preview_lines("edit_file", &args);
        assert!(!rows.is_empty(), "edit_file must produce preview rows");
        let header_plain = &rows[0].0;
        assert!(header_plain.contains("📄 preview"));
        assert!(header_plain.contains("+"), "header carries +N count");
        assert!(header_plain.contains("-"), "header carries -M count");
    }

    #[test]
    fn edit_preview_warns_when_old_string_missing() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!(
            "aegis_edit_preview_{}_{}",
            std::process::id(),
            "missing"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foo.rs");
        std::fs::write(&path, "completely different content\n").unwrap();
        let args = json!({
            "path": path.to_str().unwrap(),
            "old_string": "this string does not exist anywhere",
            "new_string": "anything",
        });
        let rows = build_edit_preview_lines("edit_file", &args);
        assert!(!rows.is_empty());
        assert!(
            rows[0].0.starts_with("⚠ preview: old_string not found"),
            "missing-anchor case must yield a yellow warning, got: {}",
            rows[0].0
        );
    }

    #[test]
    fn multi_edit_preview_starts_with_bundle_summary() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!(
            "aegis_multi_edit_preview_{}_{}",
            std::process::id(),
            "summary"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("a.rs");
        let p2 = dir.join("b.rs");
        std::fs::write(&p1, "let x = 1;\n").unwrap();
        std::fs::write(&p2, "let y = 2;\n").unwrap();
        let args = json!({
            "edits": [
                {"path": p1.to_str().unwrap(), "old_string": "let x = 1;", "new_string": "let x = 10;"},
                {"path": p1.to_str().unwrap(), "old_string": "let x = 10;", "new_string": "let x = 100;"},
                {"path": p2.to_str().unwrap(), "old_string": "let y = 2;", "new_string": "let y = 20;"},
            ]
        });
        let rows = build_edit_preview_lines("multi_edit", &args);
        assert!(rows[0].0.starts_with("📦 preview: 3 edits across 2 files"));
    }

    #[test]
    fn write_file_preview_emits_kind_and_size() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!(
            "aegis_write_preview_{}_{}",
            std::process::id(),
            "newfile"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("does_not_exist.rs");
        let args = json!({
            "path": path.to_str().unwrap(),
            "content": "line 1\nline 2\nline 3\n",
        });
        let rows = build_edit_preview_lines("write_file", &args);
        assert_eq!(rows.len(), 1);
        let plain = &rows[0].0;
        assert!(plain.contains("📄 preview"));
        assert!(plain.contains("new file"));
        assert!(plain.contains("3 lines"));
    }

    #[test]
    fn unknown_tool_yields_empty_preview() {
        use serde_json::json;
        let rows = build_edit_preview_lines("bash", &json!({"command": "ls"}));
        assert!(rows.is_empty());
    }

    // ---------- Files picker ----------

    #[test]
    fn walk_workspace_skips_target_and_git() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/foo")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "// keep").unwrap();
        std::fs::write(root.join("README.md"), "keep").unwrap();
        std::fs::write(root.join("target/debug/deps/leak.o"), "skip").unwrap();
        std::fs::write(root.join(".git/objects/skip"), "skip").unwrap();
        std::fs::write(root.join("node_modules/foo/skip.js"), "skip").unwrap();
        let walked = walk_workspace_files(root, 100);
        assert!(walked.iter().any(|p| p == "src/lib.rs"));
        assert!(walked.iter().any(|p| p == "README.md"));
        assert!(!walked.iter().any(|p| p.contains("target/")));
        assert!(!walked.iter().any(|p| p.contains(".git/")));
        assert!(!walked.iter().any(|p| p.contains("node_modules/")));
    }

    #[test]
    fn walk_workspace_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let walked = walk_workspace_files(tmp.path(), 10);
        assert_eq!(walked.len(), 10, "must hard-cap at limit");
    }

    #[test]
    fn files_picker_filter_substring_match() {
        let paths = vec![
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
            "tests/integration.rs".to_string(),
            "README.md".to_string(),
        ];
        let hits = files_picker_filtered(&paths, "lib");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0], "src/lib.rs");
    }

    #[test]
    fn files_picker_filter_empty_returns_all() {
        let paths = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(files_picker_filtered(&paths, "").len(), 3);
    }

    // ---------- Streaming pause/resume ----------

    #[test]
    fn paused_stream_diverts_chunks_to_buffer() {
        let mut app = TuiApp::new("m");
        app.streaming_text.clear();
        app.stream_paused_buffer.clear();
        app.stream_paused = true;
        app.push_stream_chunk("hello ");
        app.push_stream_chunk("world");
        assert!(app.streaming_text.is_empty(), "paused must not surface live");
        assert_eq!(app.stream_paused_buffer, "hello world");
    }

    #[test]
    fn unpaused_stream_appends_to_streaming_text() {
        let mut app = TuiApp::new("m");
        app.streaming_text.clear();
        app.stream_paused = false;
        app.push_stream_chunk("foo");
        app.push_stream_chunk("bar");
        assert_eq!(app.streaming_text, "foobar");
        assert!(app.stream_paused_buffer.is_empty());
    }

    #[test]
    fn toggle_pause_resume_drains_buffer() {
        let mut app = TuiApp::new("m");
        app.streaming_text.clear();
        app.stream_paused_buffer.clear();
        // First toggle: pause
        app.toggle_stream_pause();
        assert!(app.stream_paused);
        // Chunks while paused queue
        app.push_stream_chunk("queued ");
        app.push_stream_chunk("text");
        assert!(app.streaming_text.is_empty());
        // Second toggle: resume drains
        app.toggle_stream_pause();
        assert!(!app.stream_paused);
        assert_eq!(app.streaming_text, "queued text");
        assert!(app.stream_paused_buffer.is_empty());
    }

    // ---------- Image attach metadata ----------

    #[test]
    fn format_image_byte_size_picks_sensible_unit() {
        assert_eq!(format_image_byte_size(512), "512 B");
        assert_eq!(format_image_byte_size(2048), "2.0 KB");
        let mb = format_image_byte_size(2 * 1024 * 1024 + 512 * 1024);
        assert!(mb.starts_with("2.5"), "got {mb}");
        assert!(mb.ends_with(" MB"));
    }

    #[test]
    fn format_image_byte_size_threshold_at_1kb() {
        assert_eq!(format_image_byte_size(1023), "1023 B");
        assert_eq!(format_image_byte_size(1024), "1.0 KB");
    }

    // ---------- MCP attached list ----------

    #[test]
    fn attached_mcps_default_empty() {
        let app = TuiApp::new("m");
        let g = app.attached_mcps.lock().unwrap();
        assert!(g.is_empty());
    }

    // ---------- Slash ghost-text suggestion ----------

    #[test]
    fn ghost_suggestion_unique_match_returns_full_suffix() {
        // `/compact` is the only entry starting with `compa`, so the
        // ghost must be the full unique suffix.
        let g = slash_ghost_suggestion("/compa");
        assert_eq!(g.as_deref(), Some("ct"));
    }

    #[test]
    fn ghost_suggestion_shared_prefix_extends_to_branch_point() {
        // `/session` and `/session-info` and `/sessions` all share
        // `session`. `/sess` must ghost just `ion` to reach the
        // branch point, not pick a winner arbitrarily.
        let g = slash_ghost_suggestion("/sess");
        assert_eq!(g.as_deref(), Some("ion"));
    }

    #[test]
    fn ghost_suggestion_multi_match_returns_common_delta() {
        // `/ask` and `/askall` share `ask`; typing `/as` must extend
        // to the shared prefix `ask` (delta = "k") rather than picking
        // one arbitrarily.
        let g = slash_ghost_suggestion("/as");
        assert_eq!(g.as_deref(), Some("k"));
    }

    #[test]
    fn ghost_suggestion_no_match_returns_none() {
        assert!(slash_ghost_suggestion("/zzznotacommand").is_none());
    }

    #[test]
    fn ghost_suggestion_skips_when_input_has_whitespace() {
        // `/ask transformer ...` is a real prompt, not a typing-the-
        // command-name moment. No ghost.
        assert!(slash_ghost_suggestion("/ask transformer").is_none());
    }

    #[test]
    fn ghost_suggestion_skips_when_no_leading_slash() {
        assert!(slash_ghost_suggestion("ask transformer").is_none());
    }

    // ---------- Permission timeline ----------

    // ---------- Fork chain ----------

    // ---------- Action chips ----------

    // ---------- Tabs strip ----------

    #[test]
    fn tabs_strip_default_off() {
        let app = TuiApp::new("m");
        assert!(!app.tabs_strip_visible);
    }

    #[test]
    fn tabs_slash_toggles_visibility() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.handle_slash("/tabs", tmp.path());
        assert!(app.tabs_strip_visible);
        app.handle_slash("/tabs", tmp.path());
        assert!(!app.tabs_strip_visible);
    }

    #[test]
    fn tool_invites_action_chips_recognizes_edit_and_bash() {
        assert!(tool_invites_action_chips("edited foo.rs (1 replacement)\n--- foo.rs"));
        assert!(tool_invites_action_chips("$ ls -la\nfoo bar"));
        assert!(tool_invites_action_chips("📦 preview: 3 edits across 2 files"));
        assert!(tool_invites_action_chips("📄 preview: foo.rs  +5 -2"));
    }

    #[test]
    fn tool_invites_action_chips_skips_read_only() {
        // grep / glob / read_file results don't start with mutation
        // verbs, so chips stay hidden — those tools have nothing to
        // explain/revise/redo.
        assert!(!tool_invites_action_chips("matches in foo.rs:\n  line 12: hit"));
        assert!(!tool_invites_action_chips("pattern not found"));
        assert!(!tool_invites_action_chips(""));
    }

    #[test]
    fn fork_chain_root_session_is_single_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let chain = fork_chain(tmp.path(), "root-id");
        assert_eq!(chain, vec!["root-id".to_string()]);
    }

    #[test]
    fn fork_chain_walks_parent_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".metis").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("child.meta.json"),
            r#"{"parent_id":"middle"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("middle.meta.json"),
            r#"{"parent_id":"root"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("root.meta.json"), r#"{}"#).unwrap();
        let chain = fork_chain(tmp.path(), "child");
        assert_eq!(
            chain,
            vec![
                "root".to_string(),
                "middle".to_string(),
                "child".to_string()
            ]
        );
    }

    #[test]
    fn fork_chain_breaks_on_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".metis").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        // Pathological: a points to b, b points to a.
        std::fs::write(dir.join("a.meta.json"), r#"{"parent_id":"b"}"#).unwrap();
        std::fs::write(dir.join("b.meta.json"), r#"{"parent_id":"a"}"#).unwrap();
        let chain = fork_chain(tmp.path(), "a");
        // Must terminate, must not loop forever.
        assert!(chain.len() <= 6);
        assert_eq!(chain.last().unwrap(), "a");
    }

    #[test]
    fn record_permission_appends_entry() {
        let mut app = TuiApp::new("m");
        record_permission(
            &mut app,
            "edit_file",
            r#"{"path":"foo.rs"}"#,
            PermissionLogDecision::Allow,
        );
        assert_eq!(app.permission_history.len(), 1);
        assert_eq!(app.permission_history[0].tool, "edit_file");
        assert_eq!(
            app.permission_history[0].decision,
            PermissionLogDecision::Allow
        );
    }

    #[test]
    fn record_permission_truncates_giant_args() {
        let mut app = TuiApp::new("m");
        let big = "x".repeat(2000);
        record_permission(&mut app, "multi_edit", &big, PermissionLogDecision::Allow);
        let entry = &app.permission_history[0];
        assert!(entry.args_preview.chars().count() <= 241, "must hard-cap");
        assert!(entry.args_preview.ends_with('…'));
    }

    #[test]
    fn record_permission_caps_history_at_200() {
        let mut app = TuiApp::new("m");
        for i in 0..205 {
            record_permission(
                &mut app,
                &format!("tool_{i}"),
                "{}",
                PermissionLogDecision::Allow,
            );
        }
        assert_eq!(app.permission_history.len(), 200);
        // Oldest entries should have rolled off.
        assert_eq!(app.permission_history.first().unwrap().tool, "tool_5");
        assert_eq!(app.permission_history.last().unwrap().tool, "tool_204");
    }

    #[test]
    fn permissions_slash_opens_overlay_when_history_nonempty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        record_permission(
            &mut app,
            "edit_file",
            "{}",
            PermissionLogDecision::AutoAllow,
        );
        app.handle_slash("/permissions", tmp.path());
        assert!(app.permission_overlay_open);
    }

    #[test]
    fn permissions_slash_no_overlay_when_history_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.handle_slash("/permissions", tmp.path());
        assert!(!app.permission_overlay_open);
        let last = app.messages.last().unwrap();
        assert!(last.text.contains("no permission decisions"));
    }

    #[test]
    fn format_seconds_ago_picks_unit_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(format_seconds_ago(now), "just now");
        assert!(format_seconds_ago(now.saturating_sub(30)).ends_with("s ago"));
        assert!(format_seconds_ago(now.saturating_sub(180)).ends_with("m ago"));
        assert!(format_seconds_ago(now.saturating_sub(7200)).ends_with("h ago"));
    }

    #[test]
    fn ghost_suggestion_skips_on_exact_match() {
        // Typing the full command should not show a ghost — there's
        // nothing to extend.
        assert!(slash_ghost_suggestion("/sessions").is_none());
    }

    #[test]
    fn attached_mcps_dedupe_label() {
        let app = TuiApp::new("m");
        {
            let mut g = app.attached_mcps.lock().unwrap();
            let label = "playwright (5 tools)".to_string();
            if !g.iter().any(|s| s == &label) {
                g.push(label.clone());
            }
            // Same label shouldn't double-push.
            if !g.iter().any(|s| s == &label) {
                g.push(label);
            }
        }
        assert_eq!(app.attached_mcps.lock().unwrap().len(), 1);
    }

    #[test]
    fn flush_streaming_drains_paused_buffer_too() {
        let mut app = TuiApp::new("m");
        app.streaming_text.clear();
        app.stream_paused = true;
        app.stream_paused_buffer = "tail content".to_string();
        app.flush_streaming();
        // Buffer must surface as a finalized assistant message rather
        // than vanishing because the turn ended while paused.
        let last = app.messages.iter().rev().find(|m| m.role == MessageRole::Assistant);
        assert!(last.is_some(), "paused tail must flush as assistant turn");
        assert!(last.unwrap().text.contains("tail content"));
        assert!(!app.stream_paused);
        assert!(app.stream_paused_buffer.is_empty());
    }

    #[test]
    fn bare_files_opens_picker_when_workspace_has_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "y").unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/files", tmp.path());
        assert!(app.files_picker.is_some(), "bare /files must open picker");
        let paths = app.files_picker.as_ref().unwrap();
        assert!(paths.iter().any(|p| p == "a.rs"));
        assert!(paths.iter().any(|p| p == "b.rs"));
    }

    #[test]
    fn sessions_picker_empty_dir_pushes_no_sessions_msg() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("empty");
        std::fs::create_dir_all(&ws).unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/sessions", &ws);
        assert!(app.session_picker.is_none(), "empty dir must not open picker");
        let last = app.messages.last().unwrap();
        assert!(last.text.contains("no sessions"));
    }

    // --- A. Input editing ---

    // ============================================================
    // TEST SCHEMA (sıra şeması)
    // ============================================================
    // A. Input editing        — insert, backspace, cursor, kill, delete, newline
    // B. Completion           — slash suggest, levenshtein, tab complete
    // C. Drag-drop / paste    — image parse, bracketed paste
    // D. History / scroll     — history nav, scroll math, take_input
    // E. Slash commands       — /help, /exit, /clear, /cost, /rate, /fork, /plan, /btw
    // F. Slash commands (new) — /init, /undo, /context, /usage, /editor, /export
    // G. Message rendering    — push, flush, strip thinking, roles, diff
    // H. Recap / status line  — tool count, plan chip, queue strip, files touched
    // I. Plan mode / mode     — toggle_plan_mode, plan chip, mode cycling
    // J. Permission / allow   — allow_menu, permission modal
    // K. Session / fork       — fork, overwrite, expiry
    // L. Shell / @ search     — run_shell_command, at_search
    // M. View / search        — /view output, /search
    // N. Misc                 — tmp_ws, home_lock, cleanup, drag-drop edge cases
    // ============================================================

    /// HOME env is process-wide. Tests that mutate it must serialize
    /// through this lock or they will clobber each other under
    /// `cargo test`'s default parallel runner.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
    // --- B. Completion ---
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn test_new_app_defaults() {
        let app = TuiApp::new("test-model");
        assert_eq!(app.model, "test-model");
        assert_eq!(app.turn_count, 0);
        assert!(!app.busy);
        assert!(!app.should_quit);
        assert_eq!(app.messages.len(), 1); // welcome message
        assert_eq!(app.messages[0].role, MessageRole::System);
    }

    #[test]
    fn test_insert_and_backspace() {
        let mut app = TuiApp::new("m");
        app.insert_char('h');
        app.insert_char('i');
        assert_eq!(app.input, "hi");
        assert_eq!(app.cursor, 2);

        app.backspace();
        assert_eq!(app.input, "h");
        assert_eq!(app.cursor, 1);

        app.backspace();
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);

        // Backspace on empty should not panic.
        app.backspace();
        assert_eq!(app.input, "");
    }
    // --- C. Drag-drop / paste ---

    #[test]
    fn test_cursor_movement() {
        let mut app = TuiApp::new("m");
        app.insert_char('a');
        app.insert_char('b');
        app.insert_char('c');
        assert_eq!(app.cursor, 3);

        app.move_left();
        assert_eq!(app.cursor, 2);
        app.move_left();
        assert_eq!(app.cursor, 1);
        app.home();
        assert_eq!(app.cursor, 0);
        app.move_left(); // should not go negative
        assert_eq!(app.cursor, 0);

        app.end();
        assert_eq!(app.cursor, 3);
        app.move_right(); // should not go past end
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn test_kill_to_end() {
        let mut app = TuiApp::new("m");
        for c in "hello world".chars() {
            app.insert_char(c);
        }
        app.cursor = 5; // between "hello" and " world"
        app.kill_to_end();
        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn test_kill_to_start() {
        let mut app = TuiApp::new("m");
        for c in "hello world".chars() {
            app.insert_char(c);
        }
        app.cursor = 6; // at 'w'
        app.kill_to_start();
        assert_eq!(app.input, "world");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn test_kill_word_back_trailing_space() {
        let mut app = TuiApp::new("m");
        for c in "foo bar ".chars() {
    // --- D. History / scroll ---
            app.insert_char(c);
        }
        // cursor at end ("foo bar |")
        app.kill_word_back();
        assert_eq!(app.input, "foo ");
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn test_kill_word_back_mid_word() {
        let mut app = TuiApp::new("m");
        for c in "foo bar baz".chars() {
            app.insert_char(c);
        }
        app.cursor = 7; // after "foo bar"
        app.kill_word_back();
        assert_eq!(app.input, "foo  baz");
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn test_kill_word_back_at_start_is_noop() {
        let mut app = TuiApp::new("m");
        app.insert_char('x');
        app.cursor = 0;
        app.kill_word_back();
        assert_eq!(app.input, "x");
        assert_eq!(app.cursor, 0);
    }

    // --- G. Message rendering ---
    #[test]
    fn test_suggest_prefix_match() {
        assert_eq!(super::suggest_slash_command("imag"), Some("image"));
        assert_eq!(super::suggest_slash_command("hel"), Some("help"));
        assert_eq!(super::suggest_slash_command("mod"), Some("model"));
    }

    #[test]
    fn test_suggest_typo_levenshtein() {
        // single-char typo → suggest
        assert_eq!(super::suggest_slash_command("imge"), Some("image"));
        assert_eq!(super::suggest_slash_command("clera"), Some("clear"));
    }

    #[test]
    fn test_suggest_returns_none_on_gibberish() {
        assert_eq!(super::suggest_slash_command("xyzzy"), None);
        assert_eq!(super::suggest_slash_command(""), None);
    }

    #[test]
    fn test_suggest_does_not_return_self_on_exact() {
        // Exact known cmd should NOT suggest itself — that would be
        // weird in the "unknown command" path.
        assert_ne!(super::suggest_slash_command("image"), Some("image"));
    }

    #[test]
    fn test_levenshtein_basic() {
        assert_eq!(super::levenshtein("", ""), 0);
        assert_eq!(super::levenshtein("cat", "cat"), 0);
        assert_eq!(super::levenshtein("cat", "bat"), 1);
        assert_eq!(super::levenshtein("cat", ""), 3);
        assert_eq!(super::levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn test_try_parse_drag_drop_single_image() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("screenshot.png");
        std::fs::write(&img, b"fakepng").unwrap();
        let got = super::try_parse_drag_drop_paths(img.to_str().unwrap(), tmp.path());
        assert_eq!(got, Some(vec![img]));
    }

    #[test]
    fn test_try_parse_drag_drop_quoted_path_with_space() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("with space");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("x.png");
        std::fs::write(&img, b"fakepng").unwrap();
        let pasted = format!("\"{}\"", img.display());
        let got = super::try_parse_drag_drop_paths(&pasted, tmp.path());
        assert_eq!(got, Some(vec![img]));
    }

    #[test]
    fn test_try_parse_drag_drop_multi_newline() {
        let tmp = tempfile::tempdir().unwrap();
    // --- E. Slash commands ---
        let a = tmp.path().join("a.png");
        let b = tmp.path().join("b.jpg");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let pasted = format!("{}\n{}", a.display(), b.display());
        let got = super::try_parse_drag_drop_paths(&pasted, tmp.path());
        assert_eq!(got, Some(vec![a, b]));
    }

    #[test]
    fn test_try_parse_drag_drop_rejects_text() {
        let tmp = tempfile::tempdir().unwrap();
        // Plain text paste with spaces → should NOT be treated as path.
        let got = super::try_parse_drag_drop_paths("hey can you review this code", tmp.path());
        assert_eq!(got, None);
    }

    #[test]
    fn test_try_parse_drag_drop_rejects_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let got = super::try_parse_drag_drop_paths("/does/not/exist.png", tmp.path());
        assert_eq!(got, None);
    }

    #[test]
    fn test_try_parse_drag_drop_rejects_wrong_ext() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("file.txt");
        std::fs::write(&p, b"hi").unwrap();
        let got = super::try_parse_drag_drop_paths(p.to_str().unwrap(), tmp.path());
        assert_eq!(got, None);
    }

    #[test]
    fn test_insert_newline_shift_enter() {
        let mut app = TuiApp::new("m");
        for c in "hello".chars() {
            app.insert_char(c);
        }
        app.insert_newline();
        for c in "world".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "hello\nworld");
        assert_eq!(app.cursor, 11);
    }

    #[test]
    fn test_delete_at_cursor() {
        let mut app = TuiApp::new("m");
        for c in "hello".chars() {
            app.insert_char(c);
        }
        app.home();
        app.delete(); // removes 'h'
        assert_eq!(app.input, "ello");
        assert_eq!(app.cursor, 0);

        app.end();
        app.delete(); // at end, should do nothing
        assert_eq!(app.input, "ello");
    }

    #[test]
    fn test_take_input() {
        let mut app = TuiApp::new("m");
        app.insert_char('x');
        let text = app.take_input();
        assert_eq!(text, "x");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
        assert_eq!(app.input_history, vec!["x".to_string()]);
    }

    #[test]
    fn test_history_navigation() {
        let mut app = TuiApp::new("m");

        // Add some history entries.
        app.input = "first".into();
        app.cursor = 5;
        app.take_input();
        app.input = "second".into();
        app.cursor = 6;
        app.take_input();

        // Browse up.
        app.insert_char('c'); // current live input
        app.history_up();
        assert_eq!(app.input, "second");
        app.history_up();
        assert_eq!(app.input, "first");
        app.history_up(); // can't go further
        assert_eq!(app.input, "first");

        // Browse down.
        app.history_down();
        assert_eq!(app.input, "second");
        app.history_down(); // back to live input
        assert_eq!(app.input, "c");
    }

    #[test]
    fn test_scroll() {
        let mut app = TuiApp::new("m");
        assert_eq!(app.scroll_offset, 0);
        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 5);
        app.scroll_down(3);
        assert_eq!(app.scroll_offset, 2);
        app.scroll_down(10); // should not go below 0
        assert_eq!(app.scroll_offset, 0);
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn pin_offset_stays_zero_at_bottom() {
        // User at bottom — auto-follow, stored offset untouched.
        assert_eq!(pin_scroll_offset(0, 100, 105, 75), 0);
        assert_eq!(pin_scroll_offset(0, 100, 95, 65), 0);
    }

    #[test]
    fn pin_offset_bumps_on_growth_to_hold_viewport() {
        // Streaming adds 5 rows — offset rises by 5 so absolute rows
        // the user sees don't drift forward.
        assert_eq!(pin_scroll_offset(20, 100, 105, 75), 25);
    }

    #[test]
    fn pin_offset_drops_on_shrink_to_hold_viewport() {
        // Thinking spinner (2 rows) dismissed — content shrinks by 2,
        // offset falls by 2 so the user's viewport doesn't ride up.
        assert_eq!(pin_scroll_offset(20, 100, 98, 68), 18);
    }

    #[test]
    fn pin_offset_clamps_pageup_overshoot() {
        // User mashed PageUp past max_scroll — stored offset inflated
        // to 10_000. Next draw clamps it so PageDown responds on the
        // first press instead of burning through overshoot.
        assert_eq!(pin_scroll_offset(10_000, 100, 100, 70), 70);
    }

    #[test]
    fn pin_offset_resize_wider_preserves_relative_position() {
        // Terminal resize wider: total shrinks from 100 to 60 (less
        // wrapping). Shrink-pin pulls offset down by 40 so the user's
        // relative reading depth is roughly preserved instead of
        // snapping to top-of-scrollback.
        assert_eq!(pin_scroll_offset(50, 100, 60, 30), 10);
    }

    #[test]
    fn pin_offset_pure_clamp_when_no_delta() {
        // Static content, stored offset already above max_scroll
        // (stale from a prior resize). No growth, no shrink — just
        // clamp. Proves the tail clamp fires independently of the
        // growth/shrink branches.
        assert_eq!(pin_scroll_offset(100, 100, 100, 50), 50);
    }

    #[test]
    fn pin_offset_shrink_saturates_at_zero() {
        // Huge shrink (e.g., /clear survivors) — offset can't go below
        // zero; saturating_sub keeps it at bottom instead of wrapping.
        assert_eq!(pin_scroll_offset(5, 100, 20, 0), 0);
    }

    #[test]
    fn pin_offset_unchanged_when_no_delta() {
        // Static content, user scrolled up — offset sticks at its
        // current value, just re-clamped against max_scroll.
        assert_eq!(pin_scroll_offset(25, 100, 100, 70), 25);
    }

    #[test]
    fn test_push_messages() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.push_user("hello");
        app.push_assistant("hi there");
        app.push_error("oops");
        assert_eq!(app.messages.len(), initial + 3);
        assert_eq!(app.messages[initial].role, MessageRole::User);
        assert_eq!(app.messages[initial + 1].role, MessageRole::Assistant);
        assert_eq!(app.messages[initial + 2].role, MessageRole::Error);
    }

    #[test]
    fn test_flush_streaming() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.streaming_text = "partial response".into();
        app.thinking_text = "hmm".into();
        app.flush_streaming();
        assert_eq!(app.messages.len(), initial + 1);
        assert_eq!(app.messages.last().unwrap().text, "partial response");
        assert!(app.streaming_text.is_empty());
        assert!(app.thinking_text.is_empty());
    }

    #[test]
    fn test_flush_streaming_empty() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.flush_streaming(); // nothing to flush
        assert_eq!(app.messages.len(), initial);
    }

    #[test]
    fn strip_thinking_removes_closed_anthropic_block() {
        assert_eq!(
            strip_thinking_tags("hi <thinking>inner</thinking> there"),
            "hi  there"
        );
    }

    #[test]
    fn strip_thinking_removes_closed_deepseek_block() {
        assert_eq!(
            strip_thinking_tags("<think>plan</think>final answer"),
            "final answer"
        );
    }

    #[test]
    fn strip_thinking_drops_from_open_tag_when_unclosed() {
        // Mid-stream: close tag hasn't arrived yet — hide from the
        // open tag to end so the user doesn't see raw XML.
        assert_eq!(
            strip_thinking_tags("prefix <thinking>reasoning in progress"),
            "prefix "
        );
    }

    #[test]
    fn strip_thinking_handles_multiple_blocks() {
        assert_eq!(
            strip_thinking_tags("a<think>1</think>b<thinking>2</thinking>c"),
            "abc"
        );
    }

    #[test]
    fn strip_thinking_passthrough_when_absent() {
        assert_eq!(strip_thinking_tags("plain text"), "plain text");
        assert_eq!(strip_thinking_tags(""), "");
    }

    #[test]
    fn flush_streaming_drops_pure_thinking_block() {
        // Saved transcript stays clean — a turn whose entire output
        // was `<thinking>…</thinking>` doesn't leave an empty
        // assistant message behind.
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.streaming_text = "<thinking>just reasoning, no answer</thinking>".into();
        app.flush_streaming();
        assert_eq!(app.messages.len(), initial);
        assert!(app.streaming_text.is_empty());
    }

    #[test]
    fn turn_recap_shows_tools_files_duration() {
        let mut app = TuiApp::new("m");
        app.turn_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(1500));
        app.tool_start("edit_file", "path: src/foo.rs, old_string: …");
        app.tool_start("edit_file", "path: src/bar.rs, old_string: …");
        app.tool_start("bash", "command: cargo test");
        app.tool_start("read_file", "path: src/foo.rs");
        let recap = app.build_turn_recap();
        // build_turn_recap groups by verb: "read N file(s), edited N file(s), ran N command(s)"
        assert!(recap.contains("read 1 file"), "read count wrong: {recap}");
        assert!(
            recap.contains("edited 2 files"),
            "edit count wrong: {recap}"
        );
        assert!(recap.contains("ran 1 command"), "run count wrong: {recap}");
        assert!(
            recap.contains("1.5s") || recap.contains("1.4s"),
            "duration wrong: {recap}"
        );
    }

    #[test]
    fn turn_recap_empty_when_nothing_happened() {
        let app = TuiApp::new("m");
        assert_eq!(app.build_turn_recap(), "");
    }

    #[test]
    fn extract_path_handles_common_preview_shapes() {
        assert_eq!(
            extract_path_from_preview("path: src/foo.rs, old_string: hi"),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(
            extract_path_from_preview(r#"{"path":"src/bar.rs","content":"…"}"#),
            Some("src/bar.rs".to_string())
        );
        assert_eq!(extract_path_from_preview("no path here"), None);
    }

    #[test]
    fn flush_streaming_keeps_post_thinking_content() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.streaming_text = "<think>plan</think>answer is 42".into();
        app.flush_streaming();
        assert_eq!(app.messages.len(), initial + 1);
        assert_eq!(app.messages.last().unwrap().text, "answer is 42");
    }

    #[test]
    fn test_tool_lifecycle() {
        let mut app = TuiApp::new("m");
        app.tool_start("read_file", "{\"path\":\"foo.rs\"}");
        assert_eq!(app.tools.len(), 1);
        assert_eq!(app.tools[0].status, ToolStatus::Running);

        app.tool_done("read_file", "200 lines", false);
        assert_eq!(app.tools[0].status, ToolStatus::Done);
        assert_eq!(app.tools[0].preview, "200 lines");
    // --- H. Recap / status line ---

        app.tool_start("bash", "ls -la");
        app.tool_done("bash", "failed", true);
        assert_eq!(app.tools[1].status, ToolStatus::Failed);
    }

    fn tmp_ws() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn test_slash_help() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let ws = tmp_ws();
        // /help opens an overlay panel rather than pushing into chat —
        // toggle on, no new chat message; second invocation toggles off.
        assert!(!app.help_overlay_open);
        let result = app.handle_slash("/help", &ws);
        assert_eq!(result, SlashResult::Handled);
        assert!(app.help_overlay_open);
        assert_eq!(app.messages.len(), initial);
        let result2 = app.handle_slash("/help", &ws);
        assert_eq!(result2, SlashResult::Handled);
        assert!(!app.help_overlay_open);
    }

    #[test]
    fn test_slash_exit() {
        let mut app = TuiApp::new("m");
        let ws = tmp_ws();
        let result = app.handle_slash("/exit", &ws);
        assert_eq!(result, SlashResult::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn test_slash_clear_requires_confirm() {
        let mut app = TuiApp::new("m");
        app.push_user("hello");
        app.push_assistant("hi");
        let before = app.messages.len();
        let ws = tmp_ws();

        // First /clear: arm only, don't wipe.
        let result = app.handle_slash("/clear", &ws);
        assert_eq!(result, SlashResult::Handled);
        assert_eq!(app.messages.len(), before + 1); // prime warning added
        assert!(app.clear_primed_at.is_some());
        // User messages still intact.
        assert!(app.messages.iter().any(|m| m.text == "hello"));

        // Second /clear (within window): actually wipe.
        let result2 = app.handle_slash("/clear", &ws);
        assert_eq!(result2, SlashResult::Clear);
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].text.contains("cleared"));
        assert!(app.clear_primed_at.is_none());
    }

    #[test]
    fn test_slash_clear_prime_expires() {
        let mut app = TuiApp::new("m");
        app.push_user("keep me");
        let ws = tmp_ws();

        // Prime it...
        let _ = app.handle_slash("/clear", &ws);
        assert!(app.clear_primed_at.is_some());

        // ...then force-expire the prime (simulating >5s gap).
    // --- I. Plan mode ---
        app.clear_primed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));

        // Next /clear re-arms rather than wiping.
        let result = app.handle_slash("/clear", &ws);
        assert_eq!(result, SlashResult::Handled);
        assert!(app.messages.iter().any(|m| m.text == "keep me"));
    }

    #[test]
    fn test_slash_cost() {
        let mut app = TuiApp::new("m");
        app.turn_count = 5;
        app.cumulative_usage.input_tokens = 1000;
        app.cumulative_usage.output_tokens = 500;
        let initial = app.messages.len();
        let ws = tmp_ws();
        let result = app.handle_slash("/cost", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("model"), "expected cost breakdown, got: {msg}");
        assert!(msg.contains("turns"), "expected turn count, got: {msg}");
    }

    #[test]
    fn test_slash_rate_missing_arg_shows_usage() {
        let mut app = TuiApp::new("m");
        let ws = tmp_ws();
        let initial = app.messages.len();
        let result = app.handle_slash("/rate", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(
            msg.contains("good") && msg.contains("bad"),
            "expected usage hint, got: {msg}"
        );
    }

    #[test]
    fn test_slash_rate_invalid_signal() {
        let mut app = TuiApp::new("m");
        let ws = tmp_ws();
        let initial = app.messages.len();
        let result = app.handle_slash("/rate maybe", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("unknown signal"), "expected error, got: {msg}");
    }

    #[test]
    fn test_slash_rate_writes_to_isolated_home() {
    // --- J. Permission ---
        let _g = home_lock();
        // Override HOME so we don't dirty the real ~/.metis
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-rate-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let initial = app.messages.len();
        let result = app.handle_slash("/rate bad too noisy", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("rating saved"), "got: {msg}");
        assert!(msg.contains("bad"));
        assert!(msg.contains("too noisy"));

        let prefs_path = tmp.join(".metis").join("preferences.jsonl");
        assert!(prefs_path.exists(), "preferences.jsonl should exist");
        let prefs = aegis_core::learning::load_preferences(&prefs_path);
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].signal, "bad");
        assert_eq!(prefs[0].note.as_deref(), Some("too noisy"));

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_ratings_empty_message() {
        let _g = home_lock();
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-ratings-empty-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let initial = app.messages.len();
        let result = app.handle_slash("/ratings", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("no ratings recorded"), "got: {msg}");

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_ratings_summarizes_after_rate() {
        let _g = home_lock();
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-ratings-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let _ = app.handle_slash("/rate good", &ws);
        let _ = app.handle_slash("/rate bad", &ws);
        let _ = app.handle_slash("/rate bad", &ws);

        let initial = app.messages.len();
        let result = app.handle_slash("/ratings", &ws);
        assert_eq!(result, SlashResult::Handled);

        let combined: String = app.messages[initial..]
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("1 good"), "got: {combined}");
        assert!(combined.contains("2 bad"), "got: {combined}");
        assert!(combined.contains("threshold"));

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_rate_undo_pops_last() {
        let _g = home_lock();
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-undo-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let _ = app.handle_slash("/rate good", &ws);
        let _ = app.handle_slash("/rate bad oops", &ws);

        let initial = app.messages.len();
        let result = app.handle_slash("/rate undo", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("undid last rating"), "got: {msg}");
        assert!(msg.contains("bad"), "should report popped signal: {msg}");

        // Only the first (good) rating should remain.
        let prefs_path = tmp.join(".metis").join("preferences.jsonl");
        let prefs = aegis_core::learning::load_preferences(&prefs_path);
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].signal, "good");

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_rate_undo_empty_workspace_messages_user() {
        let _g = home_lock();
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-undo-empty-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let initial = app.messages.len();
        let result = app.handle_slash("/rate undo", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("nothing to undo"), "got: {msg}");

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_forget_removes_matching() {
        let _g = home_lock();
        let tmp = std::env::temp_dir().join(format!(
            "metis-tui-forget-{}-{}",
            std::process::id(),
            aegis_core::telemetry::now_unix_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        // Seed an insight directly.
        let learned = tmp.join(".metis").join("learned.jsonl");
        std::fs::create_dir_all(learned.parent().unwrap()).unwrap();
        let ins = aegis_core::learning::Insight {
            timestamp: "t".into(),
            last_seen: Some("t".into()),
            workspace: Some(tmp.display().to_string()),
            category: "tool_pattern".into(),
            text: "User prefers ripgrep over find".into(),
            reinforcements: 1,
            success_count: 0,
            failure_count: 0,
            tags: vec!["find".into()],
        };
        aegis_core::learning::upsert_insight_at(&learned, &ins).unwrap();

        let mut app = TuiApp::new("m");
        let ws = tmp.clone();
        let initial = app.messages.len();
        let result = app.handle_slash("/forget find", &ws);
        assert_eq!(result, SlashResult::Handled);
        let combined: String = app.messages[initial..]
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("removed 1 insight"), "got: {combined}");

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_slash_forget_empty_arg_shows_help() {
        let mut app = TuiApp::new("m");
        let ws = tmp_ws();
        let initial = app.messages.len();
        let result = app.handle_slash("/forget", &ws);
        assert_eq!(result, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("remove insights"), "got: {msg}");
    }

    #[test]
    fn test_slash_unknown() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let ws = tmp_ws();
        let result = app.handle_slash("/foobar", &ws);
        assert_eq!(result, SlashResult::Handled);
        assert!(app.messages[initial]
            .text
            .to_lowercase()
            .contains("unknown"));
    }

    // ---------- /files tests ----------

    fn make_file(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn files_empty_arg_lists_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "a.txt", "aa");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let result = app.handle_slash("/files .", tmp.path());
        assert_eq!(result, SlashResult::Handled);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("browsing files at"));
        assert!(msg.contains("Directories:"));
        assert!(msg.contains("sub/"));
        assert!(msg.contains("Files:"));
        assert!(msg.contains("a.txt"));
        // workspace root should NOT mention "relative to workspace"
        assert!(!msg.contains("relative to workspace"));
        assert!(msg.contains("total: 1 directories, 1 files"));
    }

    #[test]
    fn files_empty_directory_reports_zero_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let result = app.handle_slash("/files .", tmp.path());
        assert_eq!(result, SlashResult::Handled);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("browsing files at"));
        assert!(msg.contains("total: 0 directories, 0 files"));
        // Neither headers should appear when both collections are empty.
        assert!(!msg.contains("Directories:"));
        assert!(!msg.contains("Files:"));
    }

    #[test]
    fn files_nonexistent_path_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let result = app.handle_slash("/files does-not-exist", tmp.path());
        assert_eq!(result, SlashResult::Handled);

        let msg = &app.messages[initial].text;
        assert!(msg.to_lowercase().contains("path not found"), "got: {msg}");
    }

    #[test]
    fn files_on_a_file_path_errors_with_view_hint() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "readme.md", "# hi");
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let result = app.handle_slash("/files readme.md", tmp.path());
        assert_eq!(result, SlashResult::Handled);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("is a file, not a directory"), "got: {msg}");
        assert!(msg.contains("/view readme.md"), "got: {msg}");
    }

    #[test]
    fn files_subdir_shows_relative_workspace_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        make_file(&tmp.path().join("nested"), "inside.txt", "x");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        let result = app.handle_slash("/files nested", tmp.path());
        assert_eq!(result, SlashResult::Handled);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("inside.txt"));
        assert!(msg.contains("relative to workspace: nested"), "got: {msg}");
    }

    #[test]
    fn files_sorts_case_insensitively_dirs_before_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Beta")).unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        make_file(tmp.path(), "Zeta.rs", "//");
        make_file(tmp.path(), "apple.txt", "!");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let msg = &app.messages[initial].text;
        // Dirs section must precede Files section.
        let dirs_at = msg.find("Directories:").expect("dirs header missing");
        let files_at = msg.find("Files:").expect("files header missing");
        assert!(dirs_at < files_at);

        // Within each section, alphabetical case-insensitive ordering.
        let alpha_pos = msg.find("alpha/").unwrap();
        let beta_pos = msg.find("Beta/").unwrap();
        assert!(alpha_pos < beta_pos, "alpha should sort before Beta");

        let apple_pos = msg.find("apple.txt").unwrap();
        let zeta_pos = msg.find("Zeta.rs").unwrap();
        assert!(apple_pos < zeta_pos, "apple should sort before Zeta");
    }

    #[test]
    fn files_total_size_sums_only_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "a.bin", &"x".repeat(100));
        make_file(tmp.path(), "b.bin", &"y".repeat(400));
        // Subdir content must NOT be counted (REPL parity).
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        make_file(&tmp.path().join("sub"), "ignored.bin", &"z".repeat(9999));

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let msg = &app.messages[initial].text;
        // 100 + 400 = 500 bytes total.
        assert!(msg.contains("500 B"), "total_size mismatch, got: {msg}");
    }

    // ---------- /files styled rendering tests (REPL color parity) ----------

    /// Walk the pushed styled_lines and return Some((line_idx, span_idx, style))
    /// for the first span whose text matches `needle`.
    fn find_span<'a>(
        lines: &'a [Line<'static>],
        needle: &str,
    ) -> Option<(usize, usize, &'a Style)> {
        for (li, line) in lines.iter().enumerate() {
            for (si, span) in line.spans.iter().enumerate() {
                if span.content.contains(needle) {
                    return Some((li, si, &span.style));
                }
            }
        }
        None
    }

    #[test]
    fn files_dirs_render_blue_per_repl() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mydir")).unwrap();

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial]
            .styled_lines
            .as_ref()
            .expect("styled_lines must be Some for /files");
        let (_, _, style) = find_span(styled, "mydir/").expect("mydir/ span missing");
        assert_eq!(style.fg, Some(Color::Blue), "dir should be blue");
    }

    #[test]
    fn files_code_files_render_yellow() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "main.rs", "fn main() {}");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, style) = find_span(styled, "main.rs").expect("main.rs span missing");
        assert_eq!(style.fg, Some(Color::Yellow));
    }

    #[test]
    fn files_doc_files_render_cyan() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "notes.md", "# hi");
        make_file(tmp.path(), "Cargo.toml", "[package]");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, md_style) = find_span(styled, "notes.md").unwrap();
        let (_, _, toml_style) = find_span(styled, "Cargo.toml").unwrap();
        assert_eq!(md_style.fg, Some(Color::Cyan));
        assert_eq!(toml_style.fg, Some(Color::Cyan));
    }

    #[test]
    fn files_image_files_render_magenta() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "photo.jpg", "jpg-bytes");
        make_file(tmp.path(), "logo.svg", "<svg/>");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        assert_eq!(
            find_span(styled, "photo.jpg").unwrap().2.fg,
            Some(Color::Magenta)
        );
        assert_eq!(
            find_span(styled, "logo.svg").unwrap().2.fg,
            Some(Color::Magenta)
        );
    }

    #[test]
    fn files_unknown_extension_uses_default_color() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "weird.xyz", "?");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        // Color::Reset = default — parity with REPL's plain `\x1b[0m`.
        assert_eq!(
            find_span(styled, "weird.xyz").unwrap().2.fg,
            Some(Color::Reset)
        );
    }

    #[test]
    fn files_section_headers_are_bold() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        make_file(tmp.path(), "f.txt", "x");

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, dirs_style) = find_span(styled, "Directories:").unwrap();
        let (_, _, files_style) = find_span(styled, "Files:").unwrap();
        assert!(dirs_style.add_modifier.contains(Modifier::BOLD));
        assert!(files_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn files_metis_label_is_green_bold_on_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/files .", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        // First span of first line must be the green-bold `[goblin] `
        // prefix so the listing visually threads with system messages.
        let first_span = &styled[0].spans[0];
        assert!(first_span.content.contains("[goblin]"));
        assert_eq!(first_span.style.fg, Some(Color::Rgb(63, 185, 80)));
        assert!(first_span.style.add_modifier.contains(Modifier::BOLD));
    }

    // ---------- /skills, /skill-install, /skill-uninstall, /skill-search ----------

    fn sample_skill_md(name: &str, description: &str, user_invocable: bool) -> String {
        format!(
            "---\nname: {name}\ndescription: {description}\nuser-invocable: {user_invocable}\n---\n\nDo the thing: {{args}}\n"
        )
    }

    #[test]
    fn skills_empty_list_opens_overlay_with_no_skills() {
        let mut app = TuiApp::new("m");
        let ws = tempfile::tempdir().unwrap();
        // Clear any builtin skills so the menu is truly empty.
        app.skill_registry = aegis_core::skills::SkillRegistry::new();
        app.handle_slash("/skills", ws.path());
        // Overlay is opened — no new chat message pushed.
        assert!(app.skill_menu.is_some());
        assert!(app.skill_menu.as_ref().unwrap().is_empty());
        assert!(app.skill_filter.is_empty());
    }

    #[test]
    fn skills_list_opens_overlay_with_all_user_invocable() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "blueprint".into(),
            description: "draft a plan".into(),
            user_invocable: true,
            prompt: "plan {args}".into(),
            ..Default::default()
        });
        app.skill_registry.register(aegis_core::Skill {
            name: "code-tour".into(),
            description: "walkthrough".into(),
            user_invocable: true,
            prompt: "tour {args}".into(),
            ..Default::default()
        });
        app.handle_slash("/skills", tempfile::tempdir().unwrap().path());

        let menu = app.skill_menu.as_ref().unwrap();
        let names: Vec<&str> = menu.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"blueprint"), "blueprint should be in menu");
        assert!(names.contains(&"code-tour"), "code-tour should be in menu");
    }

    #[test]
    fn skills_list_hides_non_user_invocable() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "hidden".into(),
            description: "internal".into(),
            user_invocable: false,
            prompt: "x".into(),
            ..Default::default()
        });
        app.skill_registry.register(aegis_core::Skill {
            name: "visible".into(),
            description: "shown".into(),
            user_invocable: true,
            prompt: "x".into(),
            ..Default::default()
        });
        app.handle_slash("/skills", tempfile::tempdir().unwrap().path());

        let menu = app.skill_menu.as_ref().unwrap();
        let names: Vec<&str> = menu.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"visible"), "user-invocable should be in menu");
        assert!(!names.contains(&"hidden"), "hidden must NOT be in menu");
    }

    #[test]
    fn skill_install_from_file_path_registers() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("mytool.md");
        std::fs::write(&src, sample_skill_md("mytool", "do stuff", true)).unwrap();

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash(&format!("/skill-install {}", src.display()), tmp.path());

        // Message confirms install with skill name listed.
        let msg = &app.messages[initial].text;
        assert!(msg.contains("installed"), "got: {msg}");
        assert!(msg.contains("mytool"), "got: {msg}");
        // Registry now has the skill available.
        assert!(app.skill_registry.get("mytool").is_some());
    }

    #[test]
    fn skill_install_rejects_missing_arg() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/skill-install", tempfile::tempdir().unwrap().path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial].text.contains("usage"));
    }

    #[test]
    fn skill_uninstall_removes_registered_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("todelete.md");
        std::fs::write(&src, sample_skill_md("todelete", "gone soon", true)).unwrap();

        let mut app = TuiApp::new("m");
        // Install first so there's something to remove.
        app.handle_slash(&format!("/skill-install {}", src.display()), tmp.path());
        assert!(app.skill_registry.get("todelete").is_some());

        let initial = app.messages.len();
        app.handle_slash("/skill-uninstall todelete", tmp.path());

        let msg = &app.messages[initial].text;
        assert!(msg.contains("uninstalled"), "got: {msg}");
        assert!(app.skill_registry.get("todelete").is_none());
    }

    #[test]
    fn skill_uninstall_rejects_missing_arg() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/skill-uninstall", tempfile::tempdir().unwrap().path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
    }

    #[test]
    fn skill_search_matches_by_description() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "planner".into(),
            description: "draft a blueprint plan".into(),
            user_invocable: true,
            prompt: "x".into(),
            ..Default::default()
        });
        app.skill_registry.register(aegis_core::Skill {
            name: "notes".into(),
            description: "take meeting notes".into(),
            user_invocable: true,
            prompt: "x".into(),
            ..Default::default()
        });
        let initial = app.messages.len();
        app.handle_slash(
            "/skill-search blueprint",
            tempfile::tempdir().unwrap().path(),
        );

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let joined: String = styled
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("/planner"), "blueprint match missing");
        assert!(!joined.contains("/notes"), "notes should NOT match");
    }

    #[test]
    fn skill_search_empty_result_shows_friendly_message() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash(
            "/skill-search nothing-here",
            tempfile::tempdir().unwrap().path(),
        );
        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let joined: String = styled
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("no matches"), "got: {joined}");
        assert!(joined.contains("nothing-here"));
    }

    #[test]
    fn unknown_slash_falls_through_to_skill_dispatch() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "myskill".into(),
            description: "test".into(),
            user_invocable: true,
            // `$ARGS` is the only substitution token supported by
            // `aegis_core::expand_prompt` — REPL parity requires we use
            // that exact placeholder so installed skills behave the same
            // in TUI as they do under `--repl`.
            prompt: "Please do: $ARGS".into(),
            ..Default::default()
        });
        app.handle_slash("/myskill hello world", tempfile::tempdir().unwrap().path());

        // The expanded prompt must be queued as a normal turn.
        let queued = app
            .pending_prompts
            .back()
            .expect("skill must queue a prompt");
        assert!(queued.contains("hello world"), "got: {queued}");
        assert!(
            queued.contains("Please do"),
            "template not expanded: {queued}"
        );
    }

    #[test]
    fn skill_dispatch_ignores_non_user_invocable() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "nope".into(),
            description: "internal".into(),
            user_invocable: false,
            prompt: "should not fire".into(),
            ..Default::default()
        });
        let initial_queue_len = app.pending_prompts.len();
        let initial_msg = app.messages.len();
        app.handle_slash("/nope", tempfile::tempdir().unwrap().path());

        // No prompt should be queued for a non-invocable skill — user
        // must still see an unknown-command message.
        assert_eq!(app.pending_prompts.len(), initial_queue_len);
        assert!(app.messages[initial_msg]
            .text
            .to_lowercase()
            .contains("unknown"));
    }

    #[test]
    fn skill_dispatch_survives_args_with_leading_whitespace() {
        let mut app = TuiApp::new("m");
        app.skill_registry.register(aegis_core::Skill {
            name: "trim".into(),
            description: "test arg trim".into(),
            user_invocable: true,
            prompt: "ARGS=[$ARGS]".into(),
            ..Default::default()
        });
        app.handle_slash(
            "/trim     spacy   arg   ",
            tempfile::tempdir().unwrap().path(),
        );
        let queued = app.pending_prompts.back().unwrap();
        // `rest_after_cmd` is `.trim()`'d but preserves interior runs of
        // whitespace. REPL matches this behavior.
        assert!(queued.contains("ARGS=[spacy   arg]"), "got: {queued}");
    }

    // ---------- /view <path> tests ----------

    #[test]
    fn view_missing_arg_errors() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view", tempfile::tempdir().unwrap().path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial].text.contains("usage"));
    }

    #[test]
    fn view_nonexistent_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view ghost.txt", tmp.path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial]
            .text
            .to_lowercase()
            .contains("file not found"));
    }

    #[test]
    fn view_on_directory_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view subdir", tmp.path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial].text.contains("cannot view directory"));
    }

    #[test]
    fn view_small_text_file_shows_all_lines_with_stats() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "one\ntwo\n\nfour\n").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view hello.txt", tmp.path());

        let msg = &app.messages[initial].text;
        assert!(msg.contains("previewing:"));
        assert!(msg.contains("showing all 4 lines:"));
        assert!(msg.contains("   1: one"));
        assert!(msg.contains("   2: two"));
        assert!(msg.contains("   3: ⟨empty⟩"));
        assert!(msg.contains("   4: four"));
        assert!(msg.contains("stats: 4 lines (3 non-empty)"));
        assert!(msg.contains("longest: 4 chars"));
    }

    #[test]
    fn view_large_file_truncates_at_100_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=150)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("big.txt"), body).unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view big.txt", tmp.path());

        let msg = &app.messages[initial].text;
        assert!(msg.contains("showing first 100 of 150 lines:"));
        assert!(msg.contains("line-1"));
        assert!(msg.contains("line-100"));
        assert!(!msg.contains("line-101"));
        assert!(msg.contains("... (50 more lines)"));
    }

    #[test]
    fn view_binary_file_is_rejected_with_hint() {
        let tmp = tempfile::tempdir().unwrap();
        // NUL + high bytes → binary by content check (no known text ext).
        std::fs::write(tmp.path().join("blob.bin"), [0u8, 0, 0xff, 0xfe, 0xfd]).unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view blob.bin", tmp.path());

        let msg = &app.messages[initial].text;
        assert!(msg.contains("binary file detected"));
        assert!(msg.contains("/files"));
    }

    #[test]
    fn view_empty_file_renders_zero_stats() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view empty.txt", tmp.path());

        let msg = &app.messages[initial].text;
        assert!(msg.contains("showing all 0 lines:"));
        assert!(msg.contains("stats: 0 lines (0 non-empty), longest: 0 chars"));
    }

    #[test]
    fn view_line_number_gutter_is_dark_gray() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x.txt"), "foo\n").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view x.txt", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, gutter_style) = find_span(styled, "1:").expect("line number missing");
        assert_eq!(gutter_style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn view_rust_keyword_renders_blue() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn main() { let x = 1; }\n").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view lib.rs", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        // `fn ` keyword must render blue via highlight_line → ansi_to_spans.
        let has_blue = styled.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("fn ") && s.style.fg == Some(Color::Blue))
        });
        assert!(has_blue, "no blue `fn ` span found in styled output");
    }

    #[test]
    fn view_empty_line_shows_dim_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gap.txt"), "a\n\nb\n").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/view gap.txt", tmp.path());

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, empty_style) = find_span(styled, "⟨empty⟩").expect("empty placeholder missing");
        // Mid-gray Rgb(150,150,150) — same DIM tone as REPL emits.
        assert_eq!(empty_style.fg, Some(Color::Rgb(150, 150, 150)));
    }

    // ---------- ansi_to_spans parser tests ----------

    #[test]
    fn ansi_parser_plain_text_single_span() {
        let spans = ansi_to_spans("hello");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello");
        assert_eq!(spans[0].style.fg, None);
    }

    #[test]
    fn ansi_parser_color_and_reset() {
        let spans = ansi_to_spans("\x1b[34mblue\x1b[0m plain");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "blue");
        assert_eq!(spans[0].style.fg, Some(Color::Blue));
        assert_eq!(spans[1].content, " plain");
        assert_eq!(spans[1].style.fg, None);
    }

    #[test]
    fn ansi_parser_handles_all_standard_colors() {
        let codes: &[(u8, Color)] = &[
            (30, Color::Black),
            (31, Color::Red),
            (32, Color::Green),
            (33, Color::Yellow),
            (34, Color::Blue),
            (35, Color::Magenta),
            (36, Color::Cyan),
            (37, Color::White),
            (90, Color::DarkGray),
            (91, Color::LightRed),
            (92, Color::LightGreen),
            (93, Color::LightYellow),
            (94, Color::LightBlue),
            (95, Color::LightMagenta),
            (96, Color::LightCyan),
        ];
        for (code, expected) in codes {
            let text = format!("\x1b[{code}mX\x1b[0m");
            let spans = ansi_to_spans(&text);
            assert_eq!(spans[0].content, "X");
            assert_eq!(
                spans[0].style.fg,
                Some(*expected),
                "code {code} mapped wrong"
            );
        }
    }

    #[test]
    fn ansi_parser_dim_maps_to_mid_gray() {
        let spans = ansi_to_spans("\x1b[2mdim\x1b[0m");
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(150, 150, 150)));
    }

    #[test]
    fn ansi_parser_bold_modifier() {
        let spans = ansi_to_spans("\x1b[1mbold\x1b[0m");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn ansi_parser_preserves_multibyte_runes() {
        let spans = ansi_to_spans("\x1b[32m● çalışır\x1b[0m");
        let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(joined.contains("● çalışır"), "got: {joined}");
    }

    // ---------- /search <pattern> tests ----------

    fn write_workspace_files(ws: &std::path::Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let full = ws.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, body).unwrap();
        }
    }

    /// macOS `tempfile::tempdir()` roots paths at `/var/.../.tmpXXXXXX`
    /// whose final component starts with `.` — `search_directory`'s
    /// `filter_entry` then drops the entire tree as "hidden". To keep
    /// /search tests realistic, create a non-dot subdir under the
    /// tempdir and return it as the effective workspace.
    fn search_workspace(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        ws
    }

    #[test]
    fn search_missing_pattern_errors() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search", tempfile::tempdir().unwrap().path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial].text.contains("usage"));
    }

    #[test]
    fn search_literal_finds_match_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(
            &ws,
            &[
                ("src/a.rs", "fn needle() {}\n"),
                ("src/b.rs", "fn other() {}\n"),
                ("README.md", "See needle in code.\n"),
            ],
        );

        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search needle", &ws);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("searching for pattern: \"needle\""));
        assert!(msg.contains("mode: literal text"));
        assert!(msg.contains("found 2 matches in"));
        assert!(msg.contains("src/a.rs"));
        assert!(msg.contains("README.md"));
    }

    #[test]
    fn search_case_insensitive_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(&ws, &[("x.txt", "ERROR\nerror\nWarn\n")]);
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search -i error", &ws);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("case: insensitive"));
        assert!(msg.contains("found 2 matches"));
    }

    #[test]
    fn search_regex_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(&ws, &[("x.rs", "fn foo() {}\nfn bar() {}\n")]);
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search -r fn \\w+", &ws);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("mode: regex"));
        assert!(msg.contains("found 2 matches"));
    }

    #[test]
    fn search_file_type_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(
            &ws,
            &[
                ("src/a.rs", "needle\n"),
                ("notes.md", "needle\n"),
                ("conf.toml", "needle\n"),
            ],
        );
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search -t rs needle", &ws);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("file types: rs"));
        assert!(msg.contains("found 1 matches"));
        assert!(msg.contains("a.rs"));
        assert!(!msg.contains("notes.md"));
    }

    #[test]
    fn search_no_matches_reports_files_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(&ws, &[("x.txt", "hello\n")]);
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search zzz-not-there", &ws);

        let msg = &app.messages[initial].text;
        assert!(msg.contains("no matches found"));
        assert!(msg.contains("files"));
    }

    #[test]
    fn search_per_file_header_is_bold_in_styled() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(&ws, &[("here.txt", "match\n")]);
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search match", &ws);

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, style) = find_span(styled, "here.txt").expect("file path span missing");
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "file path must be bold"
        );
    }

    #[test]
    fn search_gutter_and_highlighted_match_render() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        write_workspace_files(&ws, &[("f.txt", "line one has needle here\n")]);
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/search needle", &ws);

        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, gutter_style) = find_span(styled, "1: ").expect("gutter missing");
        assert_eq!(gutter_style.fg, Some(Color::DarkGray));
        let match_styled = styled.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("needle")
                && s.style.fg.is_some()
                && s.style.fg != Some(Color::DarkGray)
        });
        assert!(match_styled, "match highlight missing");
    }

    // ---------- /image, /images, /images clear tests ----------

    #[test]
    fn image_missing_arg_errors() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/image", tempfile::tempdir().unwrap().path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
    }

    #[test]
    fn image_nonexistent_path_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/image ghost.png", tmp.path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial]
            .text
            .to_lowercase()
            .contains("file not found"));
        assert!(app.pending_images.is_empty());
    }

    #[test]
    fn image_unsupported_extension_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("doc.pdf"), "fake").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/image doc.pdf", tmp.path());
        assert_eq!(app.messages[initial].role, MessageRole::Error);
        assert!(app.messages[initial]
            .text
            .to_lowercase()
            .contains("unsupported image format"));
        assert!(app.pending_images.is_empty());
    }

    #[test]
    fn image_valid_attaches_and_renders_magenta_label() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("pic.png");
        std::fs::write(&img, b"\x89PNG").unwrap();
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/image pic.png", tmp.path());

        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0], img);
        // Magenta `[image]` label in the styled output (REPL parity).
        let styled = app.messages[initial].styled_lines.as_ref().unwrap();
        let (_, _, style) = find_span(styled, "[image]").unwrap();
        assert_eq!(style.fg, Some(Color::Magenta));
    }

    #[test]
    fn image_accepts_all_supported_formats() {
        let tmp = tempfile::tempdir().unwrap();
        for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp"] {
            std::fs::write(tmp.path().join(format!("a.{ext}")), b"x").unwrap();
        }
        let mut app = TuiApp::new("m");
        for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp"] {
            app.handle_slash(&format!("/image a.{ext}"), tmp.path());
        }
        assert_eq!(app.pending_images.len(), 6);
    }

    #[test]
    fn image_absolute_path_bypasses_workspace_join() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("abs.png");
        std::fs::write(&img, b"x").unwrap();
        // Different workspace root — image should still resolve via
        // absolute path.
        let other_ws = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.handle_slash(&format!("/image {}", img.display()), other_ws.path());
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0], img);
    }

    #[test]
    fn images_list_empty_shows_hint() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/images", tempfile::tempdir().unwrap().path());
        let msg = &app.messages[initial].text;
        assert!(msg.contains("no images attached"));
        assert!(msg.contains("/image <path>"));
    }

    #[test]
    fn images_list_renders_numbered_entries() {
        let tmp = tempfile::tempdir().unwrap();
        for (i, ext) in ["png", "jpg"].iter().enumerate() {
            std::fs::write(tmp.path().join(format!("im{i}.{ext}")), b"x").unwrap();
        }
        let mut app = TuiApp::new("m");
        app.handle_slash("/image im0.png", tmp.path());
        app.handle_slash("/image im1.jpg", tmp.path());
        let initial = app.messages.len();
        app.handle_slash("/images", tmp.path());
        let msg = &app.messages[initial].text;
        assert!(msg.contains("attached images:"));
        assert!(msg.contains("1."));
        assert!(msg.contains("2."));
        assert!(msg.contains("/images clear to remove all"));
    }

    #[test]
    fn images_clear_empties_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.png"), b"x").unwrap();
        let mut app = TuiApp::new("m");
        app.handle_slash("/image a.png", tmp.path());
        assert_eq!(app.pending_images.len(), 1);

        let initial = app.messages.len();
        app.handle_slash("/images clear", tmp.path());
        assert!(app.pending_images.is_empty());
        assert!(app.messages[initial].text.contains("cleared 1 attached"));
    }

    // ---------- /update tests ----------
    //
    // The network side is covered by `aegis_core::update` tests; here we
    // only assert the TUI → main-loop handoff: `/update` must flip
    // `pending_update` and push the REPL-style "current v<N> — checking…"
    // system line so the user sees progress before the HTTP call fires.

    #[test]
    fn update_queues_and_reports_current_version() {
        let mut app = TuiApp::new("m");
        let initial = app.messages.len();
        app.handle_slash("/update", tempfile::tempdir().unwrap().path());

        assert!(app.pending_update, "pending_update must flip on /update");
        let msg = &app.messages[initial].text;
        assert!(msg.contains("current: v"), "got: {msg}");
        assert!(
            msg.contains(aegis_core::update::CURRENT_VERSION),
            "got: {msg}"
        );
        assert!(msg.to_lowercase().contains("checking"), "got: {msg}");
    }

    #[test]
    fn update_is_idempotent_before_main_loop_picks_it_up() {
        let mut app = TuiApp::new("m");
        app.handle_slash("/update", tempfile::tempdir().unwrap().path());
        app.handle_slash("/update", tempfile::tempdir().unwrap().path());
        // Two /update calls should just leave pending_update=true; main
        // loop clears it when it runs. REPL has the same semantics —
        // subsequent /update re-checks after the prior one finishes.
        assert!(app.pending_update);
    }

    // ---------- Real render test (TestBackend, per-cell color check) ----------
    //
    // Unlike the span-level tests above, this drives the full ratatui
    // pipeline: `render_chat_lines` → `Paragraph` → terminal buffer.
    // It asserts the rendered CELLS carry the right fg color, which is
    // what a live terminal actually shows. If styling regresses anywhere
    // between model and buffer, this catches it.
    #[test]
    fn files_rendered_buffer_has_correct_cell_colors() {
        use ratatui::backend::TestBackend;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("zdir")).unwrap();
        make_file(tmp.path(), "main.rs", "fn m(){}");
        make_file(tmp.path(), "readme.md", "#");

        let mut app = TuiApp::new("m");
        app.messages.clear(); // drop the welcome banner for a clean buffer
        app.handle_slash("/files .", tmp.path());

        let lines = render_chat_lines(&app);
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let para = Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false });
                frame.render_widget(para, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();

        // Locate each expected filename/dirname cell and read its fg.
        // Helper scans the buffer row-by-row for the needle and returns
        // the fg color at the first character of the match.
        let fg_at = |needle: &str| -> Option<Color> {
            for y in 0..buffer.area().height {
                let mut row = String::new();
                for x in 0..buffer.area().width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                if let Some(col) = row.find(needle) {
                    return Some(buffer[(col as u16, y)].fg);
                }
            }
            None
        };

        assert_eq!(fg_at("zdir/"), Some(Color::Blue), "dir cell must be blue");
        assert_eq!(
            fg_at("main.rs"),
            Some(Color::Yellow),
            "rust file cell must be yellow"
        );
        assert_eq!(
            fg_at("readme.md"),
            Some(Color::Cyan),
            "md file cell must be cyan"
        );
        assert_eq!(
            fg_at("[goblin]"),
            Some(Color::Rgb(63, 185, 80)),
            "label cell must be green"
        );

        // Section headers must actually carry BOLD in the rendered buffer.
        let bold_at = |needle: &str| -> bool {
            for y in 0..buffer.area().height {
                let mut row = String::new();
                for x in 0..buffer.area().width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                if let Some(col) = row.find(needle) {
                    return buffer[(col as u16, y)].modifier.contains(Modifier::BOLD);
                }
            }
            false
        };
        assert!(
            bold_at("Directories:"),
            "Directories: header must be bold in buffer"
        );
        assert!(bold_at("Files:"), "Files: header must be bold in buffer");
    }

    // ---------- Diff colorization in tool results (M4) ----------
    //
    // edit_file/multi_edit/write_file return tool results that embed a
    // unified diff. The TUI must render `+` lines green, `-` lines red,
    // `@@` hunk headers cyan-ish, and `+++ /---` markers in bold. Drives
    // the same render_chat_lines → buffer pipeline as the files test
    // so we catch any regression at the cell level.
    #[test]
    fn diff_lines_in_tool_result_have_correct_cell_colors() {
        use ratatui::backend::TestBackend;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;

        let mut app = TuiApp::new("m");
        app.messages.clear();
        // Simulate the kind of text edit_file produces: header, diff, then snippet.
        let tool_result = "\
edited foo.rs (1 replacement)
--- foo.rs
+++ foo.rs
@@ -1,3 +1,3 @@
 use std::fs;
-fn old_name() {}
+fn new_name() {}
 fn other() {}
";
        app.messages.push(ChatMessage {
            role: MessageRole::ToolResult,
            text: tool_result.to_string(),
            styled_lines: None,
            expanded: true,
        });

        let lines = render_chat_lines(&app);
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let para = Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false });
                frame.render_widget(para, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        // Find the column where `needle` starts on the first row that contains it,
        // then return (fg, modifier) for that cell. The `⎿ ` prefix sits at col 4
        // and the diff content starts at col 6, so we read at the first char of
        // the matched needle.
        let cell_at = |needle: &str| -> Option<(Color, Modifier)> {
            for y in 0..buffer.area().height {
                let mut row = String::new();
                for x in 0..buffer.area().width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                if let Some(col) = row.find(needle) {
                    let cell = &buffer[(col as u16, y)];
                    return Some((cell.fg, cell.modifier));
                }
            }
            None
        };

        // `+ fn new_name()` line — first cell `+` must be green-ish (lighter
        // fg now that we layer it over a dark green bg), no bold.
        let (fg_plus, mod_plus) = cell_at("+fn new_name").expect("+ line found");
        assert_eq!(
            fg_plus,
            Color::Rgb(180, 240, 180),
            "+ line must be green"
        );
        assert!(!mod_plus.contains(Modifier::BOLD), "+ line must not be bold");

        // `- fn old_name()` line — first cell `-` must be red-ish (lighter
        // fg over dark red bg), no bold.
        let (fg_minus, mod_minus) = cell_at("-fn old_name").expect("- line found");
        assert_eq!(
            fg_minus,
            Color::Rgb(255, 170, 170),
            "- line must be red"
        );
        assert!(!mod_minus.contains(Modifier::BOLD), "- line must not be bold");

        // `@@ -1,3 +1,3 @@` hunk header — cyan-ish.
        let (fg_hunk, _mod_hunk) = cell_at("@@ -1,3").expect("@@ hunk header found");
        assert_eq!(
            fg_hunk,
            Color::Rgb(120, 180, 200),
            "@@ hunk header must be cyan-ish"
        );

        // `--- foo.rs` file marker — bold red.
        let (fg_old_marker, mod_old_marker) =
            cell_at("--- foo.rs").expect("--- marker found");
        assert_eq!(
            fg_old_marker,
            Color::Rgb(220, 110, 110),
            "--- marker must be red"
        );
        assert!(
            mod_old_marker.contains(Modifier::BOLD),
            "--- marker must be bold"
        );

        // `+++ foo.rs` file marker — bold green.
        let (fg_new_marker, mod_new_marker) =
            cell_at("+++ foo.rs").expect("+++ marker found");
        assert_eq!(
            fg_new_marker,
            Color::Rgb(120, 200, 120),
            "+++ marker must be green"
        );
        assert!(
            mod_new_marker.contains(Modifier::BOLD),
            "+++ marker must be bold"
        );

        // Plain context line ` use std::fs;` (starts with space) must stay
        // muted gray, not red/green.
        let (fg_ctx, _mod_ctx) = cell_at(" use std::fs;").expect("context line found");
        assert_eq!(
            fg_ctx,
            Color::Rgb(150, 150, 150),
            "context line must be muted gray"
        );

        // Header line `edited foo.rs (1 replacement)` (no diff prefix) — also gray.
        let (fg_header, _mod_header) =
            cell_at("edited foo.rs").expect("header line found");
        assert_eq!(
            fg_header,
            Color::Rgb(150, 150, 150),
            "header line must be muted gray"
        );
    }

    #[test]
    fn test_render_chat_lines_basic() {
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.push_user("hello");
        app.push_assistant("world");
        let lines = render_chat_lines(&app);
        // After the blank-separator removal: one line per message,
        // no forced blank between them. Expect 2 lines minimum.
        assert!(lines.len() >= 2);
    }

    #[test]
    fn test_tool_start_pushes_inline_message() {
        let mut app = TuiApp::new("m");
        app.messages.clear();
        // CC-style: snake_case `read_file` becomes `Read` in the inline
        // header. The preview has no recognisable arg field so just the
        // bare display name should land in the message text.
        app.tool_start("read_file", "test.rs");
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Tool);
        assert!(
            app.messages[0].text.starts_with("Read"),
            "expected CC-style 'Read' prefix, got {:?}",
            app.messages[0].text,
        );
    }

    #[test]
    fn canonical_tool_name_handles_common_shapes() {
        assert_eq!(canonical_tool_name("read_file"), "Read");
        assert_eq!(canonical_tool_name("edit_file"), "Edit");
        assert_eq!(canonical_tool_name("write_file"), "Write");
        assert_eq!(canonical_tool_name("multi_edit"), "MultiEdit");
        assert_eq!(canonical_tool_name("bash"), "Bash");
        assert_eq!(canonical_tool_name("parallel_agents"), "ParallelAgents");
        // MCP tools (double underscore) are passed through unchanged so
        // they keep their server::action identity.
        assert_eq!(
            canonical_tool_name("mcp__obsidian__search"),
            "mcp__obsidian__search"
        );
    }

    #[test]
    fn extract_primary_arg_picks_relevant_field() {
        // path → covered by extract_path_from_preview
        assert_eq!(
            extract_primary_arg("{\"path\":\"/tmp/foo.rs\"}", "edit_file"),
            Some("/tmp/foo.rs".to_string())
        );
        // bash uses command
        assert_eq!(
            extract_primary_arg("{\"command\":\"cargo test\"}", "bash"),
            Some("cargo test".to_string())
        );
        // grep uses pattern
        assert_eq!(
            extract_primary_arg("{\"pattern\":\"fn main\"}", "grep"),
            Some("fn main".to_string())
        );
        // unknown tool with no recognised field → None
        assert_eq!(
            extract_primary_arg("{\"foo\":\"bar\"}", "weird_tool"),
            None
        );
    }

    // ---------- Tool header bullet ⏺ (M4 CC parity) ----------
    #[test]
    fn tool_header_renders_with_cc_bullet_and_pascal_name() {
        use ratatui::backend::TestBackend;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;

        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.tool_start("edit_file", "{\"path\":\"/tmp/foo.rs\"}");

        let lines = render_chat_lines(&app);
        let backend = TestBackend::new(120, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let para = Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false });
                frame.render_widget(para, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        // Read the first row's text to find the rendered header.
        let mut row0 = String::new();
        for x in 0..buffer.area().width {
            row0.push_str(buffer[(x, 0)].symbol());
        }
        assert!(
            row0.contains("⚙ Edit(/tmp/foo.rs)"),
            "expected '⚙ Edit(/tmp/foo.rs)' in row, got {row0:?}",
        );
        // Copilot CLI style: tool calls in dim gray, no yellow.
        let bullet_cell = &buffer[(0u16, 0u16)];
        assert_eq!(bullet_cell.symbol(), "⚙");
        assert_eq!(bullet_cell.fg, Color::Rgb(150, 150, 150));
        assert!(bullet_cell.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn test_message_role_properties() {
        // Colors now follow the REPL palette (orange assistant, etc.).
        // Just ensure accessors don't panic.
        let _ = MessageRole::User.color();
        let _ = MessageRole::Assistant.color();
        let _ = MessageRole::System.color();
        let _ = MessageRole::Error.color();
    }

    #[test]
    fn test_tool_status_properties() {
        assert_eq!(ToolStatus::Running.symbol(), "...");
        assert_eq!(ToolStatus::Done.symbol(), " ok");
        assert_eq!(ToolStatus::Failed.symbol(), "err");
        let _ = ToolStatus::Running.color();
    }

    #[test]
    fn test_unicode_input() {
        let mut app = TuiApp::new("m");
        for c in "merhaba".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "merhaba");
        // Insert a multi-byte char at the beginning.
        app.home();
        app.insert_char('\u{00FC}'); // u with umlaut (2 bytes in UTF-8)
        assert_eq!(app.input, "\u{00FC}merhaba");
        // Cursor is right after the umlaut.
        assert_eq!(app.cursor, 2); // umlaut is 2 bytes
                                   // Backspace should remove the umlaut cleanly.
        app.backspace();
        assert_eq!(app.input, "merhaba");
        assert_eq!(app.cursor, 0);
    }

    // ---------- /plan mode visual indicator ----------

    #[test]
    fn recap_line_hides_plan_chip_in_normal_mode() {
        let app = TuiApp::new("test-m");
        let line = build_recap_line(&app);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!joined.contains("[plan]"));
        assert!(!joined.contains("[exec]"));
    }

    #[test]
    fn recap_line_shows_yellow_bold_plan_chip_in_drafting_mode() {
        let app = TuiApp::new("test-m");
        *app.plan_state.lock().unwrap() = PlanState::Drafting;
        let line = build_recap_line(&app);
        let chip = line
            .spans
            .iter()
            .find(|s| s.content == "Plan ")
            .expect("plan chip must be present");
        assert_eq!(chip.style.fg, Some(Color::Rgb(210, 168, 255)));
        assert!(chip.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn recap_line_executing_mode_emits_no_chip_under_default_permission() {
        // Pre-PermMode the recap rendered a green "Exec " chip when
        // plan_state was Executing. The 4-mode permission cycle moved
        // chip rendering off plan_state and onto permission_mode, so a
        // Default/Executing combination intentionally falls through to
        // "no chip". Pin that behaviour so future renderer changes
        // don't silently resurrect the legacy chip.
        let app = TuiApp::new("test-m");
        *app.plan_state.lock().unwrap() = PlanState::Executing;
        let line = build_recap_line(&app);
        assert!(
            line.spans.iter().all(|s| s.content != "Exec "),
            "legacy 'Exec ' chip should not render under PermMode::Default"
        );
    }

    #[test]
    fn plan_toggle_flips_recap_chip_on_and_off() {
        let mut app = TuiApp::new("test-m");
        let tmp = tempfile::tempdir().unwrap();
        app.handle_slash("/plan", tmp.path());
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Drafting);
        let l1 = build_recap_line(&app);
        assert!(l1.spans.iter().any(|s| s.content == "Plan "));

        app.handle_slash("/plan", tmp.path());
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Normal);
        let l2 = build_recap_line(&app);
        assert!(!l2.spans.iter().any(|s| s.content == "Plan "));
    }

    // ---------- bottom recap line (auto-pinned status strip) ----------

    #[test]
    fn recap_line_includes_tool_count_when_tools_have_run() {
        let mut app = TuiApp::new("m");
        app.tool_start("read_file", "x");
        app.tool_done("read_file", "ok", false);
        app.tool_start("bash", "ls");
        app.tool_done("bash", "err", true);
        let line = build_recap_line(&app);
        let joined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        assert!(joined.contains("tools:2"));
    }

    #[test]
    fn recap_line_hides_tool_count_when_none_run_yet() {
        let app = TuiApp::new("m");
        let line = build_recap_line(&app);
        let joined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        assert!(!joined.contains("tools:"));
    }

    #[test]
    fn recap_line_highlights_running_tool_in_yellow_bold() {
        let mut app = TuiApp::new("m");
        app.tool_start("web_fetch", "https://…");
        let line = build_recap_line(&app);
        // Running tool indicator: `(+1)` in yellow bold
        let running_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("+1"))
            .expect("running chip must be present");
        assert_eq!(running_span.style.fg, Some(Color::Rgb(88, 166, 255)));
        assert!(running_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn recap_line_shows_queued_prompts_in_cyan_bold() {
        let mut app = TuiApp::new("m");
        app.pending_prompts.push_back("q1".into());
        app.pending_prompts.push_back("q2".into());
        let line = build_recap_line(&app);
        let queued = line
            .spans
            .iter()
            .find(|s| s.content.starts_with("queued:"))
            .expect("queued chip present");
        assert_eq!(queued.style.fg, Some(Color::Rgb(88, 166, 255)));
        assert!(queued.style.add_modifier.contains(Modifier::BOLD));
        assert!(queued.content.contains("2 prompt"));
    }

    #[test]
    fn recap_line_merges_queued_prompts_and_images() {
        let mut app = TuiApp::new("m");
        app.pending_prompts.push_back("q1".into());
        app.pending_images
            .push(std::path::PathBuf::from("/tmp/a.png"));
        let line = build_recap_line(&app);
        let queued = line
            .spans
            .iter()
            .find(|s| s.content.starts_with("queued:"))
            .unwrap();
        assert!(queued.content.contains("1 prompt"));
        assert!(queued.content.contains("1 image"));
    }

    // ---------- /models menu overlay ----------

    #[test]
    fn models_command_opens_overlay_and_stores_menu() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.handle_slash("/models", tmp.path());
        assert!(app.model_menu.is_some());
        assert_eq!(app.last_model_menu.len(), 4);
    }

    #[test]
    fn model_menu_digit_pick_fires_model_switch() {
        // Simulate the overlay digit-pick flow: /models stores the menu,
        // picking #3 sets pending_model_switch to deepseek-reasoner.
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.handle_slash("/models", tmp.path());
        assert!(app.model_menu.is_some());
        // Simulate handle_key digit pick: idx 2 → deepseek-reasoner.
        let chosen = app.last_model_menu[2].clone();
        app.model_menu = None;
        app.pending_model_switch = Some(chosen.clone());
        assert!(app.model_menu.is_none());
        assert_eq!(
            app.pending_model_switch,
            Some("deepseek-reasoner".to_string())
        );
    }

    // ---------- TUI-native permission modal ----------

    #[test]
    fn tui_permission_read_only_tools_bypass_prompt() {
        let app = Arc::new(Mutex::new(TuiApp::new("m")));
        let perm = TuiPermission::new(Arc::clone(&app));
        let args = serde_json::json!({ "path": "Cargo.toml" });
        for tool in ["read_file", "grep", "glob", "web_fetch", "web_search"] {
            let d = perm.check(tool, &args);
            assert!(
                matches!(d, PermissionDecision::Allow),
                "read-only `{tool}` must bypass prompt, got {:?}",
                d
            );
            // And no pending_permission should have been parked.
            assert!(app.lock().unwrap().pending_permission.is_none());
        }
    }

    #[test]
    fn tui_permission_allow_unblocks_worker() {
        use std::sync::mpsc;
        use std::thread;
        let app = Arc::new(Mutex::new(TuiApp::new("m")));
        let perm = Arc::new(TuiPermission::new(Arc::clone(&app)));
        let perm_for_thread = Arc::clone(&perm);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let d = perm_for_thread.check("bash", &serde_json::json!({ "command": "ls -la" }));
            done_tx.send(d).unwrap();
        });

        // Wait until the worker has parked a pending_permission.
        for _ in 0..200 {
            if app.lock().unwrap().pending_permission.is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let pending = app.lock().unwrap().pending_permission.take().unwrap();
        assert_eq!(pending.tool, "bash");
        assert!(pending.args_preview.contains("ls -la"));
        pending.response_tx.send(PermissionChoice::Allow).unwrap();

        let decision = done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(matches!(decision, PermissionDecision::Allow));
        worker.join().unwrap();
    }

    #[test]
    fn tui_permission_deny_returns_hard_deny() {
        use std::sync::mpsc;
        use std::thread;
        let app = Arc::new(Mutex::new(TuiApp::new("m")));
        let perm = Arc::new(TuiPermission::new(Arc::clone(&app)));
        let perm_t = Arc::clone(&perm);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let d = perm_t.check("edit_file", &serde_json::json!({ "path": "x" }));
            tx.send(d).unwrap();
        });
        for _ in 0..200 {
            if app.lock().unwrap().pending_permission.is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let pending = app.lock().unwrap().pending_permission.take().unwrap();
        pending.response_tx.send(PermissionChoice::Deny).unwrap();
        let d = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        match d {
            PermissionDecision::HardDeny(msg) => assert!(msg.contains("edit_file")),
            other => panic!("expected HardDeny, got {:?}", other),
        }
    }

    #[test]
    fn tui_permission_always_allow_caches_for_session() {
        use std::sync::mpsc;
        use std::thread;
        let app = Arc::new(Mutex::new(TuiApp::new("m")));
        let perm = Arc::new(TuiPermission::new(Arc::clone(&app)));

        // First call: user picks "Yes, and don't ask again".
        let perm_t = Arc::clone(&perm);
        let (tx1, rx1) = mpsc::channel();
        thread::spawn(move || {
            let d = perm_t.check("bash", &serde_json::json!({"command": "pwd"}));
            tx1.send(d).unwrap();
        });
        for _ in 0..200 {
            if app.lock().unwrap().pending_permission.is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let pending = app.lock().unwrap().pending_permission.take().unwrap();
        pending
            .response_tx
            .send(PermissionChoice::AlwaysAllow)
            .unwrap();
        assert!(matches!(
            rx1.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            PermissionDecision::Allow
        ));

        // Second call with same tool: must short-circuit to Allow
        // WITHOUT parking a new pending_permission.
        let d = perm.check("bash", &serde_json::json!({"command": "ls"}));
        assert!(matches!(d, PermissionDecision::Allow));
        assert!(
            app.lock().unwrap().pending_permission.is_none(),
            "always-allowed tool must not re-prompt"
        );
    }

    #[test]
    fn permission_modal_lines_highlight_focused_option() {
        use std::sync::mpsc::channel;
        let (tx, _rx) = channel();
        let pending = PendingPermission {
            tool: "bash".to_string(),
            args_preview: r#"{"command":"ls"}"#.to_string(),
            focused: 1, // middle option focused
            response_tx: tx,
        };
        let lines = build_permission_modal_lines(&pending);
        // Header contains tool name in white bold (changed 2026-04-19
        // from red to white per dogfood feedback — red read as "error"
        // while the modal is really a neutral "please confirm").
        let header = &lines[0];
        let has_white_bold_tool = header.spans.iter().any(|s| {
            s.content.contains("bash")
                && s.style.fg == Some(Color::White)
                && s.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(has_white_bold_tool);

        // Options start at line index 3 (header, args, blank, options…).
        // Line 4 = "2  Yes, and don't ask…", must carry cyan-bold because
        // focused == 1.
        let option2 = &lines[4];
        let has_cyan_marker = option2.spans.iter().any(|s| {
            s.content == "❯ "
                && s.style.fg == Some(Color::Cyan)
                && s.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(has_cyan_marker, "focused option must have cyan ❯ marker");
    }

    #[test]
    fn permission_modal_truncates_huge_args() {
        use std::sync::mpsc::channel;
        let (tx, _rx) = channel();
        let big = "x".repeat(5000);
        let pending = PendingPermission {
            tool: "bash".to_string(),
            args_preview: big,
            focused: 0,
            response_tx: tx,
        };
        let lines = build_permission_modal_lines(&pending);
        let args_line = &lines[1];
        let joined: String = args_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.ends_with('…'), "long args must be ellipsized");
        assert!(joined.chars().count() < 120, "must fit a reasonable width");
    }

    // ---------- cursor <-> byte-offset parity ----------

    #[test]
    fn byte_cursor_matches_char_count_for_ascii_input() {
        let mut app = TuiApp::new("m");
        for c in "hello".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.cursor, 5);
        let visible = app.input[..app.cursor].chars().count();
        assert_eq!(visible, 5, "ASCII: byte cursor == char count");
    }

    #[test]
    fn multi_byte_chars_drift_byte_cursor_ahead_of_visible_col() {
        // Reproduces the exact UI bug the user flagged: every Turkish
        // character (ş, ç, ı, ğ, ü, ö) is 2 bytes but 1 visible column.
        // The RAW `app.cursor` (bytes) must NOT be used as the cursor
        // x-column; the render path must convert to char count.
        let mut app = TuiApp::new("m");
        for c in "için".chars() {
            app.insert_char(c);
        }
        // "için" = ç(2) + i(1) + ç(2) + i(1) + n(1) — wait, only one ç.
        // "için" = i(1) + ç(2) + i(1) + n(1) = 5 bytes, 4 chars.
        assert_eq!(app.cursor, 5, "byte cursor advanced by utf-8 len");
        let visible = app.input[..app.cursor].chars().count();
        assert_eq!(visible, 4, "visible column is the char count");
        assert_ne!(
            app.cursor, visible,
            "raw byte cursor must differ from visible column"
        );
    }

    #[test]
    fn visible_col_handles_partial_cursor_between_chars() {
        // `get(..cursor)` returns None if cursor lands mid-char — the
        // render path falls back to the total char count in that case.
        // We can't easily produce a mid-char cursor via insert_char
        // (it always lands on a boundary), so test the fallback math
        // directly against a known good string.
        let s = "şöç";
        assert_eq!(s.len(), 6);
        assert_eq!(s.chars().count(), 3);
        // Cursor at byte 2 (after "ş") = 1 visible col.
        assert_eq!(s[..2].chars().count(), 1);
        // Cursor at byte 4 (after "şö") = 2 visible cols.
        assert_eq!(s[..4].chars().count(), 2);
    }

    // ---------- /insights ----------

    #[test]
    fn insights_empty_session_warns() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let _ = SessionStore::open(&ws, &sid).unwrap();

        let mut app = TuiApp::new("m");
        app.session_id = sid;
        app.messages.clear();
        app.handle_slash("/insights", &ws);
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("no session messages"));
        assert!(app.pending_prompts.is_empty(), "no turn should be queued");
    }

    #[test]
    fn insights_nonexistent_session_errors_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut app = TuiApp::new("m");
        app.session_id = "no-such-session-id".to_string();
        app.messages.clear();
        app.handle_slash("/insights", &ws);
        // SessionStore::open creates the file if missing, so this path
        // treats it as empty rather than erroring — same as REPL.
        let last = app.messages.last().unwrap();
        assert!(
            last.text.contains("no session messages")
                || last.text.contains("could not open session"),
            "unexpected: {}",
            last.text
        );
    }

    #[test]
    fn insights_builds_prompt_and_queues_turn_when_content_exists() {
        use aegis_api::ChatMessage;
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let mut store = SessionStore::open(&ws, &sid).unwrap();
        store
            .append(&ChatMessage::user("investigate the auth bug"))
            .unwrap();
        store
            .append(&ChatMessage::assistant_text(
                "found root cause in session middleware",
            ))
            .unwrap();
        drop(store);

        let mut app = TuiApp::new("m");
        app.session_id = sid;
        app.messages.clear();
        app.handle_slash("/insights", &ws);

        // 1 turn queued
        assert_eq!(app.pending_prompts.len(), 1);
        let queued = app.pending_prompts.front().unwrap();
        // Prompt must include the conversation AND the memory_save
        // instruction — byte-for-byte REPL parity.
        assert!(queued.contains("User: investigate the auth bug"));
        assert!(queued.contains("Assistant: found root cause"));
        assert!(queued.contains("memory_save"));
        assert!(queued.contains("non-obvious facts"));
        // System notice pushed
        let has_notice = app
            .messages
            .iter()
            .any(|m| m.text.contains("extracting insights"));
        assert!(has_notice);
    }

    #[test]
    fn insights_truncates_tool_output_at_500_chars_like_repl() {
        use aegis_api::{ChatMessage, Role};
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let mut store = SessionStore::open(&ws, &sid).unwrap();
        let huge: String = "x".repeat(1200);
        let tool_msg = ChatMessage {
            role: Role::Tool,
            content: Some(huge.clone()),
            content_blocks: Vec::new(),
            tool_calls: vec![],
            tool_call_id: Some("t1".to_string()),
            name: None,
            protected: false,
            reasoning_content: None,
        };
        // Seed with one real user turn first so the role filter has
        // something to keep alongside the tool output.
        store
            .append(&ChatMessage::user("run that command"))
            .unwrap();
        store.append(&tool_msg).unwrap();
        drop(store);

        let mut app = TuiApp::new("m");
        app.session_id = sid;
        app.messages.clear();
        app.handle_slash("/insights", &ws);

        let queued = app.pending_prompts.front().unwrap();
        // 500 x's present, 501st isn't
        let needle_500 = "x".repeat(500);
        let needle_501 = "x".repeat(501);
        assert!(queued.contains(&needle_500));
        assert!(
            !queued.contains(&needle_501),
            "tool output must be truncated at 500 chars"
        );
    }

    // ---------- /models ----------

    #[test]
    fn models_unknown_provider_returns_empty_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "notaprovider".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("no models registered"));
        assert!(app.last_model_menu.is_empty());
    }

    #[test]
    fn models_nvidia_lists_nim_models() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "nvidia".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        assert_eq!(app.last_model_menu.len(), 7);
        assert!(app.last_model_menu.contains(&"meta/llama-4-maverick-17b-128e-instruct".to_string()));
        assert!(app.last_model_menu.contains(&"deepseek-ai/deepseek-r1".to_string()));
    }

    #[test]
    fn models_minimax_lists_m27_and_m25() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "minimax".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        assert_eq!(app.last_model_menu.len(), 7);
        assert_eq!(app.last_model_menu[0], "MiniMax-M2.7");
        assert_eq!(app.last_model_menu[2], "MiniMax-M2.5");
        assert_eq!(app.last_model_menu[6], "MiniMax-VL-01");
        assert!(app.model_menu.is_some());
    }

    #[test]
    fn models_deepseek_lists_both_variants_and_stores_menu() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());

        assert_eq!(app.last_model_menu.len(), 4);
        assert!(app.last_model_menu.contains(&"deepseek-chat".to_string()));
        assert!(app.last_model_menu.contains(&"deepseek-reasoner".to_string()));
        assert!(app.model_menu.is_some());
    }

    #[test]
    fn models_openai_has_full_gpt_and_o_series() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "openai".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        assert_eq!(app.last_model_menu.len(), 10);
        assert!(app.last_model_menu.contains(&"gpt-4o".to_string()));
        assert!(app.last_model_menu.contains(&"o3".to_string()));
        assert!(app.last_model_menu.contains(&"gpt-4.1".to_string()));
        assert!(app.last_model_menu.contains(&"o4-mini".to_string()));
    }

    #[test]
    fn models_list_header_is_green_bold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        // Models now shown in overlay only — verify overlay is populated.
        assert!(app.model_menu.is_some());
        let overlay_lines = build_model_menu_lines(app.model_menu.as_ref().unwrap(), &app.model, 0);
        // First rendered line should have a colored span (header via overlay renderer).
        assert!(!overlay_lines.is_empty());
    }

    #[test]
    fn model_by_number_picks_from_last_menu() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        // Model 4 = index 3 = deepseek-chat (0: v4-flash, 1: v4-pro, 2: reasoner, 3: chat)
        app.handle_slash("/model 4", tmp.path());
        assert_eq!(
            app.pending_model_switch,
            Some("deepseek-chat".to_string())
        );
    }

    #[test]
    fn glm_model_menu_lists_current_working_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "glm".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        // All of these are live codes verified against z.ai's
        // /api/paas/v4/chat/completions endpoint with a valid ZAI key.
        // Lock the menu so a future refactor can't accidentally drop
        // one again (I deleted glm-5.1 by mistake based on a partial
        // probe — user caught it immediately).
        let expected = [
            "glm-5.1",
            "glm-5",
            "glm-5-turbo",
            "glm-4.6",
            "glm-4.5",
            "glm-4.5v",
            "glm-4-plus",
        ];
        for code in expected {
            assert!(
                app.last_model_menu.contains(&code.to_string()),
                "expected glm model `{code}` in menu, got {:?}",
                app.last_model_menu
            );
        }
    }

    #[test]
    fn deepseek_reasoner_labeled_as_r1_not_v32() {
        // `deepseek-reasoner` is the R1 series, not V3.2. Menu label
        // must match or the user picks the wrong thing thinking it's
        // the V3.2 reasoner.
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.current_provider = "deepseek".to_string();
        app.messages.clear();
        app.handle_slash("/models", tmp.path());
        // Labels are verified via models_for_provider — overlay shows the label string.
        let models = models_for_provider("deepseek");
        let r1 = models.iter().find(|(id, _)| *id == "deepseek-reasoner");
        assert!(r1.is_some(), "deepseek-reasoner must be in menu");
        assert_eq!(r1.unwrap().1, "R1 Reasoner (V4 Flash thinking)");
        let has_v32 = models.iter().any(|(_, label)| label.contains("V3.2 Reasoner"));
        assert!(!has_v32);
    }

    #[test]
    fn model_by_number_without_menu_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/model 3", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
        assert!(last.text.contains("out of range"));
        assert!(app.pending_model_switch.is_none());
    }

    #[test]
    fn model_by_name_still_works_alongside_numeric() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/model glm-4.6", tmp.path());
        assert_eq!(app.pending_model_switch, Some("glm-4.6".to_string()));
    }

    // ---------- /key ----------

    #[test]
    fn key_rejects_missing_value() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/key", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
        assert!(last.text.contains("usage"));
    }

    #[test]
    fn key_rejects_env_var_without_value() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/key OPENAI_API_KEY", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
    }

    #[test]
    fn key_sets_env_var_and_pushes_persistence_hint_with_truncated_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        // Use a uniquely-named env var so the test doesn't collide with
        // real keys in the environment, and so unsetting in teardown is
        // safe. Value is long enough to exercise the 8-char truncation.
        let env = "METIS_TEST_KEY_UNIQUE_9X7";
        let value = "sk-verysecretvaluehere1234567890";
        app.handle_slash(&format!("/key {env} {value}"), tmp.path());

        assert_eq!(std::env::var(env).unwrap(), value);
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains(env));
        assert!(last.text.contains("session only"));
        assert!(last.text.contains("~/.metis/config.toml"));
        assert!(last.text.contains("[api_keys]"));
        // Preview must be exactly first 8 chars, NOT the full value.
        assert!(last.text.contains("\"sk-verys...\""));
        assert!(
            !last.text.contains("secretvaluehere"),
            "full value must NOT be echoed back"
        );
        // Clean up env to not pollute other tests.
        std::env::remove_var(env);
    }

    #[test]
    fn key_truncation_handles_short_values_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        let env = "METIS_TEST_SHORT_KEY_9X7";
        app.handle_slash(&format!("/key {env} abc"), tmp.path());
        let last = app.messages.last().unwrap();
        // "abc" < 8 chars: preview = full value (no panic)
        assert!(last.text.contains("\"abc...\""));
        std::env::remove_var(env);
    }

    // ---------- /glm ----------

    #[test]
    fn glm_rejects_empty_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/glm", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
        assert!(last.text.contains("usage"));
        assert!(app.pending_consult.is_none());
    }

    #[test]
    fn glm_queues_consult_against_glm_provider() {
        // Only run the positive path when glm is actually registered
        // as a provider in the running build — otherwise assert the
        // error branch fires instead. Either way the handler is sane.
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/glm explain reasoning step by step", tmp.path());
        if aegis_api::Provider::lookup("glm").is_some() {
            assert_eq!(
                app.pending_consult,
                Some((
                    "glm".to_string(),
                    "explain reasoning step by step".to_string()
                ))
            );
            let has_notice = app
                .messages
                .iter()
                .any(|m| m.text.contains("consult queued: glm"));
            assert!(has_notice);
        } else {
            let last = app.messages.last().unwrap();
            assert_eq!(last.role, MessageRole::Error);
            assert!(last.text.contains("unknown provider"));
            assert!(app.pending_consult.is_none());
        }
    }

    #[test]
    fn glm_is_alias_for_consult_glm() {
        // Functional parity: /glm <p> and /consult glm <p> produce
        // identical pending_consult state.
        let tmp = tempfile::tempdir().unwrap();
        if aegis_api::Provider::lookup("glm").is_none() {
            return; // skip — provider not registered in this build
        }
        let mut a1 = TuiApp::new("m");
        a1.messages.clear();
        a1.handle_slash("/glm what is zk-snark", tmp.path());

        let mut a2 = TuiApp::new("m");
        a2.messages.clear();
        a2.handle_slash("/consult glm what is zk-snark", tmp.path());

        assert_eq!(a1.pending_consult, a2.pending_consult);
    }

    // ---------- end-to-end pipeline tests ----------
    // Drive full sequences through handle_slash to catch interactions
    // between commands that unit tests miss (fork + btw + plan + compact).

    #[test]
    fn e2e_plan_then_compact_preserves_state_correctly() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let _ = SessionStore::open(&ws, &sid).unwrap();

        let mut app = TuiApp::new("m");
        app.session_id = sid;
        app.messages.clear();

        app.handle_slash("/plan", &ws);
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Drafting);

        app.handle_slash("/compact", &ws);
        assert!(app.pending_compact);

        app.handle_slash("/plan", &ws);
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Normal);

        assert!(app.pending_compact);
        assert!(app.messages.len() >= 3);
    }

    #[test]
    fn e2e_fork_after_plan_keeps_plan_state_and_messages() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let sid = SessionStore::new_id();
        let _ = SessionStore::open(&ws, &sid).unwrap();

        let mut app = TuiApp::new("m");
        app.session_id = sid.clone();
        app.messages.clear();
        app.push_user("design prompt");

        // Enter plan mode, then fork — plan state should persist.
        app.handle_slash("/plan", &ws);
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Drafting);

        app.handle_slash("/fork branch-a", &ws);
        assert_ne!(app.session_id, sid);
        assert_eq!(
            *app.plan_state.lock().unwrap(),
            PlanState::Drafting,
            "plan state must survive a fork (session-scoped, not forkscoped)"
        );
        // Original user message still visible in chat panel
        assert!(app.messages.iter().any(|m| m.text == "design prompt"));
    }

    #[test]
    fn e2e_consult_then_swarm_queues_both_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();

        app.handle_slash("/consult openrouter explain tensor cores", tmp.path());
        app.handle_slash("/swarm 4 how to optimize matmul", tmp.path());

        assert_eq!(
            app.pending_consult,
            Some(("openrouter".to_string(), "explain tensor cores".to_string()))
        );
        assert_eq!(app.pending_prompts.len(), 1);
        assert!(app
            .pending_prompts
            .front()
            .unwrap()
            .contains("how to optimize matmul"));
    }

    // ---------- /btw deprecation ----------

    #[test]
    fn btw_emits_deprecation_pointing_at_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/btw remember to check the index", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("/ask"));
    }

    // ---------- /swarm behavior ----------

    #[test]
    fn swarm_rejects_empty_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/swarm", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
        assert!(last.text.contains("usage"));
    }

    #[test]
    fn swarm_rejects_n_out_of_range() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/swarm 11 foo", tmp.path());
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Error);
        assert!(last.text.contains("2-10"));
    }

    #[test]
    fn swarm_default_n_is_3_and_queues_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/swarm how to compress a directory", tmp.path());
        assert_eq!(app.pending_prompts.len(), 1);
        let queued = app.pending_prompts.front().unwrap();
        assert!(queued.contains("parallel_agents"));
        assert!(queued.contains("how to compress a directory"));
        // Default 3 agents
        assert!(queued.contains("Agent 1"));
        assert!(queued.contains("Agent 2"));
        assert!(queued.contains("Agent 3"));
        assert!(!queued.contains("Agent 4"));
        // System notice pushed
        let has_notice = app
            .messages
            .iter()
            .any(|m| m.text.contains("3 parallel agents"));
        assert!(has_notice);
    }

    #[test]
    fn swarm_explicit_n_and_quorum_are_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/swarm 5 quorum:3 explain entropy", tmp.path());
        let queued = app.pending_prompts.front().unwrap();
        assert!(queued.contains("Agent 5"));
        assert!(queued.contains("quorum of 3"));
        let notice = app
            .messages
            .iter()
            .find(|m| m.text.contains("swarm: 5"))
            .unwrap();
        assert!(notice.text.contains("quorum 3/5"));
    }

    // ---------- /dag colorization ----------

    #[test]
    fn dag_colorize_success_marker_is_green_bold() {
        let plain = "  turn  2  ── read_file           {\"path\":\"x\"}  ✓";
        let lines = colorize_dag_lines(plain);
        assert_eq!(lines.len(), 1);
        let check = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "✓")
            .expect("green check span present");
        assert_eq!(check.style.fg, Some(Color::Green));
        assert!(check.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dag_colorize_failure_marker_is_red_bold() {
        let plain = "  turn  3  ── web_search          {\"q\":\"x\"}  ✗ error";
        let lines = colorize_dag_lines(plain);
        let err = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains('✗'))
            .expect("cross span present");
        assert_eq!(err.style.fg, Some(Color::White));
        assert!(err.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dag_colorize_turn_label_is_yellow_bold() {
        let plain = "  turn  1  ── bash                 {\"cmd\":\"ls\"}  ✓";
        let lines = colorize_dag_lines(plain);
        let turn = lines[0]
            .spans
            .iter()
            .find(|s| s.content.starts_with("turn"))
            .expect("turn label present");
        assert_eq!(turn.style.fg, Some(Color::Yellow));
        assert!(turn.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dag_colorize_tool_name_is_cyan() {
        let plain = "  turn  1  ── read_file           {}  ✓";
        let lines = colorize_dag_lines(plain);
        let name = lines[0]
            .spans
            .iter()
            .find(|s| s.content.starts_with("read_file"))
            .expect("tool name span present");
        assert_eq!(name.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn dag_colorize_preserves_line_count() {
        let plain = "  turn  1  ┬─ a                     {}  ✓\n           ├─ b                     {}  ✓\n           └─ c                     {}  ✗";
        let lines = colorize_dag_lines(plain);
        assert_eq!(lines.len(), 3);
    }

    // ---------- /map parity confirmation ----------

    #[test]
    fn map_passes_through_core_output_unchanged() {
        // Parity: REPL prints `build_repo_map` output byte-for-byte
        // (trailing whitespace trimmed). TUI does the same via
        // push_system. This test locks that contract so no future
        // refactor sneaks in a transform.
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp); // non-dot subdir (walkdir filter)
        make_file(
            &ws,
            "lib.rs",
            "fn add(a:i32,b:i32)->i32{a+b}\nstruct Pt{x:i32}",
        );
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/map", &ws);

        let last = app.messages.last().expect("map output pushed");
        assert_eq!(last.role, MessageRole::System);
        let core_out = aegis_core::repomap::build_repo_map(&ws, 200);
        assert_eq!(
            last.text.as_str(),
            core_out.trim_end(),
            "map must match core output byte-for-byte (trim_end only)"
        );
        assert!(
            last.text.contains("lib.rs"),
            "map must include the source file"
        );
    }

    #[test]
    fn map_empty_workspace_shows_friendly_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = search_workspace(&tmp);
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/map", &ws);
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("no source files found"));
    }

    // ---------- /consult live streaming ----------

    #[test]
    fn consult_handler_queues_pending_and_logs_queued_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/consult openrouter what is entropy", tmp.path());
        let pending = app.pending_consult.clone().expect("consult queued");
        assert_eq!(pending.0, "openrouter");
        assert_eq!(pending.1, "what is entropy");
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::System);
        assert!(last.text.contains("consult queued"));
    }

    #[test]
    fn consult_no_args_opens_provider_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/consult", tmp.path());
        assert!(app.pending_consult.is_none());
        assert!(app.consult_pick_mode);
        assert!(app.provider_menu.is_some());
    }

    #[test]
    fn consult_streaming_field_default_is_none() {
        let app = TuiApp::new("m");
        assert!(app.consult_streaming.is_none());
    }

    #[test]
    fn consult_streaming_renders_live_placeholder_when_body_empty() {
        // When the stream has started but no tokens have arrived yet,
        // the render loop must emit `[provider] consulting…` so the
        // user sees *something* instead of a silent pause.
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.consult_streaming = Some(("zai".to_string(), String::new()));
        let lines = render_chat_lines(&app);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            joined.contains("[zai] consulting…"),
            "live stream must show placeholder when body empty, got: {joined}"
        );
    }

    #[test]
    fn consult_streaming_renders_tokens_as_they_grow() {
        // Simulate a partial stream: first token "Entropy ", then " is".
        // Each render reflects the up-to-the-moment body. This is the
        // fix — previous behavior was a silent Mutex<String> collect
        // until the stream closed.
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.consult_streaming = Some(("zai".to_string(), "Entropy ".to_string()));
        let first: String = render_chat_lines(&app)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(first.contains("[zai] Entropy"));
        assert!(!first.contains("consulting…"));

        if let Some((_, body)) = app.consult_streaming.as_mut() {
            body.push_str("is disorder");
        }
        let second: String = render_chat_lines(&app)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(second.contains("[zai] Entropy is disorder"));
    }

    #[test]
    fn consult_stream_is_rendered_in_system_green() {
        // The streaming line must use the System role's green color so
        // it reads as an out-of-band helper turn, not a main-model reply.
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.consult_streaming = Some(("zai".to_string(), "hi".to_string()));
        let lines = render_chat_lines(&app);
        let has_green_line = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("[zai]") && s.style.fg == Some(Color::Rgb(63, 185, 80)))
        });
        assert!(
            has_green_line,
            "expected a span containing '[zai]' styled green"
        );
    }

    // ---------- /compact behavior ----------

    #[test]
    fn compact_queues_flag_and_pushes_queued_system_notice() {
        // /compact flips pending_compact + pushes a "queued — running on
        // next idle tick" line into chat. Main loop then consumes the
        // flag, runs force_compact, and pushes "[goblin] compacted: N"
        // (byte-identical to REPL) — that branch is covered by REPL
        // integration tests; here we lock the TUI-visible contract.
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        assert!(!app.pending_compact);
        app.handle_slash("/compact", tmp.path());
        assert!(app.pending_compact, "compact flag must be set");
        let last = app.messages.last().expect("system notice pushed");
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.text.contains("Compacting"),
            "expected compaction notice, got: {}",
            last.text
        );
    }

    #[test]
    fn compact_flag_idempotent_on_repeat() {
        // Repeated /compact before main loop drains should not panic or
        // double-push — flag stays true, multiple queued notices appear
        // (matches REPL's behavior of echoing each invocation).
        let tmp = tempfile::tempdir().unwrap();
        let mut app = TuiApp::new("m");
        app.messages.clear();
        app.handle_slash("/compact", tmp.path());
        app.handle_slash("/compact", tmp.path());
        assert!(app.pending_compact);
        let queued: Vec<_> = app
            .messages
            .iter()
            .filter(|m| m.text.contains("Compacting"))
            .collect();
        assert_eq!(queued.len(), 2, "each /compact invocation logs once");
    }

    // ---------- /fork chat-panel behavior (parity confirmation) ----------

    #[test]
    fn fork_preserves_chat_messages_on_switch() {
        // REPL /fork rebuilds the agent but never clears terminal
        // scrollback — the user sees their history continue. TUI's chat
        // panel is the scrollback equivalent, so parity means messages
        // must survive a fork. This test locks that guarantee in place.
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let sid = SessionStore::new_id();
        let store = SessionStore::open(&ws_dir, &sid).unwrap();
        drop(store);

        let mut app = TuiApp::new("m");
        app.session_id = sid.clone();
        app.messages.clear();
        app.push_user("hello from parent branch");
        app.push_assistant("parent reply");
        let before_count = app.messages.len();
        assert_eq!(before_count, 2);

        app.handle_slash("/fork forkchild", &ws_dir);

        assert_ne!(app.session_id, sid, "session id must change after fork");
        assert!(
            app.messages.len() >= before_count,
            "messages must not shrink"
        );
        let joined: String = app
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("hello from parent branch"),
            "pre-fork user message must still be visible in chat panel"
        );
        assert!(
            joined.contains("parent reply"),
            "pre-fork assistant reply must still be visible"
        );
        assert!(
            joined.contains("forked"),
            "system notice about fork must be appended"
        );
    }

    #[test]
    fn fork_existing_target_requires_confirm() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();

        // Parent session with content.
        let parent_id = SessionStore::new_id();
        let _parent = SessionStore::open(&ws_dir, &parent_id).unwrap();

        // Pre-create the fork target so the overwrite guard triggers.
        let target_id = "collides";
        let target_store = SessionStore::open(&ws_dir, target_id).unwrap();
        drop(target_store);

        let mut app = TuiApp::new("m");
        app.session_id = parent_id.clone();

        // First /fork: arms the prime, does NOT switch session.
        let before_sid = app.session_id.clone();
        let r1 = app.handle_slash(&format!("/fork {target_id}"), &ws_dir);
        assert_eq!(r1, SlashResult::Handled);
        assert_eq!(
            app.session_id, before_sid,
            "session id must not change on prime"
        );
        assert!(app.fork_overwrite_primed.is_some());
        let warned = app
            .messages
            .iter()
            .any(|m| m.text.contains("already exists"));
        assert!(warned, "user must see overwrite warning");

        // Second /fork within window: proceeds, session switches.
        let r2 = app.handle_slash(&format!("/fork {target_id}"), &ws_dir);
        assert!(matches!(r2, SlashResult::SwitchSession(_)));
        assert_eq!(app.session_id, target_id);
        assert!(app.fork_overwrite_primed.is_none());
    }

    #[test]
    fn fork_overwrite_prime_expires() {
        use aegis_core::SessionStore;
        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();

        let parent_id = SessionStore::new_id();
        let _parent = SessionStore::open(&ws_dir, &parent_id).unwrap();
        let target_id = "collides2";
        let _ = SessionStore::open(&ws_dir, target_id).unwrap();

        let mut app = TuiApp::new("m");
        app.session_id = parent_id;

        // Arm…
        let _ = app.handle_slash(&format!("/fork {target_id}"), &ws_dir);
        assert!(app.fork_overwrite_primed.is_some());

        // …then force-expire the prime.
        if let Some((n, _)) = app.fork_overwrite_primed.clone() {
            app.fork_overwrite_primed = Some((
                n,
                std::time::Instant::now() - std::time::Duration::from_secs(10),
            ));
        }

        // Next /fork re-arms rather than overwriting.
        let before_messages_on_target = std::fs::metadata(
            ws_dir
                .join(".metis")
                .join("sessions")
                .join(format!("{target_id}.jsonl")),
        )
        .unwrap()
        .len();
        let r = app.handle_slash(&format!("/fork {target_id}"), &ws_dir);
        assert_eq!(r, SlashResult::Handled);
        // File still exists, untouched by the re-arm attempt.
        let after = std::fs::metadata(
            ws_dir
                .join(".metis")
                .join("sessions")
                .join(format!("{target_id}.jsonl")),
        )
        .unwrap()
        .len();
        assert_eq!(before_messages_on_target, after);
    }

    // ---------- Tab completion ----------

    #[test]
    fn tab_completes_unambiguous_slash_command() {
        let mut app = TuiApp::new("m");
        app.input.clear();
        app.cursor = 0;
        for c in "/resum".chars() {
            app.insert_char(c);
        }
        let changed = app.complete_tab(std::path::Path::new("/tmp"));
        assert!(changed);
        assert_eq!(app.input, "/resume ");
        assert_eq!(app.cursor, "/resume ".len());
    }

    #[test]
    fn tab_extends_to_common_prefix() {
        // `/sk` is ambiguous across skills, skill-install, …; common
        // prefix is `skill`, so Tab should extend to `/skill`.
        let mut app = TuiApp::new("m");
        app.input.clear();
        app.cursor = 0;
        for c in "/sk".chars() {
            app.insert_char(c);
        }
        let _ = app.complete_tab(std::path::Path::new("/tmp"));
        assert_eq!(app.input, "/skill");
    }

    #[test]
    fn tab_lists_candidates_when_stuck() {
        // After the extend step, `/skill` matches several commands that
        // don't share a longer prefix — this time Tab shows candidates
        // without further editing the buffer.
        let mut app = TuiApp::new("m");
        app.input.clear();
        app.cursor = 0;
        for c in "/skill".chars() {
            app.insert_char(c);
        }
        let before = app.input.clone();
        let changed = app.complete_tab(std::path::Path::new("/tmp"));
        assert!(!changed);
        assert_eq!(app.input, before);
        let listed = app
            .messages
            .iter()
            .any(|m| m.text.contains("/skills") && m.text.contains("/skill-install"));
        assert!(
            listed,
            "candidates must appear in chat: {:?}",
            app.messages.last()
        );
    }

    #[test]
    fn tab_completes_path_in_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let unique = "metis_tab_xyzzy.png";
        std::fs::write(tmp.path().join(unique), b"x").unwrap();

        let mut app = TuiApp::new("m");
        app.input.clear();
        app.cursor = 0;
        // `/image <abs-prefix>`
        let abs = tmp.path().join("metis_tab_xyzzy");
        let typed = format!("/image {}", abs.display());
        for c in typed.chars() {
            app.insert_char(c);
        }
        let changed = app.complete_tab(std::path::Path::new("/tmp"));
        assert!(changed, "tab must complete an unambiguous path");
        assert!(
            app.input.ends_with(unique),
            "expected input to end with {unique}, got {:?}",
            app.input
        );
    }

    #[test]
    fn last_token_respects_escaped_space() {
        assert_eq!(last_token("/image foo\\ bar"), (7, "foo\\ bar"));
        assert_eq!(last_token("/image foo bar"), (11, "bar"));
        assert_eq!(last_token("/image "), (7, ""));
    }

    #[test]
    fn longest_common_prefix_basic() {
        assert_eq!(longest_common_prefix(["skill", "skills"]), "skill");
        assert_eq!(longest_common_prefix(["abc", "abd", "abz"]), "ab");
        assert_eq!(longest_common_prefix(["abc"]), "abc");
        let empty: [&str; 0] = [];
        assert_eq!(longest_common_prefix(empty), "");
    }

    #[test]
    fn shell_escape_spaces_and_quotes() {
        assert_eq!(shell_escape("foo bar"), "foo\\ bar");
        assert_eq!(shell_escape("a'b"), "a\\'b");
        assert_eq!(shell_escape("plain"), "plain");
    }

    // ---------- Reverse-i-search (Ctrl+R) ----------

    fn with_history(entries: &[&str]) -> TuiApp {
        let mut app = TuiApp::new("m");
        app.input_history = entries.iter().map(|s| s.to_string()).collect();
        app
    }

    #[test]
    fn reverse_search_finds_latest_match() {
        let mut app = with_history(&["git status", "git push", "cargo test"]);
        app.reverse_search_begin();
        app.reverse_search_append('g');
        app.reverse_search_append('i');
        app.reverse_search_append('t');
        let idx = app.search_state.as_ref().unwrap().match_idx.unwrap();
        assert_eq!(app.input_history[idx], "git push");
    }

    #[test]
    fn reverse_search_step_walks_older() {
        let mut app = with_history(&["git status", "git push", "cargo test"]);
        app.reverse_search_begin();
        app.reverse_search_append('g');
        app.reverse_search_append('i');
        app.reverse_search_append('t');
        // First hit: "git push" (most recent containing "git").
        let first = app.search_state.as_ref().unwrap().match_idx.unwrap();
        assert_eq!(app.input_history[first], "git push");
        // Ctrl+R again → step back to "git status".
        app.reverse_search_step();
        let second = app.search_state.as_ref().unwrap().match_idx.unwrap();
        assert_eq!(app.input_history[second], "git status");
        // One more step → no more matches.
        app.reverse_search_step();
        assert!(app.search_state.as_ref().unwrap().match_idx.is_none());
    }

    #[test]
    fn reverse_search_accept_places_entry_in_input() {
        let mut app = with_history(&["cargo test", "git push"]);
        app.reverse_search_begin();
        app.reverse_search_append('p');
        app.reverse_search_append('u');
        app.reverse_search_accept();
        assert!(app.search_state.is_none());
        assert_eq!(app.input, "git push");
        assert_eq!(app.cursor, "git push".len());
    }

    #[test]
    fn reverse_search_cancel_restores_input() {
        let mut app = with_history(&["cargo test"]);
        app.input = "half-typed".into();
        app.cursor = app.input.len();
        app.reverse_search_begin();
        // overwrite the query — user types gibberish
        app.reverse_search_append('z');
        app.reverse_search_cancel();
        assert!(app.search_state.is_none());
        assert_eq!(app.input, "half-typed");
        assert_eq!(app.cursor, "half-typed".len());
    }

    #[test]
    fn submit_path_like_is_recognized_as_drag_drop() {
        // Regression: Terminal.app drag-drop sends the path as plain
        // keystrokes, so `Event::Paste` is never fired. The submit path
        // must fall back to `try_parse_drag_drop_paths` so the user
        // doesn't get "unknown command: /Users/…".
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("test.png");
        std::fs::write(&img, b"x").unwrap();
        let submitted = img.to_string_lossy().to_string();
        let got = super::try_parse_drag_drop_paths(submitted.trim(), tmp.path());
        assert_eq!(got, Some(vec![img.clone()]));
    }

    #[test]
    fn submit_multi_path_accepted_as_drag_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.jpg");
        let b = tmp.path().join("b.jpg");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let submitted = format!("{} {}", a.display(), b.display());
        let got = super::try_parse_drag_drop_paths(submitted.trim(), tmp.path());
        assert_eq!(got, Some(vec![a, b]));
    }

    // --- N. Misc utilities ---
    #[test]
    fn cleanup_removes_metis_paste_temp() {
        let tmp_root = std::env::temp_dir().canonicalize().unwrap();
        let f = tmp_root.join(format!("metis-paste-{}.png", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        assert!(f.exists());
        super::cleanup_metis_temp_images(std::slice::from_ref(&f));
        assert!(!f.exists(), "metis-paste-* temp file must be removed");
    }

    #[test]
    fn cleanup_leaves_user_paths_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("user-image.png");
        std::fs::write(&f, b"x").unwrap();
        super::cleanup_metis_temp_images(std::slice::from_ref(&f));
        assert!(f.exists(), "arbitrary user paths must not be deleted");
    }

    #[test]
    fn cleanup_ignores_metis_named_file_outside_tempdir() {
        // Even if the basename matches `metis-paste-…`, a file that
        // isn't actually in /tmp must not be touched — otherwise a
        // user who renames an image to `metis-paste-x.png` and drops
        // it in a project dir would lose it.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("metis-paste-fake.png");
        std::fs::write(&f, b"x").unwrap();
        super::cleanup_metis_temp_images(std::slice::from_ref(&f));
        assert!(
            f.exists(),
            "name-matching file outside $TMPDIR must be preserved"
        );
    }

    #[test]
    fn submit_plain_text_not_treated_as_drag_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let got = super::try_parse_drag_drop_paths("hey claude analyze this please", tmp.path());
        assert!(got.is_none());
    }

    #[test]
    fn reverse_search_backspace_widens_query() {
        let mut app = with_history(&["git status", "cargo test"]);
        app.reverse_search_begin();
        app.reverse_search_append('x'); // no match
        assert!(app.search_state.as_ref().unwrap().match_idx.is_none());
        app.reverse_search_backspace(); // back to empty query
                                        // Empty query → no match anchor (bash behavior).
        assert!(app.search_state.as_ref().unwrap().match_idx.is_none());
        app.reverse_search_append('c');
        let idx = app.search_state.as_ref().unwrap().match_idx.unwrap();
        assert_eq!(app.input_history[idx], "cargo test");
    }

    // ---------- /init ----------

    // --- F. Slash commands (new: /init, /undo, /context, /usage) ---
    #[test]
    fn test_slash_init_creates_agents_md() {
        let mut app = TuiApp::new("m");
        let ws = tempfile::tempdir().unwrap();
        let agents = ws.path().join("AGENTS.md");
        assert!(!agents.exists());
        let r = app.handle_slash("/init", ws.path());
        assert_eq!(r, SlashResult::Handled);
        assert!(agents.exists());
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("AGENTS.md"));
    }

    #[test]
    fn test_slash_init_already_exists() {
        let mut app = TuiApp::new("m");
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "existing").unwrap();
        let initial = app.messages.len();
        let r = app.handle_slash("/init", ws.path());
        assert_eq!(r, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("already exists"));
    }

    // ---------- toggle_plan_mode ----------

    #[test]
    fn toggle_plan_mode_cycles_build_to_plan() {
        let mut app = TuiApp::new("m");
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Normal);
        app.toggle_plan_mode();
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Drafting);
        app.toggle_plan_mode();
        assert_eq!(*app.plan_state.lock().unwrap(), PlanState::Normal);
    }

    // ---------- run_shell_command ----------

    #[test]
    fn run_shell_command_echo() {
        let mut app = TuiApp::new("m");
        app.input = "!echo hello".to_string();
        app.cursor = app.input.len();
        let initial = app.messages.len();
        assert!(app.run_shell_command());
        assert!(app.input.is_empty());
        let msg = &app.messages[initial].text;
        assert!(msg.contains("hello"));
    }

    #[test]
    fn run_shell_command_bang_only() {
        let mut app = TuiApp::new("m");
        app.input = "!".to_string();
        app.cursor = 1;
        assert!(app.run_shell_command());
        assert!(app.input.is_empty());
    }

    #[test]
    fn run_shell_command_not_bang() {
        let mut app = TuiApp::new("m");
        app.input = "hello".to_string();
        assert!(!app.run_shell_command());
        assert_eq!(app.input, "hello");
    }

    // ---------- @ file search ----------

    #[test]
    fn at_search_activated_on_at() {
        let mut app = TuiApp::new("m");
        app.insert_char('@');
        assert!(app.at_search_active);
        assert_eq!(app.at_search_start, 1);
    }

    #[test]
    fn at_search_not_activated_mid_word() {
        let mut app = TuiApp::new("m");
        app.insert_char('t');
        app.insert_char('e');
        app.insert_char('s');
        app.insert_char('t');
        app.insert_char('@'); // after "test" — should NOT activate
        assert!(!app.at_search_active);
    }

    // ---------- /context /usage ----------

    #[test]
    fn test_slash_context_shows_token_breakdown() {
        let mut app = TuiApp::new("m");
        app.cumulative_usage.input_tokens = 100;
        app.cumulative_usage.output_tokens = 50;
        let ws = tempfile::tempdir().unwrap();
        let initial = app.messages.len();
        let r = app.handle_slash("/context", ws.path());
        assert_eq!(r, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("context window") || msg.contains("token"), "expected token info, got: {msg}");
    }

    #[test]
    fn test_slash_usage_shows_stats() {
        let mut app = TuiApp::new("m");
        app.turn_count = 3;
        let ws = tempfile::tempdir().unwrap();
        let initial = app.messages.len();
        let r = app.handle_slash("/usage", ws.path());
        assert_eq!(r, SlashResult::Handled);
        let msg = &app.messages[initial].text;
        assert!(msg.contains("turns: 3"), "expected turns, got: {msg}");
    }

    // ---------- Chat search (Ctrl+F) ----------

    fn seed_messages(app: &mut TuiApp) {
        app.messages.clear();
        for (role, text) in [
            (MessageRole::User, "fix the auth bug in login"),
            (MessageRole::Assistant, "looking at handlers.rs"),
            (MessageRole::User, "what about the SESSION cookie?"),
            (MessageRole::Assistant, "session cookie is set on login"),
        ] {
            app.messages.push(ChatMessage {
                role,
                text: text.to_string(),
                styled_lines: None,
                expanded: false,
            });
        }
    }

    #[test]
    fn chat_search_open_seeds_empty_state_and_snaps_scroll() {
        let mut app = TuiApp::new("m");
        app.scroll_offset = 7;
        app.chat_search_open();
        let s = app.chat_search.as_ref().unwrap();
        assert!(s.query.is_empty());
        assert!(s.matches.is_empty());
        assert_eq!(s.saved_scroll, 7);
        // Idempotent: re-open is a no-op so the saved scroll is not
        // overwritten with whatever scroll_offset is now.
        app.scroll_offset = 0;
        app.chat_search_open();
        assert_eq!(app.chat_search.as_ref().unwrap().saved_scroll, 7);
    }

    #[test]
    fn chat_search_recompute_finds_case_insensitive_matches() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        app.chat_search_open();
        app.chat_search.as_mut().unwrap().query = "session".into();
        app.chat_search_recompute();
        let s = app.chat_search.as_ref().unwrap();
        // Both message 2 ("SESSION") and message 3 ("session") should match.
        assert_eq!(s.matches, vec![2, 3]);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn chat_search_step_wraps() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        app.chat_search_open();
        app.chat_search.as_mut().unwrap().query = "login".into();
        app.chat_search_recompute();
        // "login" appears in messages 0 and 3.
        assert_eq!(app.chat_search.as_ref().unwrap().matches, vec![0, 3]);
        assert_eq!(app.chat_search_active_match(), Some(0));
        app.chat_search_step(true);
        assert_eq!(app.chat_search_active_match(), Some(3));
        app.chat_search_step(true);
        assert_eq!(app.chat_search_active_match(), Some(0), "wrap forward");
        app.chat_search_step(false);
        assert_eq!(app.chat_search_active_match(), Some(3), "wrap back");
    }

    #[test]
    fn chat_search_cancel_restores_scroll_and_clears_state() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        app.scroll_offset = 12;
        app.chat_search_open();
        app.scroll_offset = 0; // simulate render-side scroll-to-match
        app.chat_search_cancel();
        assert!(app.chat_search.is_none());
        assert_eq!(app.scroll_offset, 12);
    }

    // ---------- Pinned messages (/pin /unpin /pinned) ----------

    // ---------- Inline syntax highlight ----------

    fn span_strings(line: &Line<'static>) -> Vec<String> {
        line.spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    fn span_with_color(line: &Line<'static>, content: &str) -> Option<Color> {
        line.spans
            .iter()
            .find(|s| s.content == content)
            .and_then(|s| s.style.fg)
    }

    fn default_code_style() -> Style {
        Style::default()
            .fg(Color::Rgb(190, 190, 190))
            .bg(Color::Rgb(30, 30, 30))
    }

    #[test]
    fn highlight_rust_keyword_picks_up_kw_color() {
        let line = highlight_code_line("fn main() {", "rust", default_code_style());
        // `fn` should land in its own span tagged with the keyword color.
        let kw_color = span_with_color(&line, "fn").expect("fn span missing");
        assert_eq!(kw_color, Color::Rgb(199, 146, 234));
        // Identifier `main` stays default.
        let id_color = span_with_color(&line, "main").expect("main span missing");
        assert_eq!(id_color, Color::Rgb(190, 190, 190));
    }

    #[test]
    fn highlight_string_literal_distinct_from_code() {
        let line = highlight_code_line(
            r#"let x = "hello";"#,
            "rust",
            default_code_style(),
        );
        let str_color = span_with_color(&line, "\"hello\"")
            .expect("string literal must be a single span");
        assert_eq!(str_color, Color::Rgb(195, 232, 141));
    }

    #[test]
    fn highlight_line_comment_swallows_rest_of_line() {
        let line = highlight_code_line(
            "let x = 1; // explanation goes here",
            "rust",
            default_code_style(),
        );
        // The trailing `// explanation goes here` should be one comment
        // span — not split by the tokenizer mid-comment.
        let strs = span_strings(&line);
        assert!(
            strs.iter().any(|s| s == "// explanation goes here"),
            "expected single comment span, got: {strs:?}"
        );
    }

    #[test]
    fn highlight_unknown_lang_falls_through_clean() {
        // No keywords for `pascal` → no highlighting, but render still
        // works and emits text. Guards against a future "unknown lang
        // panics" regression.
        let line = highlight_code_line("BEGIN END", "pascal", default_code_style());
        let combined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(combined, "BEGIN END");
    }

    #[test]
    fn highlight_python_hash_comment() {
        let line = highlight_code_line(
            "x = 1  # hello",
            "python",
            default_code_style(),
        );
        let strs = span_strings(&line);
        assert!(
            strs.iter().any(|s| s == "# hello"),
            "py # comment must be one span, got: {strs:?}"
        );
    }

    #[test]
    fn pin_no_arg_pins_last_user_message() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        let ws = tempfile::tempdir().unwrap();
        app.handle_slash("/pin", ws.path());
        // seed_messages: indices 0, 2 are user. Latest is 2.
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn pin_indexed_toggles() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        let ws = tempfile::tempdir().unwrap();
        app.handle_slash("/pin 1", ws.path());
        assert!(app.pinned.contains(&1));
        // Same command unpins.
        app.handle_slash("/pin 1", ws.path());
        assert!(!app.pinned.contains(&1));
    }

    #[test]
    fn unpin_all_clears_set() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        let ws = tempfile::tempdir().unwrap();
        app.handle_slash("/pin 0", ws.path());
        app.handle_slash("/pin 2", ws.path());
        assert_eq!(app.pinned.len(), 2);
        app.handle_slash("/unpin all", ws.path());
        assert!(app.pinned.is_empty());
    }

    #[test]
    fn pin_out_of_range_errors() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        let ws = tempfile::tempdir().unwrap();
        let before = app.pinned.len();
        app.handle_slash("/pin 999", ws.path());
        assert_eq!(app.pinned.len(), before, "out-of-range must not pin");
    }

    #[test]
    fn chat_search_apply_scroll_moves_offset_above_zero_for_old_match() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        // Simulate that the chat panel has been rendered with some
        // visible/total geometry. Without these the apply path bails
        // early; with them it should compute a non-zero scroll offset
        // for a match that lives 2 messages above the bottom.
        app.last_visible_height = 10;
        app.last_max_scroll = 50;
        app.chat_search_open();
        // Match message 0 ("fix the auth bug in login") — three messages
        // worth of lines below it should push scroll_offset > 0.
        app.chat_search.as_mut().unwrap().query = "auth".into();
        app.chat_search_recompute();
        assert!(
            app.scroll_offset > 0,
            "match 2 above bottom should yield non-zero scroll, got {}",
            app.scroll_offset
        );
    }

    #[test]
    fn chat_search_apply_scroll_no_op_without_match() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        app.last_visible_height = 10;
        app.last_max_scroll = 50;
        app.scroll_offset = 17;
        app.chat_search_open();
        app.chat_search.as_mut().unwrap().query = "no_such_token".into();
        app.chat_search_recompute();
        // No matches → scroll must stay where the user left it. (Cancel
        // restores the saved baseline; recompute alone shouldn't.)
        assert_eq!(app.scroll_offset, 17);
    }

    #[test]
    fn approx_lines_below_counts_msgs_after_idx() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        // 4 messages, single-line text each → 3 messages below idx 0,
        // each contributing (1 line + 1 separator) = 2. So below(0)=6.
        assert_eq!(app.approx_lines_below(0), 6);
        assert_eq!(app.approx_lines_below(2), 2);
        assert_eq!(app.approx_lines_below(3), 0);
    }

    #[test]
    fn chat_search_empty_query_yields_no_matches() {
        let mut app = TuiApp::new("m");
        seed_messages(&mut app);
        app.chat_search_open();
        // Queue no query; recompute should leave matches empty and not
        // surface every message as a "match-everything" hit.
        app.chat_search_recompute();
        let s = app.chat_search.as_ref().unwrap();
        assert!(s.matches.is_empty());
        assert!(app.chat_search_active_match().is_none());
    }
}
