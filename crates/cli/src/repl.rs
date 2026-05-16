//! Interactive REPL mode for `goblin`.
//!
//! Drops into a `aegis> ` prompt, runs each line of input through the
//! agent loop, and prints the model's reply. Multi-turn conversation is
//! kept on disk via [`SessionStore`] — the same machinery `--resume`
//! uses for one-shot mode — so the REPL doesn't need to mirror the
//! transcript itself and Ctrl-C interrupting a run still leaves the
//! prior turns intact.
//!
//! Slash commands are intercepted before the agent sees them. Anything
//! that doesn't start with `/` becomes the next user prompt.
//!
//! Design notes:
//!
//! * **Agent rebuilt on `/clear`.** Wiping the conversation means
//!   minting a new session id and constructing a fresh `Agent`. The
//!   alternative — adding an in-place reset method — would push REPL
//!   concerns into the core crate. Rebuilding is a couple of cheap
//!   borrows and keeps the public API surface unchanged.
//! * **Cumulative usage tracked here.** `Agent::run` returns the usage
//!   for the run only, but the REPL is the place where "session cost so
//!   far" makes sense, so we sum into a local counter and print it on
//!   `/cost` and on exit.
//! * **History on disk, opt-in directory.** rustyline writes its line
//!   history to `<workspace>/.aegis/repl.history`. Sharing the
//!   workspace `.aegis` dir with sessions keeps the layout single-rooted
//!   so deleting `.aegis` cleans the slate.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use aegis_api::ChatProvider;
use aegis_core::{
    format_cost_footer, Agent, AgentConfig, BudgetStatus, Permission,
    SessionStore, ToolRegistry, UsageSnapshot, spawn_mcp_server,
};

use crate::markdown::MdRenderer;
use rustyline::error::ReadlineError;
use rustyline::Editor;

mod builder;
mod command;
pub mod dag;
pub mod format;
mod helper;
mod overlay;
pub mod search;
mod spinner;
use builder::build_agent;
pub use command::normalize_model_name;
use command::{print_help, ReplCommand};
use dag::{format_dag, format_session_tree};
use format::{format_size, format_time_ago, highlight_line};
use helper::{is_stop_signal, MetisHelper};
#[cfg(unix)]
use overlay::InputOverlay;
use overlay::{drain_pending_lines, erase_input_line};
use search::{highlight_pattern, parse_search_options, search_directory, SearchResult};
use spinner::ThinkingSpinner;

/// Path to the rustyline history file inside the workspace `.metis` dir.
fn history_path(workspace: &Path) -> PathBuf {
    workspace.join(".metis").join("repl.history")
}

/// Validate + attach a single image path for REPL `/image`. Mirrors
/// `TuiApp::attach_image_path` but prints to stderr since REPL has no
/// ratatui buffer. Supports HEIC/HEIF via `sips` conversion.
fn attach_image_path_repl(path: &Path, pending: &mut Vec<PathBuf>) {
    use crate::path_input::{prepare_image, ImagePrep};
    match prepare_image(path) {
        ImagePrep::Ok(p) => {
            eprintln!(
                "\x1b[35m[image]\x1b[0m attached: {}",
                p.file_name().unwrap_or_default().to_string_lossy()
            );
            pending.push(p);
        }
        ImagePrep::NotFound(p) => {
            eprintln!("[goblin] file not found: {}", p.display());
        }
        ImagePrep::Unsupported(ext) => {
            eprintln!("[goblin] unsupported image format: .{ext} (use png/jpg/heic/gif/webp/bmp)");
        }
        ImagePrep::ConversionFailed(e) => {
            eprintln!("[goblin] HEIC conversion failed: {e} (install `sips` or pre-convert)");
        }
    }
}

use crate::markdown::resolve_dedup_enabled;

#[derive(Debug, Clone, Copy)]
enum BudgetHardStopChoice {
    Continue,
    Always,
    Stop,
}

/// Prompt the user when the running spend has crossed `daily_budget_usd`
/// and `budget_hard_stop` is on. On non-TTY invocations (scripted /
/// piped / CI) the call treats "no human at the keyboard" as a hard
/// `Stop` — the whole point of the flag is to keep runaway autonomous
/// sessions from silently burning past the cap. This is the same
/// safety posture as the EOF-denies-permission fix in the mutating
/// tool path (crates/core/src/permission.rs).
/// One-line, actionable recovery hint for user-facing AgentError prints.
/// The raw `err.to_string()` already includes the technical details;
/// this adds "what to do next" so the user knows the session is still
/// alive and how to continue.
fn recovery_hint_for_error(err: &aegis_core::AgentError) -> &'static str {
    match err {
        aegis_core::AgentError::LoopDetected { .. } => {
            "model kept emitting the same tool call after every result was an error — \
             rephrase the request or switch model with /model. Session is preserved; \
             type a follow-up to keep going."
        }
        aegis_core::AgentError::TextLoopDetected { .. } => {
            "model repeated the same paragraph 3 turns running — try a different \
             model with /model or rephrase. Session is preserved."
        }
        aegis_core::AgentError::MaxTurns(_) => {
            "agent hit the per-run turn cap. Send a follow-up to continue, or raise \
             max_turns in goblin.toml."
        }
        aegis_core::AgentError::Api(_) => {
            "provider call failed after retry. Check API key / network, or /model to \
             switch providers. Session is preserved."
        }
        aegis_core::AgentError::NoChoices => {
            "provider returned an empty response. Often a transient rate-limit; retry \
             the same prompt or /model to switch."
        }
        aegis_core::AgentError::GuardrailBlocked { .. } => {
            "output guardrail fired — adjust the banlist or rephrase the request."
        }
        aegis_core::AgentError::BadToolArgs(_) => {
            "model emitted invalid tool arguments. Usually self-corrects on retry."
        }
        aegis_core::AgentError::Session(_) => {
            "session store error — check disk space and `.metis/sessions/` permissions."
        }
        aegis_core::AgentError::Config(_) => "config error — fix goblin.toml and restart.",
        aegis_core::AgentError::BudgetExceeded { .. } => {
            "cost budget exceeded — raise the limit in goblin.toml or pass --budget."
        }
    }
}

fn budget_hard_stop_prompt(spent: f64, limit: f64) -> BudgetHardStopChoice {
    use std::io::{self, BufRead, IsTerminal, Write};
    let over = spent - limit;
    eprintln!();
    eprintln!("\x1b[1;33m╭─ budget exceeded ──────────────────────╮\x1b[0m");
    eprintln!("\x1b[1;33m│\x1b[0m  today: ${spent:.4} / ${limit:.2}  (+${over:.4} over)");
    eprintln!("\x1b[1;33m│\x1b[0m  [y] continue this turn   [a] always this session   [n] stop");
    eprintln!("\x1b[1;33m╰────────────────────────────────────────╯\x1b[0m");
    if !io::stdin().is_terminal() {
        eprintln!(
            "\x1b[1;33m[goblin]\x1b[0m non-TTY session — treating as [n] stop \
             (set budget_hard_stop = false to override in scripts)."
        );
        return BudgetHardStopChoice::Stop;
    }
    eprint!("> ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let stdin = io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return BudgetHardStopChoice::Stop;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => BudgetHardStopChoice::Continue,
        "a" | "always" => BudgetHardStopChoice::Always,
        _ => BudgetHardStopChoice::Stop,
    }
}

/// Runs the interactive loop until the user exits. Borrows the client
/// and registry from `main::run` so the REPL doesn't need to know how
/// either was constructed.
#[allow(clippy::too_many_arguments)]
pub async fn run_repl(
    client: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    model: &str,
    provider: &str,
    sandbox: aegis_core::SandboxMode,
    routing: crate::router::RoutingConfig,
    initial_session: Option<SessionStore>,
    daily_budget_usd: Option<f64>,
    budget_hard_stop: bool,
    #[cfg(feature = "ctx")] blob_handles: Option<(
        Arc<aegis_core::BlobStore>,
        Arc<aegis_core::BlobIndex>,
    )>,
) -> Result<()> {
    let mut client = client;
    let mut model = model.to_string();
    let mut provider = provider.to_string();
    let mut config = config;
    let original_routing = routing.clone();
    let mut routing = routing;
    // Make sure `.metis/` exists before we hand the path to rustyline,
    // otherwise the first save_history call would fail with ENOENT.
    let metis_dir = workspace.join(".metis");
    std::fs::create_dir_all(&metis_dir)
        .with_context(|| format!("could not create `{}`", metis_dir.display()))?;

    let history_file = history_path(workspace);
    let helper = MetisHelper::new(workspace);
    let mut editor = Editor::new().context("could not initialize line editor")?;
    editor.set_helper(Some(helper));
    let _ = editor.load_history(&history_file); // missing file is fine

    // Graceful shutdown on SIGTERM (e.g. terminal closed, process killed).
    // Sets a flag the main loop checks so the normal exit path runs,
    // including ft export and cost footer.
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let shutdown2 = Arc::clone(&shutdown);
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut stream) = signal(SignalKind::terminate()) {
                    stream.recv().await;
                    shutdown2.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }

    let plan_state_flag = Arc::new(Mutex::new(aegis_core::PlanState::Normal));
    let md_renderer = {
        let mut r = MdRenderer::new();
        r.set_dedup_enabled(resolve_dedup_enabled(&provider));
        Arc::new(Mutex::new(r))
    };
    let spinner = Arc::new(ThinkingSpinner::new());
    #[cfg(unix)]
    let overlay = Arc::new(InputOverlay::new());
    // Honor --resume: if the caller pre-loaded a session (either by id
    // or by resolving the most recent one), adopt it here so the first
    // agent instance replays its transcript instead of minting a fresh
    // session.
    let is_resumed = initial_session.is_some();
    if let Some(s) = &initial_session {
        eprintln!(
            "[goblin] resumed session={} ({} prior messages)",
            s.id(),
            s.messages().len()
        );
    }
    let mut agent = build_agent(
        &*client,
        &registry,
        workspace,
        config.clone(),
        Arc::clone(&permission),
        initial_session,
        Some(Arc::clone(&plan_state_flag)),
        Arc::clone(&md_renderer),
        Arc::clone(&client),
        Arc::clone(&registry),
        sandbox.clone(),
        Arc::clone(&spinner),
        #[cfg(unix)]
        Arc::clone(&overlay),
        #[cfg(feature = "ctx")]
        blob_handles.clone(),
    )?;
    let mut total = UsageSnapshot::default();
    let mut turn_count: usize = 0;
    let mut thinking_enabled = config.thinking;
    let mut pending_images: Vec<std::path::PathBuf> = Vec::new();
    // Session-scoped override for the budget hard-stop prompt. Once
    // the user picks "[a] always" we stop re-prompting for the rest
    // of this REPL session; a fresh `metis --resume` starts clean.
    let mut budget_stop_overridden = false;

    // Discover skills from filesystem + builtins
    let mut skill_registry = aegis_core::SkillRegistry::discover(workspace);
    for skill in aegis_core::builtin_skills() {
        skill_registry.register(skill);
    }

    eprintln!("\x1b[1;32m[goblin] model: {model} | type /help for commands, /exit to leave\x1b[0m");

    // Budget banner: only surfaces when a daily_budget_usd is set so the
    // no-config path stays silent. Uses prior-session telemetry; the
    // current session's spend is $0 at this point.
    if let Some(budget) = daily_budget_usd {
        let status = BudgetStatus {
            prior_usd: aegis_core::spent_today(),
            session_usd: 0.0,
            budget_usd: Some(budget),
        };
        let marker = if status.over_budget() { "!" } else { "·" };
        eprintln!("\x1b[2m[goblin] {marker} {}\x1b[0m", status.summary());
    }

    let mut last_cron_check = std::time::Instant::now();
    let mut cost_footer_enabled = true;
    // Show cost footer at most once per 5 minutes; init to 5 min ago so
    // the very first turn still prints.
    let mut last_cost_shown = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(300))
        .unwrap_or_else(std::time::Instant::now);

    // Lines the user typed and submitted with Enter while a previous
    // `agent.run` was streaming. Filled by `drain_pending_lines()`
    // after each turn; consumed at the top of the loop in place of
    // a fresh `editor.readline` call. FIFO so the queue runs in the
    // order the user typed it.
    let mut pending_inputs: VecDeque<String> = VecDeque::new();
    // Partial input the user was typing when the agent finished — pre-fill readline
    let mut pending_partial: String = String::new();

    // Auto-opening message: fire a synthetic first turn before waiting for user input.
    // Only on new sessions (not --resume). The agent's session_start hook injects
    // memory context, so the model can write an informed opening message.
    if !is_resumed {
        let hook_cfg = aegis_core::load_hooks(workspace);
        if let Some(opener) = hook_cfg.opening_prompt {
            pending_inputs.push_front(opener);
        }
    }

    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Poll cron scheduler every 60 seconds
        if last_cron_check.elapsed() >= std::time::Duration::from_secs(60) {
            let results = aegis_core::cron::run_due_crons(workspace);
            for (id, cmd, output) in &results {
                eprintln!(
                    "\x1b[33m[cron #{id}]\x1b[0m `{cmd}` → {}",
                    output.lines().next().unwrap_or("(empty)")
                );
            }
            last_cron_check = std::time::Instant::now();
        }

        let (prompt, prompt_visible_width) = {
            let state = plan_state_flag.lock().unwrap();
            // Show the active brain model in bracketed cyan inside the
            // prompt so the user always knows which provider is actually
            // answering. An earlier iteration dimmed it (\x1b[2m) which
            // the user missed for hours and ended up thinking they were
            // on a different model — the cost implications were real,
            // so visibility beats minimalism here. Plan/exec markers
            // keep their colored prefix in front of the model tag.
            //
            // The `_visible` length (trailing return tuple field) is the
            // display-column count without the ANSI escape sequences so
            // `erase_input_line` can rewrap long inputs correctly.
            let model_len = model.chars().count();
            let (base, base_visible) = match *state {
                aegis_core::PlanState::Drafting => (
                    format!("\x1b[33mmetis [plan]\x1b[0m \x1b[38;2;220;50;50m[{model}]\x1b[0m> "),
                    // "metis [plan] [" + model + "]> "
                    "metis [plan] [".len() + model_len + "]> ".len(),
                ),
                aegis_core::PlanState::Executing => (
                    format!("\x1b[32mmetis [exec]\x1b[0m \x1b[38;2;220;50;50m[{model}]\x1b[0m> "),
                    "metis [exec] [".len() + model_len + "]> ".len(),
                ),
                aegis_core::PlanState::Normal => (
                    format!("metis \x1b[38;2;220;50;50m[{model}]\x1b[0m> "),
                    "metis [".len() + model_len + "]> ".len(),
                ),
            };
            if pending_images.is_empty() {
                (base, base_visible)
            } else {
                let n = pending_images.len();
                let suffix = if n == 1 { "image" } else { "images" };
                let img_tag = format!("[{n} {suffix}] ");
                let img_visible = img_tag.chars().count();
                let full = format!("\x1b[35m{img_tag}\x1b[0m{base}");
                (full, img_visible + base_visible)
            }
        };
        // If the user typed-and-submitted any lines while a previous
        // turn was streaming, those lines were captured into
        // `pending_inputs` instead of being discarded. Process them
        // FIFO before going back to rustyline. We echo the queued
        // line under the prompt so the conversation log still shows
        // what the user actually entered, just rendered after the
        // fact.
        let line = if let Some(queued) = pending_inputs.pop_front() {
            eprintln!("{prompt}{queued} \x1b[2m(queued)\x1b[0m");
            queued
        } else {
            let partial = std::mem::take(&mut pending_partial);
            let readline_result = if partial.is_empty() {
                editor.readline(&prompt)
            } else {
                editor.readline_with_initial(&prompt, (&partial, ""))
            };
            match readline_result {
                Ok(l) => l,
                // Ctrl-C: cancel current input but stay in the REPL — same
                // contract as bash and python's repl.
                Err(ReadlineError::Interrupted) => {
                    eprintln!("(interrupted — type /exit to leave)");
                    continue;
                }
                // Ctrl-D on an empty line is the polite way to exit.
                Err(ReadlineError::Eof) => {
                    eprintln!();
                    break;
                }
                Err(err) => {
                    eprintln!("[goblin] readline error: {err}");
                    break;
                }
            }
        };

        let cmd = ReplCommand::parse(&line);
        match cmd {
            ReplCommand::Empty => continue,
            ReplCommand::Exit => break,
            ReplCommand::Help => {
                print_help();
                continue;
            }
            ReplCommand::SlashMenu => {
                eprintln!("\x1b[2m  /help  /cost  /clear  /providers  /provider  /model  /swarm  /plan  /overthink\x1b[0m");
                eprintln!("\x1b[2m  /skills  /tasks  /image  /compact  /stats  /fork  /sessions  /exit\x1b[0m");
                continue;
            }
            ReplCommand::Shell(ref cmd) => {
                let _ = editor.add_history_entry(&line);
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(workspace)
                    .status();
                match status {
                    Ok(s) if !s.success() => {
                        eprintln!("[goblin] command exited with {}", s.code().unwrap_or(-1));
                    }
                    Err(e) => eprintln!("[goblin] could not run command: {e}"),
                    _ => {}
                }
                continue;
            }
            ReplCommand::Cost => {
                eprint!(
                    "{}",
                    aegis_core::format_cost_breakdown(&total, &model, turn_count, true)
                );
                continue;
            }
            ReplCommand::CostOff => {
                cost_footer_enabled = false;
                eprintln!("\x1b[2m[goblin] cost footer off\x1b[0m");
                continue;
            }
            ReplCommand::CostOn => {
                cost_footer_enabled = true;
                eprintln!("\x1b[2m[goblin] cost footer on\x1b[0m");
                continue;
            }
            ReplCommand::Providers => {
                use aegis_api::Provider;
                eprintln!("\n\x1b[1mAvailable providers:\x1b[0m\n");
                for p in Provider::BUILTINS {
                    let has_key = std::env::var(p.env_var).is_ok();
                    // Detect active provider by checking if current model
                    // matches this provider's default or known models.
                    let status = if has_key {
                        "\x1b[32m● ready\x1b[0m "
                    } else {
                        "\x1b[2m· no key\x1b[0m"
                    };
                    eprintln!(
                        "  {status}  \x1b[36m{:<12}\x1b[0m  \x1b[2m{:<24}\x1b[0m  {}",
                        p.id, p.default_model, p.env_var,
                    );
                }
                eprintln!("\n  \x1b[2mcurrent: \x1b[0m\x1b[1m{model}\x1b[0m");
                eprintln!("  \x1b[2mswitch:  /provider <name>\x1b[0m\n");
                continue;
            }
            ReplCommand::Image(ref raw) => {
                let paths = crate::path_input::resolve_many(raw, workspace);
                for resolved in paths {
                    attach_image_path_repl(&resolved, &mut pending_images);
                }
                continue;
            }
            ReplCommand::Images => {
                if pending_images.is_empty() {
                    eprintln!("[goblin] no images attached. Use /image <path> to attach one.");
                } else {
                    eprintln!("[goblin] attached images:");
                    for (i, p) in pending_images.iter().enumerate() {
                        eprintln!("  {}. {}", i + 1, p.display());
                    }
                    eprintln!("Use /images clear to remove all.");
                }
                continue;
            }
            ReplCommand::ImagesClear => {
                let n = pending_images.len();
                pending_images.clear();
                eprintln!("[goblin] cleared {n} attached image(s).");
                continue;
            }
            ReplCommand::Paste => {
                match crate::path_input::paste_image_from_clipboard() {
                    Ok(p) => attach_image_path_repl(&p, &mut pending_images),
                    Err(e) => eprintln!("[goblin] /paste failed: {e}"),
                }
                continue;
            }
            ReplCommand::Files(ref path) => {
                let _ = editor.add_history_entry(&line);
                let target_path = path
                    .as_ref()
                    .map(|p| workspace.join(p))
                    .unwrap_or_else(|| workspace.to_path_buf());
                if !target_path.exists() {
                    eprintln!("[goblin] path not found: {}", target_path.display());
                    continue;
                }

                // Check if it's a file or directory
                if target_path.is_file() {
                    eprintln!(
                        "[goblin] {} is a file, not a directory",
                        target_path.display()
                    );
                    eprintln!(
                        "  Use /view {} to see its contents",
                        path.as_ref().map(|s| s.as_str()).unwrap_or("")
                    );
                    continue;
                }

                eprintln!("[goblin] browsing files at: {}", target_path.display());

                // Get directory info
                let mut dirs: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
                let mut files: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
                let mut total_size: u64 = 0;

                if let Ok(entries) = std::fs::read_dir(&target_path) {
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

                    // Sort: directories first, then files, both alphabetically
                    dirs.sort_by_key(|d| d.0.to_lowercase());
                    files.sort_by_key(|f| f.0.to_lowercase());

                    // Print with more details
                    if !dirs.is_empty() {
                        eprintln!("\n  \x1b[1mDirectories:\x1b[0m");
                        for (name, size, modified) in &dirs {
                            let modified_str = format_time_ago(*modified);
                            eprintln!(
                                "    \x1b[34m{}/\x1b[0m  {:>10}  {}",
                                name,
                                format_size(*size),
                                modified_str
                            );
                        }
                    }

                    if !files.is_empty() {
                        eprintln!("\n  \x1b[1mFiles:\x1b[0m");
                        for (name, size, modified) in &files {
                            let modified_str = format_time_ago(*modified);
                            let ext = std::path::Path::new(name)
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("");
                            let color = match ext {
                                "rs" | "cpp" | "c" | "h" | "hpp" | "py" | "js" | "ts" | "java"
                                | "go" => "\x1b[33m", // yellow for code
                                "md" | "txt" | "json" | "toml" | "yaml" | "yml" => "\x1b[36m", // cyan for docs/config
                                "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => "\x1b[35m", // magenta for images
                                _ => "\x1b[0m", // default
                            };
                            eprintln!(
                                "    {}{}\x1b[0m  {:>10}  {}",
                                color,
                                name,
                                format_size(*size),
                                modified_str
                            );
                        }
                    }

                    eprintln!(
                        "\n[goblin] total: {} directories, {} files, {} total size",
                        dirs.len(),
                        files.len(),
                        format_size(total_size)
                    );

                    // Show relative path to workspace
                    if target_path != workspace {
                        let rel_path = target_path
                            .strip_prefix(workspace)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| target_path.display().to_string());
                        eprintln!("  (relative to workspace: {})", rel_path);
                    }
                } else {
                    eprintln!("[goblin] cannot read directory: {}", target_path.display());
                }
                continue;
            }
            ReplCommand::View(ref path_str) => {
                let _ = editor.add_history_entry(&line);
                let path = workspace.join(path_str);
                if !path.exists() {
                    eprintln!("[goblin] file not found: {}", path.display());
                    continue;
                }
                if path.is_dir() {
                    eprintln!("[goblin] cannot view directory: {}", path.display());
                    continue;
                }

                // Get file info
                match std::fs::metadata(&path) {
                    Ok(metadata) => {
                        let size = metadata.len();
                        let modified = metadata
                            .modified()
                            .unwrap_or_else(|_| std::time::SystemTime::now());
                        let modified_str = format_time_ago(modified);

                        eprintln!("[goblin] previewing: {}", path.display());
                        eprintln!("  size: {}, modified: {}", format_size(size), modified_str);

                        // Determine file type for highlighting
                        let ext = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();

                        let is_text_file = match ext.as_str() {
                            "rs" | "py" | "js" | "ts" | "java" | "c" | "cpp" | "h" | "hpp"
                            | "go" | "md" | "txt" | "json" | "toml" | "yaml" | "yml" | "xml"
                            | "html" | "css" | "sh" | "bash" | "zsh" | "fish" | "sql" | "csv"
                            | "log" | "rs.bak" => true,
                            _ => {
                                // Check if it's binary by reading first few bytes
                                match std::fs::read(&path) {
                                    Ok(bytes) => {
                                        !bytes.is_empty()
                                            && !bytes.iter().all(|b| {
                                                b.is_ascii_graphic()
                                                    || *b == b' '
                                                    || *b == b'\n'
                                                    || *b == b'\r'
                                                    || *b == b'\t'
                                            })
                                    }
                                    Err(_) => false,
                                }
                            }
                        };

                        if !is_text_file {
                            eprintln!("[goblin] binary file detected (not displaying content)");
                            eprintln!("  use /files to browse, or !cat with appropriate flags");
                            continue;
                        }

                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                let max_lines = 100;
                                let lines: Vec<&str> = content.lines().collect();

                                if lines.len() > max_lines {
                                    eprintln!(
                                        "[goblin] showing first {} of {} lines:",
                                        max_lines,
                                        lines.len()
                                    );
                                } else {
                                    eprintln!("[goblin] showing all {} lines:", lines.len());
                                }

                                // Basic syntax highlighting based on file extension
                                for (i, line) in lines.iter().enumerate().take(max_lines) {
                                    let line_num = i + 1;
                                    let colored_line = if line.is_empty() {
                                        "\x1b[2m⟨empty⟩\x1b[0m".to_string()
                                    } else {
                                        highlight_line(line, &ext)
                                    };
                                    eprintln!("  \x1b[90m{:>4}:\x1b[0m {}", line_num, colored_line);
                                }

                                if lines.len() > max_lines {
                                    eprintln!(
                                        "  \x1b[90m... ({} more lines)\x1b[0m",
                                        lines.len() - max_lines
                                    );
                                }

                                // Show line stats
                                let empty_lines =
                                    lines.iter().filter(|l| l.trim().is_empty()).count();
                                let non_empty_lines = lines.len() - empty_lines;
                                let longest_line =
                                    lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
                                eprintln!("  \x1b[90mstats: {} lines ({} non-empty), longest: {} chars\x1b[0m", 
                                    lines.len(), non_empty_lines, longest_line);
                            }
                            Err(e) => {
                                eprintln!("[goblin] could not read file: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[goblin] could not get file info: {e}");
                    }
                }
                continue;
            }
            ReplCommand::Search(ref pattern) => {
                let _ = editor.add_history_entry(&line);

                // Parse search options
                let (pattern, case_sensitive, use_regex, max_results, file_types) =
                    parse_search_options(pattern);

                eprintln!("[goblin] searching for pattern: \"{}\"", pattern);
                if use_regex {
                    eprintln!("  mode: regex");
                } else {
                    eprintln!("  mode: literal text");
                }
                if !case_sensitive {
                    eprintln!("  case: insensitive");
                }
                if !file_types.is_empty() {
                    eprintln!("  file types: {}", file_types.join(", "));
                }

                let mut results: Vec<SearchResult> = Vec::new();
                let start_time = std::time::Instant::now();
                let files_searched = search_directory(
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
                    eprintln!(
                        "[goblin] no matches found in {} files ({:.2}s)",
                        files_searched,
                        elapsed.as_secs_f32()
                    );
                    continue;
                }

                eprintln!(
                    "[goblin] found {} matches in {} files ({:.2}s)",
                    results.len(),
                    files_searched,
                    elapsed.as_secs_f32()
                );

                // Group by file
                let mut file_groups: std::collections::HashMap<String, Vec<SearchResult>> =
                    std::collections::HashMap::new();
                for result in results {
                    file_groups
                        .entry(result.file_path.clone())
                        .or_default()
                        .push(result);
                }

                // Sort files by number of matches
                let mut sorted_files: Vec<_> = file_groups.iter().collect();
                sorted_files.sort_by_key(|f| std::cmp::Reverse(f.1.len()));

                // Show top results
                let show_files = sorted_files.len().min(10);
                for (_i, (file_path, matches)) in sorted_files.iter().enumerate().take(show_files) {
                    let rel_path = std::path::Path::new(file_path)
                        .strip_prefix(workspace)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| file_path.to_string());

                    eprintln!("  \x1b[1m{}\x1b[0m ({} matches)", rel_path, matches.len());

                    // Show first 3 matches from each file
                    for result in matches.iter().take(3) {
                        let line_content = result.line_content.trim();
                        let colored_line = if use_regex && case_sensitive {
                            // Highlight regex matches
                            highlight_pattern(line_content, &pattern, true)
                        } else if !case_sensitive {
                            highlight_pattern(line_content, &pattern, false)
                        } else {
                            highlight_pattern(line_content, &pattern, true)
                        };
                        eprintln!(
                            "    \x1b[90m{:>4}:\x1b[0m {}",
                            result.line_number, colored_line
                        );
                    }
                    if matches.len() > 3 {
                        eprintln!(
                            "    \x1b[90m... and {} more lines\x1b[0m",
                            matches.len() - 3
                        );
                    }
                }

                if sorted_files.len() > show_files {
                    eprintln!("  ... and {} more files", sorted_files.len() - show_files);
                }

                continue;
            }
            ReplCommand::MultiModelToggle => {
                eprintln!("[goblin] multi-model evaluation: use CLI flag --multi-model-evaluation");
                continue;
            }
            ReplCommand::PerturbationToggle => {
                eprintln!("[goblin] prompt perturbation: use CLI flag --prompt-perturbation");
                continue;
            }
            ReplCommand::ParallelToggle => {
                eprintln!("[goblin] parallel models: use CLI flag --parallel-models");
                continue;
            }
            ReplCommand::ApiKeysToggle => {
                eprintln!("[goblin] API key management: use CLI flag --api-keys");
                continue;
            }
            ReplCommand::GodmodeToggle => {
                eprintln!("[goblin] GODMODE features: use CLI flags --multi-model-evaluation --prompt-perturbation --parallel-models --api-keys");
                continue;
            }
            ReplCommand::Browser => {
                let spec = "npx -y @playwright/mcp@latest";
                eprintln!("\x1b[2m[goblin] attaching Playwright MCP...\x1b[0m");
                match spawn_mcp_server(spec, &[]).await {
                    Ok(handle) => {
                        let n = handle.register_into_shared(&registry);
                        eprintln!("\x1b[1;32m[goblin] Playwright MCP attached, {n} tools\x1b[0m");
                    }
                    Err(e) => eprintln!("[goblin] failed to attach Playwright: {e}"),
                }
                continue;
            }
            ReplCommand::Computer => {
                let bin = "/Users/macmini/.nvm/versions/node/v20.20.1/bin/open-computer-use";
                eprintln!("\x1b[2m[goblin] attaching open-computer-use MCP...\x1b[0m");
                match spawn_mcp_server(bin, &["mcp".to_string()]).await {
                    Ok(handle) => {
                        let n = handle.register_into_shared(&registry);
                        eprintln!("\x1b[1;32m[goblin] open-computer-use MCP attached, {n} tools\x1b[0m");
                    }
                    Err(e) => eprintln!("[goblin] failed to attach open-computer-use: {e}"),
                }
                continue;
            }
            ReplCommand::Context => {
                eprintln!("[goblin] /context — not yet implemented");
                continue;
            }
            ReplCommand::Tokens => {
                eprintln!("[goblin] /tokens — not yet implemented");
                continue;
            }
            ReplCommand::History => {
                eprintln!("[goblin] /history — not yet implemented");
                continue;
            }
            ReplCommand::Copy => {
                let messages = agent.session_messages();
                let last = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == aegis_api::types::Role::Assistant);
                match last {
                    Some(msg) if msg.content.is_some() => {
                        let text = msg.content.as_ref().unwrap();
                        if aegis_core::display::copy_to_clipboard_osc52(text) {
                            let preview: String = if text.len() > 80 {
                                format!("{}…", &text[..77])
                            } else {
                                text.clone()
                            };
                            eprintln!("[goblin] copied to clipboard: {preview}");
                        } else {
                            eprintln!("[goblin] copy failed — terminal may not support OSC 52");
                        }
                    }
                    Some(_) => eprintln!("[goblin] last assistant message has no text content"),
                    None => eprintln!("[goblin] no assistant message in transcript yet"),
                }
                continue;
            }
            ReplCommand::Unknown(ref name) => {
                // Try skill lookup before reporting unknown command.
                if let Some(skill) = skill_registry.get(name) {
                    if skill.user_invocable {
                        // Extract args: everything after `/name `
                        let args = line
                            .trim()
                            .strip_prefix('/')
                            .unwrap_or("")
                            .strip_prefix(name.as_str())
                            .unwrap_or("")
                            .trim();
                        let expanded = aegis_core::expand_prompt(skill, args);
                        eprintln!(
                            "[goblin] skill \x1b[38;2;122;240;227m/{name}\x1b[0m → {}",
                            if expanded.len() > 60 {
                                format!("{}…", &expanded[..57])
                            } else {
                                expanded.clone()
                            }
                        );
                        // Feed expanded prompt to agent as user message
                        let _ = editor.add_history_entry(&line);
                        erase_input_line(&line, prompt_visible_width);
                        spinner.begin_turn();
                        let skill_result = agent.run(expanded).await;
                        spinner.end_turn();
                        let skill_name_clone = name.clone();
                        match skill_result {
                            Ok(out) => {
                                aegis_core::skills::record_skill_outcome(&skill_name_clone, true);
                                let stats = aegis_core::skills::load_skill_stats(&skill_name_clone);
                                if stats.needs_improvement() {
                                    eprintln!(
                                        "\x1b[33m  ⚠ skill /{skill_name_clone} success rate {:.0}% — run /skill-improve {skill_name_clone}\x1b[0m",
                                        stats.success_rate() * 100.0
                                    );
                                }
                                #[cfg(unix)]
                                {
                                    let queued = drain_pending_lines();
                                    if !queued.is_empty() {
                                        eprintln!(
                                            "\x1b[2m[goblin] {} input{} queued from typing during run\x1b[0m",
                                            queued.len(),
                                            if queued.len() == 1 { "" } else { "s" }
                                        );
                                        pending_inputs.extend(queued);
                                    }
                                }
                                if let Ok(mut r) = md_renderer.lock() {
                                    r.finish();
                                }
                                println!();
                                total.input_tokens =
                                    total.input_tokens.saturating_add(out.usage.input_tokens);
                                total.output_tokens =
                                    total.output_tokens.saturating_add(out.usage.output_tokens);
                                total.cache_read_tokens = total
                                    .cache_read_tokens
                                    .saturating_add(out.usage.cache_read_tokens);
                                total.cache_write_tokens = total
                                    .cache_write_tokens
                                    .saturating_add(out.usage.cache_write_tokens);
                                turn_count += out.turns;
                                if cost_footer_enabled && last_cost_shown.elapsed() >= std::time::Duration::from_secs(300) {
                                    eprintln!("{}", format_cost_footer(&total, &model));
                                    last_cost_shown = std::time::Instant::now();
                                }
                            }
                            Err(e) => {
                                aegis_core::skills::record_skill_outcome(&skill_name_clone, false);
                                #[cfg(unix)]
                                {
                                    let queued = drain_pending_lines();
                                    if !queued.is_empty() {
                                        eprintln!(
                                            "\x1b[2m[goblin] {} input{} queued from typing during run\x1b[0m",
                                            queued.len(),
                                            if queued.len() == 1 { "" } else { "s" }
                                        );
                                        pending_inputs.extend(queued);
                                    }
                                }
                                eprintln!("\n[goblin] error: {e}");
                            }
                        }
                        continue;
                    }
                }
                let hint = crate::tui::suggest_slash_command(name)
                    .map(|s| format!(" (did you mean /{s}?)"))
                    .unwrap_or_default();
                eprintln!("[goblin] unknown command `/{name}`{hint} — try /help");
                continue;
            }
            ReplCommand::Clear => {
                pending_images.clear();
                *plan_state_flag.lock().unwrap() = aegis_core::PlanState::Normal;
                agent = build_agent(
                    &*client,
                    &registry,
                    workspace,
                    config.clone(),
                    Arc::clone(&permission),
                    None,
                    Some(Arc::clone(&plan_state_flag)),
                    Arc::clone(&md_renderer),
                    Arc::clone(&client),
                    Arc::clone(&registry),
                    sandbox.clone(),
                    Arc::clone(&spinner),
                    #[cfg(unix)]
                    Arc::clone(&overlay),
                    #[cfg(feature = "ctx")]
                    blob_handles.clone(),
                )?;
                total = UsageSnapshot::default();
                turn_count = 0;
                eprintln!("[goblin] conversation cleared");
                continue;
            }
            ReplCommand::ProviderSwitch(ref name) => {
                match aegis_api::Provider::lookup(name) {
                    Some(provider_info) => {
                        match provider_info.client_from_env() {
                            Ok(new_client) => {
                                let new_model = provider_info.default_model.to_string();
                                let old_model = model.clone();
                                drop(agent);
                                client = Arc::from(new_client);
                                model = new_model.clone();
                                pending_images.clear();
                                *plan_state_flag.lock().unwrap() = aegis_core::PlanState::Normal;

                                let mut new_config = config.clone();
                                new_config.model = new_model.clone();
                                if let Some(ref mut sp) = new_config.system_prompt {
                                    *sp = sp.replace(
                                        &format!("You are running as model `{}`", old_model),
                                        &format!("You are running as model `{}`", new_model),
                                    );
                                }

                                agent = build_agent(
                                    &*client,
                                    &registry,
                                    workspace,
                                    new_config.clone(),
                                    Arc::clone(&permission),
                                    None,
                                    Some(Arc::clone(&plan_state_flag)),
                                    Arc::clone(&md_renderer),
                                    Arc::clone(&client),
                                    Arc::clone(&registry),
                                    sandbox.clone(),
                                    Arc::clone(&spinner),
                                    #[cfg(unix)]
                                    Arc::clone(&overlay),
                                    #[cfg(feature = "ctx")]
                                    blob_handles.clone(),
                                )?;
                                let can_route = original_routing.is_enabled() && {
                                    let serves = |m: &Option<String>| {
                                        m.as_ref().map_or(true, |model_name| {
                                            let ml = model_name.to_ascii_lowercase();
                                            let pl = name.to_ascii_lowercase();
                                            ml.contains(&pl) || *model_name == new_model
                                        })
                                    };
                                    serves(&original_routing.fast_model)
                                        && serves(&original_routing.strong_model)
                                };
                                if can_route {
                                    routing = original_routing.clone();
                                    eprintln!("[goblin] auto-routing restored");
                                } else {
                                    routing = crate::router::RoutingConfig::default();
                                }
                                // Update loop config so next switch has correct state
                                config = new_config.clone();
                                provider = name.to_string();
                                if let Ok(mut r) = md_renderer.lock() {
                                    r.set_dedup_enabled(resolve_dedup_enabled(&provider));
                                }
                                total = UsageSnapshot::default();
                                turn_count = 0;
                                eprintln!("[goblin] switched to {name} (model: {new_model})");
                            }
                            Err(e) => {
                                eprintln!("[goblin] failed to init {name}: {e}");
                                eprintln!("  set {} first", provider_info.env_var);
                            }
                        }
                    }
                    None => {
                        let names: Vec<&str> =
                            aegis_api::Provider::BUILTINS.iter().map(|p| p.id).collect();
                        eprintln!(
                            "[goblin] unknown provider `{name}` — available: {}",
                            names.join(", ")
                        );
                    }
                }
                continue;
            }
            ReplCommand::SetKey {
                ref env_var,
                ref value,
            } => {
                std::env::set_var(env_var, value);
                eprintln!("[goblin] {} set (session only)", env_var);
                eprintln!("  to persist, add to ~/.metis/config.toml:");
                eprintln!("  [api_keys]");
                eprintln!("  {} = \"{}...\"", env_var, &value[..value.len().min(8)]);
                continue;
            }
            ReplCommand::ModelMenu => {
                let models: Vec<(&str, &str)> = match provider.as_str() {
                    "anthropic" => vec![
                        ("claude-sonnet-4-6", "Sonnet 4.6"),
                        ("claude-opus-4-6", "Opus 4.6"),
                        ("claude-haiku-4-5-20251001", "Haiku"),
                    ],
                    "deepseek" => vec![
                        ("deepseek-v4-flash", "V4 Flash"),
                        ("deepseek-v4-pro", "V4 Pro"),
                        ("deepseek-reasoner", "R1 (V4 thinking)"),
                        ("deepseek-chat", "V3 Chat [legacy]"),
                    ],
                    "gemini" => vec![
                        ("gemini-2.5-flash", "2.5 Flash"),
                        ("gemini-2.5-pro", "2.5 Pro"),
                        ("gemini-3.1-flash-lite-preview", "3.1 Flash Lite"),
                        ("gemini-3.1-pro-preview", "3.1 Pro"),
                    ],
                    "glm" => vec![("glm-5.1", "5.1"), ("glm-5-turbo", "Turbo")],
                    "openrouter" => vec![
                        ("deepseek/deepseek-chat", "DeepSeek Chat"),
                        ("deepseek/deepseek-r1", "DeepSeek R1"),
                        ("anthropic/claude-sonnet-4-6", "Sonnet 4.6"),
                        ("anthropic/claude-opus-4-6", "Opus 4.6"),
                        ("google/gemini-2.5-pro", "Gemini Pro"),
                        ("glm/glm-5.1", "GLM 5.1"),
                    ],
                    "openai" => vec![
                        ("gpt-4o", "GPT-4o"),
                        ("gpt-4o-mini", "GPT-4o Mini"),
                        ("gpt-4.1", "GPT-4.1"),
                        ("gpt-5.3-chat-latest", "GPT-5.3 Chat"),
                        ("gpt-5.3-codex", "GPT-5.3 Codex"),
                        ("o1", "O1 Reasoning"),
                        ("o1-mini", "O1 Mini"),
                        ("o3", "O3 Reasoning"),
                        ("o3-mini", "O3 Mini"),
                    ],
                    "minimax" => vec![
                        ("MiniMax-M2.7", "M2.7"),
                        ("MiniMax-M2.5", "M2.5"),
                        ("MiniMax-Text-01", "Text-01"),
                    ],
                    _ => vec![],
                };

                if models.is_empty() {
                    eprintln!("[goblin] no models available for this provider");
                    continue;
                }

                eprintln!("[goblin] {} models:", provider);
                for (i, (_, name)) in models.iter().enumerate() {
                    eprintln!("  {}. {}", i + 1, name);
                }

                // Read single digit without waiting for enter
                let choice_opt = {
                    #[cfg(unix)]
                    {
                        use std::io::Read;
                        let mut t: libc::termios = unsafe { std::mem::zeroed() };
                        unsafe {
                            libc::tcgetattr(libc::STDIN_FILENO, &mut t);
                        }
                        let saved_t = t;
                        t.c_lflag &= !(libc::ECHO | libc::ICANON);
                        t.c_cc[libc::VMIN] = 1;
                        t.c_cc[libc::VTIME] = 0;
                        unsafe {
                            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
                        }

                        let mut buf = [0u8; 1];
                        let result = std::io::stdin().read_exact(&mut buf);
                        unsafe {
                            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved_t);
                        }

                        if result.is_ok() {
                            (buf[0] as char).to_digit(10).map(|d| d as usize)
                        } else {
                            None
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let mut input = String::new();
                        std::io::stdin().read_line(&mut input).ok()?;
                        input.trim().parse::<usize>().ok()
                    }
                };

                if let Some(choice) = choice_opt {
                    if choice > 0 && choice <= models.len() {
                        let selected_model = models[choice - 1].0.to_string();
                        let old = model.clone();
                        model = selected_model.clone();

                        let mut new_config = config.clone();
                        new_config.model = selected_model.clone();
                        if let Some(ref mut sp) = new_config.system_prompt {
                            *sp = sp.replace(
                                &format!("You are running as model `{}`", old),
                                &format!("You are running as model `{}`", selected_model),
                            );
                        }

                        drop(agent);
                        agent = build_agent(
                            &*client,
                            &registry,
                            workspace,
                            new_config.clone(),
                            Arc::clone(&permission),
                            None,
                            Some(Arc::clone(&plan_state_flag)),
                            Arc::clone(&md_renderer),
                            Arc::clone(&client),
                            Arc::clone(&registry),
                            sandbox.clone(),
                            Arc::clone(&spinner),
                            #[cfg(unix)]
                            Arc::clone(&overlay),
                            #[cfg(feature = "ctx")]
                            blob_handles.clone(),
                        )?;
                        routing = crate::router::RoutingConfig::default();
                        config = new_config;
                        eprintln!("[goblin] model: {old} → {selected_model}");
                    }
                }
                continue;
            }
            ReplCommand::ModelSwitch(ref new_model) => {
                let old = agent.model().to_string();

                // Support "provider:model" format — switch provider too if specified.
                let (new_provider, bare_model) = if new_model.contains(':') {
                    let mut iter = new_model.splitn(2, ':');
                    let p = iter.next().unwrap().to_string();
                    let m = iter.next().unwrap().to_string();
                    (Some(p), m)
                } else {
                    (None, new_model.clone())
                };

                model = bare_model.clone();

                // Update system prompt with new model name
                let mut new_config = config.clone();
                new_config.model = bare_model.clone();
                if let Some(ref mut sp) = new_config.system_prompt {
                    *sp = sp.replace(
                        &format!("You are running as model `{}`", old),
                        &format!("You are running as model `{}`", bare_model),
                    );
                }

                // Switch provider if specified (e.g. "/model anthropic:claude-opus-4-5")
                if let Some(ref pname) = new_provider {
                    match aegis_api::Provider::lookup(pname) {
                        Some(provider) => match provider.client_from_env() {
                            Ok(new_client) => {
                                drop(agent);
                                client = Arc::from(new_client);
                                agent = build_agent(
                                    &*client,
                                    &registry,
                                    workspace,
                                    new_config.clone(),
                                    Arc::clone(&permission),
                                    None,
                                    Some(Arc::clone(&plan_state_flag)),
                                    Arc::clone(&md_renderer),
                                    Arc::clone(&client),
                                    Arc::clone(&registry),
                                    sandbox.clone(),
                                    Arc::clone(&spinner),
                                    #[cfg(unix)]
                                    Arc::clone(&overlay),
                                    #[cfg(feature = "ctx")]
                                    blob_handles.clone(),
                                )?;
                                routing = crate::router::RoutingConfig::default();
                                config = new_config.clone();
                                let _ = agent.append_note(&format!(
                                    "Model switched from `{old}` to `{pname}:{bare_model}`."
                                ));
                                eprintln!("[goblin] model: {old} → {pname}:{bare_model}");
                                continue;
                            }
                            Err(e) => {
                                eprintln!("[goblin] can't switch to provider `{pname}`: {e}");
                                continue;
                            }
                        },
                        None => {
                            eprintln!("[goblin] unknown provider `{pname}`");
                            continue;
                        }
                    }
                }

                // No provider switch — just rebuild agent with new model.
                drop(agent);
                agent = build_agent(
                    &*client,
                    &registry,
                    workspace,
                    new_config.clone(),
                    Arc::clone(&permission),
                    None,
                    Some(Arc::clone(&plan_state_flag)),
                    Arc::clone(&md_renderer),
                    Arc::clone(&client),
                    Arc::clone(&registry),
                    sandbox.clone(),
                    Arc::clone(&spinner),
                    #[cfg(unix)]
                    Arc::clone(&overlay),
                    #[cfg(feature = "ctx")]
                    blob_handles.clone(),
                )?;

                // Disable auto-routing when user manually selects a model
                routing = crate::router::RoutingConfig::default();

                // Update loop config so next model switch has correct system prompt
                config = new_config.clone();

                let _ =
                    agent.append_note(&format!("Model switched from `{old}` to `{bare_model}`."));
                eprintln!("[goblin] model: {old} → {bare_model}");
                continue;
            }
            ReplCommand::Swarm {
                ref prompt,
                n,
                quorum,
            } => {
                let quorum_label = if quorum > 0 {
                    format!(", quorum {quorum}/{n}")
                } else {
                    String::new()
                };
                eprintln!("[goblin] swarm: {n} parallel agents{quorum_label}");

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

                // Inject quorum instructions into each agent's prompt when quorum > 0.
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
                            "prompt": format!("{prompt}{quorum_instruction}")
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

                match agent.run(tool_call).await {
                    Ok(output) => {
                        total.input_tokens += output.usage.input_tokens;
                        total.output_tokens += output.usage.output_tokens;
                        total.cache_read_tokens += output.usage.cache_read_tokens;
                        total.cache_write_tokens += output.usage.cache_write_tokens;
                        turn_count += output.turns;
                        eprintln!("[goblin] swarm complete");
                    }
                    Err(e) => {
                        eprintln!("[goblin] swarm failed: {e}");
                    }
                }
                continue;
            }
            ReplCommand::Consult {
                ref provider,
                ref prompt,
            } => {
                // One-shot query to a secondary model (e.g. /glm <question>).
                // Streams the response to stderr, then injects a [consult]
                // system note into the main agent context so the main model
                // can use the information.
                let prov_info = aegis_api::Provider::lookup(provider.as_str());
                match prov_info {
                    Some(prov_info) => match prov_info.client_from_env() {
                        Ok(consult_client) => {
                            eprintln!("\x1b[38;2;220;50;50m[{}]\x1b[0m consulting...", provider);
                            let consult_req = aegis_api::ChatRequest {
                                model: prov_info.default_model.to_string(),
                                messages: vec![aegis_api::ChatMessage::user(prompt.clone())],
                                tools: None,
                                temperature: Some(0.7),
                                max_tokens: Some(2048),
                                thinking: false,
                                thinking_budget: 0,
                            };
                            let mut response_text = String::new();
                            eprint!("\x1b[38;2;220;50;50m[{}]\x1b[0m ", provider);
                            let result = consult_client
                                .chat_stream(&consult_req, &mut |event| {
                                    if let aegis_api::StreamEvent::TextDelta(t) = event {
                                        eprint!("{}", t);
                                        response_text.push_str(&t);
                                    }
                                })
                                .await;
                            eprintln!();
                            match result {
                                Ok(_) => {
                                    // Inject response into main agent context as a note
                                    let note = format!(
                                        "[consult/{provider}] {prompt}\n\nResponse:\n{response_text}"
                                    );
                                    let _ = agent.append_note(&note);
                                }
                                Err(e) => {
                                    eprintln!("\x1b[37m[{}] error: {}\x1b[0m", provider, e);
                                    eprintln!("  hint: check {} is set", prov_info.env_var);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[goblin] /glm: can't init {provider}: {e}");
                            eprintln!("  hint: set {} in config or env", prov_info.env_var);
                        }
                    },
                    None => {
                        eprintln!("[goblin] /glm: unknown provider '{provider}'");
                        eprintln!(
                            "  available: {}",
                            aegis_api::Provider::BUILTINS
                                .iter()
                                .map(|p| p.id)
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
                continue;
            }
            ReplCommand::Race(ref prompt) => {
                // Query 3 strong models in parallel, show all responses,
                // inject the longest (best) into the main agent context.
                const RACE_PROVIDERS: &[&str] = &[
                    "anthropic",
                    "gemini",
                    "deepseek",
                    "openai",
                    "minimax",
                    "glm",
                ];
                let req_template = aegis_api::ChatRequest {
                    model: String::new(), // filled per-provider below
                    messages: vec![aegis_api::ChatMessage::user(prompt.clone())],
                    tools: None,
                    temperature: Some(0.7),
                    max_tokens: Some(2048),
                    thinking: false,
                    thinking_budget: 0,
                };
                let futures: Vec<_> = RACE_PROVIDERS
                    .iter()
                    .filter_map(|id| {
                        let prov = aegis_api::Provider::lookup(id)?;
                        let client = prov.client_from_env().ok()?;
                        let mut req = req_template.clone();
                        req.model = prov.default_model.to_string();
                        Some((*id, client, req))
                    })
                    .collect();
                if futures.is_empty() {
                    eprintln!("[race] no providers available — set ANTHROPIC_API_KEY, GEMINI_API_KEY, or DEEPSEEK_API_KEY");
                    continue;
                }
                eprintln!("[race] querying {} models in parallel...", futures.len());
                let handles: Vec<_> = futures
                    .into_iter()
                    .map(|(id, client, req)| {
                        tokio::spawn(async move {
                            let result = client.chat(&req).await;
                            (id, result)
                        })
                    })
                    .collect();
                let mut results: Vec<(&str, String)> = Vec::new();
                for handle in handles {
                    match handle.await {
                        Ok((id, Ok(resp))) => {
                            let text = resp
                                .choices
                                .first()
                                .and_then(|c| c.message.content.clone())
                                .unwrap_or_default();
                            eprintln!("\n\x1b[1m[{}]\x1b[0m\n{}", id, text);
                            results.push((id, text));
                        }
                        Ok((id, Err(e))) => {
                            eprintln!("[race] {id} error: {e}");
                        }
                        Err(e) => {
                            eprintln!("[race] task error: {e}");
                        }
                    }
                }
                // Score all responses with a fast judge model.
                let best = if results.len() > 1 {
                    // Build a numbered list of responses for the judge.
                    let candidates = results
                        .iter()
                        .enumerate()
                        .map(|(i, (id, text))| format!("[{}] ({}):\n{}", i + 1, id, text))
                        .collect::<Vec<_>>()
                        .join("\n\n---\n\n");
                    let query_ctx = aegis_api::autotune::autotune(prompt);
                    let criteria = match aegis_api::autotune::autotune(prompt).temperature {
                        t if t <= 0.2 => "correctness (does it compile/run?), handles edge cases, efficiency, idiomatic style",
                        t if t <= 0.3 => "accuracy of reasoning, completeness, clear explanation of cause and effect",
                        t if t <= 0.15 => "factual accuracy, specificity, cites concrete details",
                        t if t >= 0.8 => "originality, coherence, engaging tone, creative depth",
                        _ => "overall helpfulness, clarity, and relevance to the question",
                    };
                    let _ = query_ctx; // used via temperature match above
                    let judge_prompt = format!(
                        "You are a judge evaluating AI responses. Given the following question and candidate responses, \
                        reply with ONLY the number (1, 2, 3, ...) of the best response. \
                        Evaluation criteria: {criteria}. No explanation, just the number.\n\n\
                        QUESTION: {prompt}\n\nCANDIDATES:\n{candidates}"
                    );
                    // Use whichever fast judge is available: gemini > deepseek > first available.
                    let judge_provider =
                        ["gemini", "deepseek", "anthropic"].iter().find_map(|id| {
                            let prov = aegis_api::Provider::lookup(id)?;
                            let client = prov.client_from_env().ok()?;
                            Some((prov, client))
                        });
                    match judge_provider {
                        Some((prov, judge_client)) => {
                            eprintln!("[race] scoring with {}...", prov.id);
                            let judge_req = aegis_api::ChatRequest {
                                model: prov.default_model.to_string(),
                                messages: vec![aegis_api::ChatMessage::user(judge_prompt)],
                                tools: None,
                                temperature: Some(0.0),
                                max_tokens: Some(8),
                                thinking: false,
                                thinking_budget: 0,
                            };
                            match judge_client.chat(&judge_req).await {
                                Ok(resp) => {
                                    let verdict = resp
                                        .choices
                                        .first()
                                        .and_then(|c| c.message.content.as_deref())
                                        .unwrap_or("1")
                                        .trim()
                                        .to_string();
                                    let idx = verdict
                                        .chars()
                                        .find(|c| c.is_ascii_digit())
                                        .and_then(|c| c.to_digit(10))
                                        .map(|n| (n as usize).saturating_sub(1))
                                        .unwrap_or(0);
                                    results.get(idx).or_else(|| results.first())
                                }
                                Err(_) => results.iter().max_by_key(|(_, t)| t.len()),
                            }
                        }
                        None => results.iter().max_by_key(|(_, t)| t.len()),
                    }
                } else {
                    results.first()
                };
                if let Some((best_id, best_text)) = best {
                    let note = format!(
                        "[race] prompt: {prompt}\n\nbest response (from {best_id}):\n{best_text}"
                    );
                    let _ = agent.append_note(&note);
                    eprintln!("\n[race] injected best response from {best_id} into context");
                }
                continue;
            }
            ReplCommand::Fork { name, take } => {
                // Fork the current transcript into a new session on
                // disk, then rebuild the agent around it. The parent
                // session file is left untouched — `metis --resume
                // <parent-id>` can still pick it back up from exactly
                // where we branched.
                let parent = match agent.session() {
                    Some(s) => s,
                    None => {
                        eprintln!("[goblin] nothing to fork — no session is attached");
                        continue;
                    }
                };
                let new_id = name.unwrap_or_else(SessionStore::new_id);
                let forked = match parent.fork(&new_id, take) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[goblin] fork failed: {e}");
                        continue;
                    }
                };
                let kept = forked.messages().len();
                agent = build_agent(
                    &*client,
                    &registry,
                    workspace,
                    config.clone(),
                    Arc::clone(&permission),
                    Some(forked),
                    Some(Arc::clone(&plan_state_flag)),
                    Arc::clone(&md_renderer),
                    Arc::clone(&client),
                    Arc::clone(&registry),
                    sandbox.clone(),
                    Arc::clone(&spinner),
                    #[cfg(unix)]
                    Arc::clone(&overlay),
                    #[cfg(feature = "ctx")]
                    blob_handles.clone(),
                )?;
                eprintln!("[goblin] forked → session={new_id} ({kept} messages carried over)");
                continue;
            }
            ReplCommand::Resume(id) => {
                // Open (or create) the requested session file and
                // rebuild the agent around it. The agent's `run` path
                // already replays existing messages as the seed
                // transcript, so there is nothing to do here beyond
                // swapping the SessionStore in. Cumulative cost
                // survives because it is a REPL-process property.
                let store = match SessionStore::open(workspace, &id) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[goblin] could not open session `{id}`: {e}");
                        continue;
                    }
                };
                let carried = store.messages().len();
                agent = build_agent(
                    &*client,
                    &registry,
                    workspace,
                    config.clone(),
                    Arc::clone(&permission),
                    Some(store),
                    Some(Arc::clone(&plan_state_flag)),
                    Arc::clone(&md_renderer),
                    Arc::clone(&client),
                    Arc::clone(&registry),
                    sandbox.clone(),
                    Arc::clone(&spinner),
                    #[cfg(unix)]
                    Arc::clone(&overlay),
                    #[cfg(feature = "ctx")]
                    blob_handles.clone(),
                )?;
                eprintln!("[goblin] resumed session={id} ({carried} messages)");
                continue;
            }
            ReplCommand::Sessions => {
                match SessionStore::list(workspace) {
                    Ok(list) if list.is_empty() => {
                        eprintln!("[goblin] no sessions under {}", workspace.display());
                    }
                    Ok(list) => {
                        eprintln!("[goblin] sessions ({} total, newest first):", list.len());
                        for s in list {
                            eprintln!("  {}  ({} msgs)", s.id, s.message_count);
                        }
                    }
                    Err(e) => eprintln!("[goblin] could not list sessions: {e}"),
                }
                continue;
            }
            ReplCommand::Tree => {
                match SessionStore::list(workspace) {
                    Ok(list) if list.is_empty() => {
                        eprintln!("[goblin] no sessions");
                    }
                    Ok(list) => {
                        let current_id = agent.session().map(|s| s.id().to_string());
                        eprintln!("{}", format_session_tree(&list, current_id.as_deref()));
                    }
                    Err(e) => eprintln!("[goblin] could not list sessions: {e}"),
                }
                continue;
            }
            ReplCommand::SessionInfo => {
                match agent.session() {
                    Some(s) => {
                        let meta = s.meta();
                        eprintln!("[goblin] session: {}", s.id());
                        eprintln!("  messages: {}", s.messages().len());
                        if let Some(pid) = &meta.parent_id {
                            eprintln!("  parent: {pid}");
                        }
                        if let Some(fp) = meta.fork_point {
                            eprintln!("  fork point: message {fp}");
                        }
                        // List children
                        if let Ok(all) = SessionStore::list(workspace) {
                            let children: Vec<_> = all
                                .iter()
                                .filter(|x| x.parent_id.as_deref() == Some(s.id()))
                                .collect();
                            if !children.is_empty() {
                                eprintln!("  children:");
                                for c in &children {
                                    eprintln!("    └─ {} ({} msgs)", c.id, c.message_count);
                                }
                            }
                        }
                    }
                    None => eprintln!("[goblin] no session attached"),
                }
                continue;
            }
            ReplCommand::Stats => {
                if let Some(path) = aegis_core::telemetry::telemetry_path() {
                    let records = aegis_core::telemetry::load_records(&path);
                    if records.is_empty() {
                        eprintln!("[goblin] no telemetry data yet");
                    } else {
                        let stats = aegis_core::telemetry::UsageStats::from_records(&records);
                        eprint!("{}", stats.format_dashboard());
                    }
                } else {
                    eprintln!("[goblin] could not determine telemetry path");
                }
                continue;
            }
            ReplCommand::Update => {
                use aegis_core::update;
                eprintln!("[goblin] current: v{}", update::CURRENT_VERSION);
                eprintln!("[goblin] checking...");
                let http = reqwest::Client::new();
                match update::check_latest(&http).await {
                    Ok(check) if !check.is_newer => {
                        eprintln!("[goblin] already up to date");
                    }
                    Ok(check) => {
                        eprintln!("[goblin] v{} → v{} available", check.current, check.latest);
                        if check.download_url.is_some() {
                            eprintln!("[goblin] downloading...");
                            match update::perform_update(&http, &check).await {
                                Ok(path) => {
                                    eprintln!(
                                        "[goblin] updated to v{} — restart to use new version",
                                        check.latest
                                    );
                                    let _ = path;
                                }
                                Err(e) => eprintln!("[goblin] update failed: {e}"),
                            }
                        } else {
                            eprintln!(
                                "[goblin] no binary for {} — build from source",
                                update::current_target()
                            );
                        }
                    }
                    Err(e) => eprintln!("[goblin] update check failed: {e}"),
                }
                continue;
            }
            ReplCommand::Overthink => {
                thinking_enabled = !thinking_enabled;
                let mut new_config = config.clone();
                new_config.thinking = thinking_enabled;
                // Carry over the current session so history is preserved.
                let session = agent.take_session();
                agent = build_agent(
                    &*client,
                    &registry,
                    workspace,
                    new_config,
                    Arc::clone(&permission),
                    session,
                    Some(Arc::clone(&plan_state_flag)),
                    Arc::clone(&md_renderer),
                    Arc::clone(&client),
                    Arc::clone(&registry),
                    sandbox.clone(),
                    Arc::clone(&spinner),
                    #[cfg(unix)]
                    Arc::clone(&overlay),
                    #[cfg(feature = "ctx")]
                    blob_handles.clone(),
                )?;
                let state = if thinking_enabled { "ON" } else { "OFF" };
                eprintln!(
                    "[goblin] overthink {state} (budget={})",
                    config.thinking_budget
                );
                continue;
            }
            ReplCommand::Dag => {
                let messages = agent.session_messages();
                let dag = format_dag(&messages);
                eprint!("{dag}");
                continue;
            }
            ReplCommand::Map(max) => {
                let max_files = max.unwrap_or(200);
                let map = aegis_core::repomap::build_repo_map(workspace, max_files);
                if map.is_empty() {
                    eprintln!(
                        "[goblin] /map: no source files found in {}",
                        workspace.display()
                    );
                } else {
                    eprint!("{map}");
                }
                continue;
            }
            ReplCommand::Budget => {
                let pricing = aegis_core::ModelPricing::resolve(&model);
                let status = BudgetStatus {
                    prior_usd: aegis_core::spent_today(),
                    session_usd: pricing.estimate(&total).total_usd(),
                    budget_usd: daily_budget_usd,
                };
                let marker = if status.over_budget() { "!" } else { "·" };
                eprintln!("[goblin] {marker} {}", status.summary());
                if daily_budget_usd.is_none() {
                    eprintln!(
                        "\x1b[2m[goblin]   (set daily_budget_usd in .metis/config.toml to enable the ceiling)\x1b[0m"
                    );
                }
                continue;
            }
            ReplCommand::Compact => {
                let removed = agent.force_compact();
                if removed > 0 {
                    eprintln!("[goblin] compacted: {removed} messages removed");
                } else {
                    eprintln!("[goblin] nothing to compact (transcript too short)");
                }
                continue;
            }
            ReplCommand::Insights => {
                eprintln!("[goblin] extracting insights from session...");
                let messages = agent.session_messages();
                if messages.is_empty() {
                    eprintln!("[goblin] no session messages to extract insights from");
                    continue;
                }
                // Build a flat text of conversation for the LLM
                let mut conv = String::new();
                for m in &messages {
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
                        // Truncate large tool outputs to keep context manageable
                        let trimmed = if c.len() > 500 { &c[..500] } else { c };
                        conv.push_str(&format!("{role}: {trimmed}\n"));
                    }
                }
                if conv.is_empty() {
                    eprintln!("[goblin] no extractable content in session");
                    continue;
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
                // Add a history entry for the insights command
                let insights_line = format!(
                    "[/insights]: {}",
                    insight_prompt.chars().take(40).collect::<String>()
                );
                editor.add_history_entry(&insights_line).ok();
                // Fall through to Prompt handler by re-running with the insight prompt
                let run_result_insights = {
                    spinner.begin_turn();
                    let r = tokio::select! {
                        result = agent.run(aegis_core::UserInput::Text(insight_prompt)) => Some(result),
                        _ = tokio::signal::ctrl_c() => {
                                    None
                        }
                    };
                    spinner.end_turn();
                    r
                };
                if let Some(Ok(out)) = run_result_insights {
                    total.input_tokens = total.input_tokens.saturating_add(out.usage.input_tokens);
                    total.output_tokens =
                        total.output_tokens.saturating_add(out.usage.output_tokens);
                    total.cache_read_tokens = total
                        .cache_read_tokens
                        .saturating_add(out.usage.cache_read_tokens);
                    total.cache_write_tokens = total
                        .cache_write_tokens
                        .saturating_add(out.usage.cache_write_tokens);
                    turn_count += out.turns;
                    if last_cost_shown.elapsed() >= std::time::Duration::from_secs(300) {
                        eprintln!("{}", format_cost_footer(&total, &model));
                        last_cost_shown = std::time::Instant::now();
                    }
                } else if let Some(Err(e)) = run_result_insights {
                    eprintln!("[goblin] insights error: {e}");
                }
                continue;
            }
            ReplCommand::Learn(ref text) => {
                if text.trim().is_empty() {
                    eprintln!("[goblin] usage: /learn <rule or insight text>");
                    continue;
                }
                let workspace = std::env::current_dir().unwrap_or_default();
                let ws_str = workspace.display().to_string();
                let insight = aegis_core::learning::Insight {
                    timestamp: aegis_core::telemetry::now_iso8601(),
                    workspace: Some(ws_str),
                    category: "preference".to_string(),
                    text: text.clone(),
                    reinforcements: 1,
                    last_seen: None,
                    success_count: 0,
                    failure_count: 0,
                    tags: vec!["manual".to_string()],
                };
                match aegis_core::learning::upsert_insight(&insight) {
                    Ok(()) => eprintln!("[goblin] saved: {text}"),
                    Err(e) => eprintln!("[goblin] failed to save insight: {e}"),
                }
                continue;
            }
            ReplCommand::ExportFt(ref out_path) => {
                let path = out_path
                    .clone()
                    .unwrap_or_else(|| "ft_export.jsonl".to_string());
                let messages = agent.session_messages();
                let mut examples: Vec<serde_json::Value> = Vec::new();
                let system_content = messages
                    .iter()
                    .find(|m| m.role == aegis_api::Role::System)
                    .and_then(|m| m.content.clone())
                    .unwrap_or_default();
                // Pair up user→assistant turns into training examples
                let mut i = 0;
                while i < messages.len() {
                    if messages[i].role == aegis_api::Role::User {
                        // Find the next assistant text response
                        if let Some(j) = messages[i + 1..]
                            .iter()
                            .position(|m| {
                                m.role == aegis_api::Role::Assistant
                                    && m.tool_calls.is_empty()
                                    && m.content.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
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
                                    "role": "user",
                                    "content": user_text
                                }));
                                msgs.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": asst_text
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
                match std::fs::write(
                    &path,
                    examples
                        .iter()
                        .map(|e| serde_json::to_string(e).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ) {
                    Ok(_) => eprintln!(
                        "[goblin] exported {} training examples → {path}",
                        examples.len()
                    ),
                    Err(e) => eprintln!("[goblin] export-ft error: {e}"),
                }
                continue;
            }
            ReplCommand::Advisor => {
                // Block mutating tools via PlanState::Drafting (same mechanism as /plan).
                *plan_state_flag.lock().unwrap() = aegis_core::PlanState::Drafting;
                let _ = agent.append_note(
                    "[ADVISOR MODE ACTIVE] You are a senior software architect in read-only \
                     analysis mode. Rules: (1) Ask clarifying questions before suggesting \
                     solutions. (2) Do NOT execute any tools or modify files — read-only \
                     tools only. (3) Focus on design tradeoffs, bottlenecks, coupling, \
                     missing tests. (4) Be concise and direct. \
                     Type /advisor off to exit.",
                );
                eprintln!("\x1b[33m[advisor]\x1b[0m mode ON — mutating tools blocked (read-only). Type \x1b[1m/advisor off\x1b[0m to exit.");
                continue;
            }
            ReplCommand::AutotuneToggle => {
                config.autotune = !config.autotune;
                if config.autotune {
                    eprintln!("[autotune] ON — adaptive temperature enabled");
                } else {
                    eprintln!("[autotune] OFF — using fixed temperature");
                }
                continue;
            }
            ReplCommand::Security(ref sub) => {
                match sub.as_deref() {
                    Some("kill") => {
                        eprintln!("\x1b[37m[security] kill switch triggered — all autonomous tool calls blocked\x1b[0m");
                        eprintln!("  use /security resume to re-enable");
                    }
                    Some("resume") => {
                        eprintln!(
                            "\x1b[32m[security] resumed — autonomous operations re-enabled\x1b[0m"
                        );
                    }
                    _ => {
                        let autotune_status = if config.autotune { "ON" } else { "OFF" };
                        eprintln!("── Autonomous Security Status ──────────────────");
                        eprintln!("  autotune:          {autotune_status}");
                        eprintln!("  kill switch:       use /security kill to trigger");
                        eprintln!("  max tool calls:    100");
                        eprintln!("  max cost:          $10.00");
                        eprintln!("  timeout:           5 min");
                        eprintln!("  protected paths:   .git, Cargo.toml, package.json");
                        eprintln!("────────────────────────────────────────────────");
                    }
                }
                continue;
            }
            ReplCommand::AdvisorOff => {
                *plan_state_flag.lock().unwrap() = aegis_core::PlanState::Normal;
                let _ = agent.append_note(
                    "[ADVISOR MODE DEACTIVATED] Resume normal operation with full tool access.",
                );
                eprintln!("\x1b[2m[advisor]\x1b[0m mode OFF — all tools restored.");
                continue;
            }
            ReplCommand::Plan => {
                let mut state = plan_state_flag.lock().unwrap();
                match *state {
                    aegis_core::PlanState::Normal => {
                        *state = aegis_core::PlanState::Drafting;
                        eprintln!("[goblin] plan mode ON (drafting — read-only tools only)");
                    }
                    aegis_core::PlanState::Drafting => {
                        *state = aegis_core::PlanState::Normal;
                        eprintln!("[goblin] plan mode OFF");
                    }
                    aegis_core::PlanState::Executing => {
                        *state = aegis_core::PlanState::Normal;
                        eprintln!("[goblin] plan execution cancelled, back to normal");
                    }
                }
                continue;
            }
            ReplCommand::Skills => {
                let invocable = skill_registry.user_invocable();
                if invocable.is_empty() {
                    eprintln!(
                        "\x1b[33m●\x1b[0m \x1b[1mskills\x1b[0m \x1b[2m— none installed\x1b[0m"
                    );
                    eprintln!(
                        "  \x1b[2muse \x1b[0m\x1b[33m/skill-install <path>\x1b[0m\x1b[2m to add one\x1b[0m"
                    );
                } else {
                    let count = invocable.len();
                    // Compute longest name for column alignment so the
                    // descriptions line up cleanly even with long names
                    // like /exit-worktree.
                    let widest = invocable
                        .iter()
                        .map(|s| s.name.chars().count())
                        .max()
                        .unwrap_or(0)
                        .max(8);
                    eprintln!(
                        "\x1b[33m●\x1b[0m \x1b[1;33mskills\x1b[0m \x1b[2m({count} available — type a slash command to invoke)\x1b[0m"
                    );
                    eprintln!("\x1b[2m  ───────────────────────────────────────────────\x1b[0m");
                    for s in &invocable {
                        let stats = aegis_core::skills::load_skill_stats(&s.name);
                        let rate_tag = if stats.use_count >= 3 {
                            let pct = stats.success_rate() * 100.0;
                            if stats.needs_improvement() {
                                format!(" \x1b[37m({pct:.0}% ok, needs /skill-improve)\x1b[0m")
                            } else {
                                format!(" \x1b[2m({pct:.0}% ok)\x1b[0m")
                            }
                        } else {
                            String::new()
                        };
                        eprintln!(
                            "  \x1b[93m/{name:<width$}\x1b[0m \x1b[2m→\x1b[0m {desc}{rate_tag}",
                            name = s.name,
                            width = widest,
                            desc = s.description
                        );
                    }
                    eprintln!("\x1b[2m  ───────────────────────────────────────────────\x1b[0m");
                    eprintln!(
                        "  \x1b[2mtip: \x1b[0m\x1b[33m/skill-search <query>\x1b[0m\x1b[2m to find more,\x1b[0m \x1b[33m/skill-install <path>\x1b[0m\x1b[2m to add\x1b[0m"
                    );
                }
                continue;
            }
            ReplCommand::SkillInstall(source) => {
                match skill_registry.install(&source) {
                    Ok(names) => {
                        eprintln!(
                            "[goblin] installed {} skill(s): {}",
                            names.len(),
                            names.join(", ")
                        );
                    }
                    Err(e) => {
                        eprintln!("[goblin] skill install failed: {e}");
                    }
                }
                continue;
            }
            ReplCommand::SkillUninstall(name) => {
                match skill_registry.uninstall(&name) {
                    Ok(()) => eprintln!("[goblin] uninstalled skill `{name}`"),
                    Err(e) => eprintln!("[goblin] {e}"),
                }
                continue;
            }
            ReplCommand::SkillSearch(query) => {
                let results = skill_registry.search(&query);
                if results.is_empty() {
                    eprintln!(
                        "\x1b[33m●\x1b[0m \x1b[1;33mskill-search\x1b[0m \x1b[2m— no matches for\x1b[0m \x1b[33m{query}\x1b[0m"
                    );
                } else {
                    let widest = results
                        .iter()
                        .map(|s| s.name.chars().count())
                        .max()
                        .unwrap_or(0)
                        .max(8);
                    eprintln!(
                        "\x1b[33m●\x1b[0m \x1b[1;33mskill-search\x1b[0m \x1b[2m({} match{} for\x1b[0m \x1b[33m{query}\x1b[0m\x1b[2m)\x1b[0m",
                        results.len(),
                        if results.len() == 1 { "" } else { "es" }
                    );
                    eprintln!("\x1b[2m  ───────────────────────────────────────────────\x1b[0m");
                    for s in &results {
                        let tags = if s.tags.is_empty() {
                            String::new()
                        } else {
                            format!(" \x1b[2m[{}]\x1b[0m", s.tags.join(", "))
                        };
                        let ver = s.version.as_deref().unwrap_or("");
                        let author = s.author.as_deref().unwrap_or("");
                        let meta = if !ver.is_empty() || !author.is_empty() {
                            format!(" \x1b[2m({author} {ver})\x1b[0m")
                        } else {
                            String::new()
                        };
                        eprintln!(
                            "  \x1b[93m/{name:<width$}\x1b[0m \x1b[2m→\x1b[0m {desc}{meta}{tags}",
                            name = s.name,
                            width = widest,
                            desc = s.description
                        );
                    }
                    eprintln!("\x1b[2m  ───────────────────────────────────────────────\x1b[0m");
                }
                continue;
            }
            ReplCommand::SkillRate { name, good } => {
                aegis_core::skills::record_skill_outcome(&name, good);
                let label = if good {
                    "\x1b[32mgood\x1b[0m"
                } else {
                    "\x1b[37mbad\x1b[0m"
                };
                eprintln!("[goblin] skill \x1b[93m{name}\x1b[0m rated {label}");
                let stats = aegis_core::skills::load_skill_stats(&name);
                if stats.needs_improvement() {
                    eprintln!(
                        "\x1b[33m  ⚠ success rate {:.0}% over {} uses — run \x1b[0m\x1b[93m/skill-improve {name}\x1b[0m\x1b[33m to fix\x1b[0m",
                        stats.success_rate() * 100.0,
                        stats.use_count
                    );
                }
                continue;
            }
            ReplCommand::LearnSkill(suggested_name) => {
                let messages = agent.session_messages();
                if messages.is_empty() {
                    eprintln!("[goblin] no session content to extract a skill from");
                    continue;
                }
                let mut conv = String::new();
                for m in &messages {
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
                        let trimmed = if c.len() > 400 { &c[..400] } else { c };
                        conv.push_str(&format!("{role}: {trimmed}\n"));
                    }
                }
                let name_hint = suggested_name
                    .as_deref()
                    .map(|n| format!("Name it `{n}`."))
                    .unwrap_or_default();
                let extract_prompt = format!(
                    "Analyze this conversation and determine if it contains a repeatable, \
                     reusable pattern worth capturing as a skill (a prompt template). \
                     If yes, produce a skill file in this exact format:\n\n\
                     ```\n\
                     ---\n\
                     name: <slug>\n\
                     description: <one line>\n\
                     user_invocable: true\n\
                     tags: <comma,separated>\n\
                     ---\n\
                     <prompt body — use $ARGS where user arguments go>\n\
                     ```\n\n\
                     {name_hint}\n\
                     If there is no reusable pattern, say so in one sentence.\n\n\
                     Conversation:\n{conv}"
                );
                spinner.begin_turn();
                let learn_result = tokio::select! {
                    r = agent.run(aegis_core::UserInput::Text(extract_prompt)) => Some(r),
                    _ = tokio::signal::ctrl_c() => { eprintln!("\n[goblin] interrupted"); None }
                };
                spinner.end_turn();
                if let Some(Ok(result)) = learn_result {
                    total.input_tokens =
                        total.input_tokens.saturating_add(result.usage.input_tokens);
                    total.output_tokens = total
                        .output_tokens
                        .saturating_add(result.usage.output_tokens);
                    turn_count += result.turns;
                    // Try to parse the skill block from the response
                    let content = &result.final_text;
                    {
                        // Extract fenced code block if present
                        let raw = if let Some(start) = content.find("```\n---") {
                            let s = &content[start + 4..];
                            s.find("\n```").map(|e| s[..e].to_string())
                        } else if content.contains("---\nname:") {
                            let s = content.find("---\nname:").map(|i| &content[i..]);
                            s.and_then(|s| {
                                s.find("\n```")
                                    .map(|e| s[..e].to_string())
                                    .or_else(|| Some(s.to_string()))
                            })
                        } else {
                            None
                        };
                        if let Some(skill_src) = raw {
                            eprintln!("\n\x1b[33m●\x1b[0m \x1b[1;33mlearned skill\x1b[0m — save to \x1b[2m~/.metis/skills/\x1b[0m? \x1b[2m(y/N)\x1b[0m");
                            eprintln!("\x1b[2m{skill_src}\x1b[0m");
                            let mut confirm = String::new();
                            let _ = std::io::stdin().read_line(&mut confirm);
                            if confirm.trim().eq_ignore_ascii_case("y") {
                                if let Some(home) = dirs::home_dir() {
                                    let skill_dir = home.join(".metis").join("skills");
                                    let _ = std::fs::create_dir_all(&skill_dir);
                                    // Extract name from frontmatter
                                    let skill_name = skill_src
                                        .lines()
                                        .find(|l| l.starts_with("name:"))
                                        .and_then(|l| l.split_once(':'))
                                        .map(|(_, v)| v.trim().to_string())
                                        .unwrap_or_else(|| {
                                            suggested_name
                                                .clone()
                                                .unwrap_or_else(|| "learned".to_string())
                                        });
                                    let dest = skill_dir.join(format!("{skill_name}.md"));
                                    if let Err(e) = std::fs::write(&dest, &skill_src) {
                                        eprintln!("[goblin] save failed: {e}");
                                    } else {
                                        eprintln!("[goblin] saved to {}", dest.display());
                                        skill_registry.load_dir_pub(&skill_dir);
                                    }
                                }
                            } else {
                                eprintln!("[goblin] skill discarded");
                            }
                        }
                    }
                }
                continue;
            }
            ReplCommand::SkillImprove(name) => {
                let skill = match skill_registry.get(&name) {
                    Some(s) => s.clone(),
                    None => {
                        eprintln!("[goblin] skill `{name}` not found");
                        continue;
                    }
                };
                let stats = aegis_core::skills::load_skill_stats(&name);
                let stats_line = if stats.use_count > 0 {
                    format!(
                        "Current stats: {} uses, {:.0}% success rate.",
                        stats.use_count,
                        stats.success_rate() * 100.0
                    )
                } else {
                    "No usage stats yet.".to_string()
                };
                let improve_prompt = format!(
                    "Improve this skill prompt to make it more reliable and effective. \
                     {stats_line}\n\n\
                     Current skill:\n\
                     ```\n\
                     ---\n\
                     name: {name}\n\
                     description: {desc}\n\
                     user_invocable: true\n\
                     ---\n\
                     {body}\n\
                     ```\n\n\
                     Rewrite the prompt body to be clearer and more actionable. \
                     Keep `$ARGS` where user arguments belong. \
                     Output only the improved skill file in the same format.",
                    desc = skill.description,
                    body = skill.prompt,
                );
                eprintln!("[goblin] asking LLM to improve `{name}`...");
                spinner.begin_turn();
                let improve_result = tokio::select! {
                    r = agent.run(aegis_core::UserInput::Text(improve_prompt)) => Some(r),
                    _ = tokio::signal::ctrl_c() => { eprintln!("\n[goblin] interrupted"); None }
                };
                spinner.end_turn();
                if let Some(Ok(result)) = improve_result {
                    total.input_tokens =
                        total.input_tokens.saturating_add(result.usage.input_tokens);
                    total.output_tokens = total
                        .output_tokens
                        .saturating_add(result.usage.output_tokens);
                    turn_count += result.turns;
                    {
                        let content = &result.final_text;
                        eprintln!("\n\x1b[33m●\x1b[0m \x1b[1;33mimproved skill\x1b[0m — apply? \x1b[2m(y/N)\x1b[0m");
                        eprintln!("\x1b[2m{content}\x1b[0m");
                        let mut confirm = String::new();
                        let _ = std::io::stdin().read_line(&mut confirm);
                        if confirm.trim().eq_ignore_ascii_case("y") {
                            if skill.source != std::path::Path::new("<builtin>") {
                                // Strip fenced block if LLM wrapped it
                                let clean = if let Some(i) = content.find("---\nname:") {
                                    content[i..].to_string()
                                } else {
                                    content.clone()
                                };
                                if let Err(e) = std::fs::write(&skill.source, &clean) {
                                    eprintln!("[goblin] write failed: {e}");
                                } else {
                                    eprintln!("[goblin] `{name}` updated");
                                    // Reload
                                    if let Some(home) = dirs::home_dir() {
                                        skill_registry
                                            .load_dir_pub(&home.join(".metis").join("skills"));
                                    }
                                }
                            } else {
                                eprintln!("[goblin] built-in skills can't be overwritten; save manually to ~/.metis/skills/{name}.md");
                            }
                        }
                    }
                }
                continue;
            }
            ReplCommand::Tasks => {
                let tasks = crate::tasks::load_tasks(workspace);
                eprint!("{}", crate::tasks::format_task_list(&tasks));
                continue;
            }
            ReplCommand::TaskAdd(text) => {
                match crate::tasks::add_task(workspace, &text) {
                    Ok(id) => eprintln!("\x1b[38;2;0;229;209m+\x1b[0m task #{id}: {text}"),
                    Err(e) => eprintln!("[goblin] {e}"),
                }
                continue;
            }
            ReplCommand::TaskDone(id) => {
                match crate::tasks::complete_task(workspace, id) {
                    Ok(text) => eprintln!("\x1b[32m✓\x1b[0m task #{id}: {text}"),
                    Err(e) => eprintln!("[goblin] {e}"),
                }
                continue;
            }
            ReplCommand::TaskRm(id) => {
                match crate::tasks::delete_task(workspace, id) {
                    Ok(text) => eprintln!("\x1b[2m✗ removed task #{id}: {text}\x1b[0m"),
                    Err(e) => eprintln!("[goblin] {e}"),
                }
                continue;
            }
            ReplCommand::TaskClear => {
                match crate::tasks::clear_done(workspace) {
                    Ok(0) => eprintln!("\x1b[2m(no completed tasks to clear)\x1b[0m"),
                    Ok(n) => eprintln!("\x1b[38;2;0;229;209mcleared {n} completed task(s)\x1b[0m"),
                    Err(e) => eprintln!("[goblin] {e}"),
                }
                continue;
            }
            ReplCommand::Btw(ref text) => {
                let preview: String = text.chars().take(60).collect();
                let ellipsis = if text.chars().count() > 60 { "…" } else { "" };
                eprintln!("\x1b[2m[btw] {preview}{ellipsis}\x1b[0m");
                let text_owned = text.clone();
                tokio::spawn(async move {
                    let btw_client = aegis_api::Provider::lookup("nvidia")
                        .and_then(|p| p.client_from_env().ok());
                    let btw_client = match btw_client {
                        Some(c) => c,
                        None => {
                            eprintln!("[btw] nvidia provider unavailable");
                            return;
                        }
                    };
                    let req = aegis_api::ChatRequest {
                        model: "deepseek-ai/deepseek-v4-flash".to_string(),
                        messages: vec![aegis_api::ChatMessage::user(text_owned)],
                        tools: None,
                        temperature: Some(0.7),
                        max_tokens: Some(512),
                        thinking: false,
                        thinking_budget: 0,
                    };
                    match btw_client.chat(&req).await {
                        Ok(resp) => {
                            let answer = resp
                                .choices
                                .first()
                                .and_then(|c| c.message.content.clone())
                                .unwrap_or_default();
                            if !answer.is_empty() {
                                eprintln!("\x1b[2m[btw] {answer}\x1b[0m");
                            }
                        }
                        Err(e) => eprintln!("[btw] {e}"),
                    }
                });
                continue;
            }
            ReplCommand::Prompt(text) => {
                let _ = editor.add_history_entry(&text);
                // Erase the `metis> <input>` line so only the response
                // and cost footer remain visible — cleaner output.
                // Use raw `line` (not trimmed `text`) for accurate width.
                erase_input_line(&line, prompt_visible_width);

                // Auto-model routing: classify this prompt and swap
                // model (and provider if specified) if routing is enabled.
                if let Some(target) = crate::router::route(&routing, &text) {
                    let tier = crate::router::classify(&text);
                    // Switch provider first if needed (e.g. "glm:glm-5.1")
                    if let Some(ref pname) = target.provider {
                        if let Some(provider) = aegis_api::Provider::lookup(pname) {
                            match provider.client_from_env() {
                                Ok(new_client) => {
                                    drop(agent);
                                    client = Arc::from(new_client);
                                    model = target.model.clone();
                                    if let Ok(mut r) = md_renderer.lock() {
                                        r.set_dedup_enabled(resolve_dedup_enabled(pname));
                                    }
                                    agent = build_agent(
                                        &*client,
                                        &registry,
                                        workspace,
                                        AgentConfig {
                                            model: target.model.clone(),
                                            ..config.clone()
                                        },
                                        Arc::clone(&permission),
                                        None,
                                        Some(Arc::clone(&plan_state_flag)),
                                        Arc::clone(&md_renderer),
                                        Arc::clone(&client),
                                        Arc::clone(&registry),
                                        sandbox.clone(),
                                        Arc::clone(&spinner),
                                        #[cfg(unix)]
                                        Arc::clone(&overlay),
                                        #[cfg(feature = "ctx")]
                                        blob_handles.clone(),
                                    )?;
                                    eprintln!(
                                        "\x1b[38;2;220;50;50m[router]\x1b[0m [1m{tier:?}[22m → {pname}:{}", target.model
                                    );
                                }
                                Err(e) => {
                                    eprintln!("[router] can't switch to {pname}: {e}, using current provider");
                                    agent.set_model(target.model.clone());
                                }
                            }
                        } else {
                            eprintln!(
                                "[router] unknown provider `{pname}`, using current provider"
                            );
                            agent.set_model(target.model.clone());
                        }
                    } else {
                        eprintln!(
                            "\x1b[38;2;220;50;50m[router]\x1b[0m [1m{tier:?}[22m → {}",
                            target.model
                        );
                        let _ = agent.append_note(&format!(
                            "Model switched to `{}` (tier: {tier:?}). This is your current model identity.", target.model
                        ));
                        agent.set_model(target.model);
                    }
                }

                // Detect explicit stop signals ("dur", "hayır", "yok", "stop",
                // etc.). When the user sends ONLY a stop word, the model often
                // reinterprets it as context and continues. We intercept and
                // rewrite the message so the model can't misread the intent.
                let text = if is_stop_signal(&text) {
                    format!(
                        "[SYSTEM INTERRUPT] The user said: \"{text}\"\n\
                         This is an unconditional STOP. Do NOT continue, do NOT \
                         reinterpret, do NOT explain what you were doing. \
                         Acknowledge in one short sentence and wait.",
                        text = text
                    )
                } else {
                    text.clone()
                };

                // Build user input: multimodal if images are attached.
                // Clone text for potential fallback use
                let text_clone = text.clone();
                let user_input = if pending_images.is_empty() {
                    aegis_core::UserInput::Text(text_clone)
                } else {
                    let n = pending_images.len();
                    eprintln!(
                        "\x1b[35m[image]\x1b[0m sending {n} image{} with prompt",
                        if n == 1 { "" } else { "s" }
                    );
                    match aegis_core::UserInput::with_images(&text_clone, &pending_images) {
                        Ok(input) => {
                            pending_images.clear();
                            input
                        }
                        Err(e) => {
                            eprintln!("[goblin] failed to read image(s): {e}");
                            pending_images.clear();
                            continue;
                        }
                    }
                };

                // Hard stop at the daily budget ceiling. Runs BEFORE the
                // agent turn so a single prompt cannot burn through. Cheap
                // when the guard is disabled or budget is unset — just
                // two `Option` checks. The prompt fires at most once per
                // over-budget turn; the user can pick `a` to silence it
                // for the rest of this REPL session.
                if budget_hard_stop && !budget_stop_overridden {
                    if let Some(limit) = daily_budget_usd {
                        let pricing = aegis_core::ModelPricing::resolve(&model);
                        let session_usd = pricing.estimate(&total).total_usd();
                        let total_usd = aegis_core::spent_today() + session_usd;
                        if total_usd >= limit {
                            match budget_hard_stop_prompt(total_usd, limit) {
                                BudgetHardStopChoice::Continue => {}
                                BudgetHardStopChoice::Always => {
                                    budget_stop_overridden = true;
                                }
                                BudgetHardStopChoice::Stop => {
                                    eprintln!(
                                        "\x1b[1;33m[goblin]\x1b[0m turn cancelled \
                                         (daily budget ${limit:.2} exceeded — at \
                                         ${total_usd:.4}). Run `/budget` or `/exit`."
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }

                // Reset per-turn dedup state so the next response
                // starts with a clean line history.
                if let Ok(mut r) = md_renderer.lock() {
                    r.reset_turn();
                }

                // Wrap agent.run with Ctrl+C handling so the user can
                // interrupt a long-running turn without killing the
                // entire process. The session is persisted up to the
                // last completed turn, so no context is lost.
                #[cfg(unix)]
                spinner.begin_turn_with_overlay(
                    Arc::clone(&overlay.term_lock),
                    Arc::clone(&overlay.buffer),
                    Arc::clone(&overlay.drawn),
                );
                #[cfg(not(unix))]
                spinner.begin_turn();
                #[cfg(unix)]
                overlay.start(spinner.drawn_flag());
                // 10-minute hardcap on a single turn. The provider call
                // already has a 300s per-attempt timeout (see agent.rs),
                // so 600s here is belt-and-suspenders for cases where
                // tool-execution between calls or an infinite tool loop
                // burns time without hitting the provider timeout. Maps
                // to None so the user sees the "[goblin] interrupted"
                // path, same UX as Ctrl+C.
                let hardcap = std::time::Duration::from_secs(600);
                let run_result = tokio::select! {
                    result = agent.run(user_input) => Some(result),
                    _ = tokio::signal::ctrl_c() => {
                            None
                    }
                    _ = tokio::time::sleep(hardcap) => None,
                };
                spinner.end_turn();
                #[cfg(unix)]
                let (drained, overlay_partial) = overlay.stop_and_collect();
                #[cfg(not(unix))]
                let drained: std::collections::VecDeque<String> = std::collections::VecDeque::new();
                #[cfg(not(unix))]
                let overlay_partial = String::new();

                match run_result {
                    Some(Ok(output)) => {
                        if !drained.is_empty() {
                            eprintln!(
                                "\x1b[2m[goblin] {} input{} queued\x1b[0m",
                                drained.len(),
                                if drained.len() == 1 { "" } else { "s" }
                            );
                            pending_inputs.extend(drained);
                        }
                        pending_partial = overlay_partial;
                        // Flush any remaining partial markdown line
                        if let Ok(mut r) = md_renderer.lock() {
                            r.finish();
                        }
                        println!();
                        total.input_tokens =
                            total.input_tokens.saturating_add(output.usage.input_tokens);
                        total.output_tokens = total
                            .output_tokens
                            .saturating_add(output.usage.output_tokens);
                        total.cache_read_tokens = total
                            .cache_read_tokens
                            .saturating_add(output.usage.cache_read_tokens);
                        total.cache_write_tokens = total
                            .cache_write_tokens
                            .saturating_add(output.usage.cache_write_tokens);
                        turn_count = turn_count.saturating_add(1);
                    }
                    Some(Err(err)) => {
                        if !drained.is_empty() {
                            eprintln!(
                                "\x1b[2m[goblin] {} input{} queued\x1b[0m",
                                drained.len(),
                                if drained.len() == 1 { "" } else { "s" }
                            );
                            pending_inputs.extend(drained.clone());
                        }

                        // REPL-level fallback: pick the right retry
                        // target for this error class — `vision_fallback`
                        // for image rejections, `fallback_model` for
                        // everything else. Logic lives in
                        // `RoutingConfig::select_retry_fallback` so it
                        // is unit-tested directly.
                        let err_text = format!("{err:#}");
                        let fallback = crate::router::RoutingConfig::select_retry_fallback(
                            &err_text,
                            &routing,
                            &original_routing,
                        );

                        if let Some(ref fb_spec) = fallback {
                            let target = crate::router::RouteTarget::parse(fb_spec);
                            eprintln!("\x1b[33m[goblin] error: {err:#}\x1b[0m");
                            eprintln!(
                                "\x1b[38;2;122;240;227m[goblin] falling back to {fb_spec}…\x1b[0m"
                            );

                            // Switch provider if needed.
                            let switch_ok = if let Some(ref pname) = target.provider {
                                match aegis_api::Provider::lookup(pname) {
                                    Some(provider) => match provider.client_from_env() {
                                        Ok(new_client) => {
                                            drop(agent);
                                            client = Arc::from(new_client);
                                            let fb_config = AgentConfig {
                                                model: target.model.clone(),
                                                ..config.clone()
                                            };
                                            agent = build_agent(
                                                &*client,
                                                &registry,
                                                workspace,
                                                fb_config,
                                                Arc::clone(&permission),
                                                None,
                                                Some(Arc::clone(&plan_state_flag)),
                                                Arc::clone(&md_renderer),
                                                Arc::clone(&client),
                                                Arc::clone(&registry),
                                                sandbox.clone(),
                                                Arc::clone(&spinner),
                                                #[cfg(unix)]
                                                Arc::clone(&overlay),
                                                #[cfg(feature = "ctx")]
                                                blob_handles.clone(),
                                            )?;
                                            model = target.model.clone();
                                            true
                                        }
                                        Err(e) => {
                                            eprintln!("[goblin] fallback provider `{pname}` unavailable: {e}");
                                            false
                                        }
                                    },
                                    None => {
                                        eprintln!("[goblin] fallback provider `{pname}` unknown");
                                        false
                                    }
                                }
                            } else {
                                // Same provider, just swap model.
                                agent.set_model(target.model.clone());
                                model = target.model.clone();
                                true
                            };

                            if switch_ok {
                                spinner.begin_turn();
                                // Rebuild user_input from original text since we're retrying
                                // Note: pending_images is empty at this point because it was cleared
                                // when the first attempt's UserInput was created
                                let user_input_for_retry =
                                    aegis_core::UserInput::Text(text.clone());

                                let retry_result = tokio::select! {
                                    result = agent.run(user_input_for_retry) => Some(result),
                                    _ = tokio::signal::ctrl_c() => {
                                                            None
                                    }
                                };
                                spinner.end_turn();
                                match retry_result {
                                    Some(Ok(output)) => {
                                        if let Ok(mut r) = md_renderer.lock() {
                                            r.finish();
                                        }
                                        println!();
                                        total.input_tokens = total
                                            .input_tokens
                                            .saturating_add(output.usage.input_tokens);
                                        total.output_tokens = total
                                            .output_tokens
                                            .saturating_add(output.usage.output_tokens);
                                        total.cache_read_tokens = total
                                            .cache_read_tokens
                                            .saturating_add(output.usage.cache_read_tokens);
                                        total.cache_write_tokens = total
                                            .cache_write_tokens
                                            .saturating_add(output.usage.cache_write_tokens);
                                        turn_count = turn_count.saturating_add(1);
                                    }
                                    Some(Err(e)) => {
                                        eprintln!("[goblin] fallback also failed: {e:#}")
                                    }
                                    None => {
                                        if let Ok(mut r) = md_renderer.lock() {
                                            r.finish();
                                        }
                                        println!();
                                    }
                                }
                                continue;
                            }
                        }

                        eprintln!("\x1b[33m[goblin] error: {err:#}\x1b[0m");
                        eprintln!("\x1b[2m[goblin] {}\x1b[0m", recovery_hint_for_error(&err));
                    }
                    None => {
                        // Ctrl+C: discard whatever was typed during
                        // the run. The user pressed interrupt — don't
                        // surprise them by silently submitting queued
                        // prompts as if nothing happened.
                        if !drained.is_empty() {
                            eprintln!(
                                "\x1b[2m[goblin] discarded {} input{} typed during run (interrupted)\x1b[0m",
                                drained.len(),
                                if drained.len() == 1 { "" } else { "s" }
                            );
                        }
                        // Flush renderer state so the next prompt
                        // starts with a clean terminal.
                        if let Ok(mut r) = md_renderer.lock() {
                            r.finish();
                        }
                        println!();
                    }
                }
                // Check for completed background agents
                drain_background_results(&agent);
            }
        }
    }

    // Save history before leaving so the next REPL session sees the
    // arrow-key history. Failures are non-fatal — we already printed
    // any cost summary the user cares about.
    let _ = editor.save_history(&history_file);
    eprintln!("{}", format_cost_footer(&total, &model));

    // Save session_id now — agent.session() becomes None after take_session().
    let session_id_for_telemetry = agent.session().map(|s| s.id().to_string());

    // Extract and save cross-session insights (dedupe + reinforce via upsert)
    if let Some(session) = agent.session() {
        let insights = aegis_core::learning::extract_insights(session.messages(), workspace);
        for insight in &insights {
            let _ = aegis_core::learning::upsert_insight(insight);
        }
        // Explicit user rules mined from imperatives ("from now on X",
        // "never Y", "bundan sonra Z", "hiç … yapma"). Upserted like any
        // other insight; format_insights_section renders them under a
        // dedicated "hard rules" header so the model doesn't bury them.
        let rules = aegis_core::learning::extract_instructions(session.messages(), workspace);
        for rule in &rules {
            let _ = aegis_core::learning::upsert_insight(rule);
        }
        // Apply net tool-level feedback signals to any existing insights
        // tagged with that tool name (reinforces clean tools, penalises
        // stuck ones — recovery sequences are skipped to avoid double-count).
        let feedback = aegis_core::learning::extract_tool_feedback(session.messages());
        for (tool_tag, positive) in &feedback {
            let _ = aegis_core::learning::record_feedback_by_tag(workspace, tool_tag, *positive);
        }
    }

    // Auto-run LLM-based memory extraction on sessions with meaningful content
    if config.auto_memory {
        let user_turn_count = agent
            .session_messages()
            .iter()
            .filter(|m| m.role == aegis_api::Role::User)
            .count();
        if user_turn_count >= config.auto_memory_min_turns {
            let mut conv = String::new();
            for m in agent.session_messages() {
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
                // Detach session so the insights turn is NOT appended to
                // the session file — --resume should see only real user
                // work, not auto-extraction boilerplate.
                let _session = agent.take_session();
                match agent.run(aegis_core::UserInput::Text(insight_prompt)).await {
                    Ok(out) => {
                        total.input_tokens =
                            total.input_tokens.saturating_add(out.usage.input_tokens);
                        total.output_tokens =
                            total.output_tokens.saturating_add(out.usage.output_tokens);
                        total.cache_read_tokens = total
                            .cache_read_tokens
                            .saturating_add(out.usage.cache_read_tokens);
                        total.cache_write_tokens = total
                            .cache_write_tokens
                            .saturating_add(out.usage.cache_write_tokens);
                    }
                    Err(e) => eprintln!("[goblin] auto-memory error: {e}"),
                }
                // Restore session so FT export below still works.
                if let Some(s) = _session {
                    agent.restore_session(s);
                }
            }
        }
    }

    // Auto-export fine-tuning data on exit
    if let Some(session) = agent.session() {
        let messages = session.messages();
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
                            && m.content.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
                    })
                    .map(|pos| i + 1 + pos)
                {
                    let user_text = messages[i].content.clone().unwrap_or_default();
                    let asst_text = messages[j].content.clone().unwrap_or_default();
                    if !user_text.is_empty() && !asst_text.is_empty() {
                        let mut msgs = Vec::new();
                        if !system_content.is_empty() {
                            msgs.push(
                                serde_json::json!({"role":"system","content":system_content}),
                            );
                        }
                        msgs.push(serde_json::json!({"role":"user","content":user_text}));
                        msgs.push(serde_json::json!({"role":"assistant","content":asst_text}));
                        examples.push(serde_json::json!({"messages": msgs}));
                    }
                    i = j + 1;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        if !examples.is_empty() {
            let sid = session.id();
            let out_path = metis_dir.join(format!("ft_{sid}.jsonl"));
            let content = examples
                .iter()
                .map(|e| serde_json::to_string(e).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            if std::fs::write(&out_path, content).is_ok() {
                eprintln!(
                    "[goblin] ft export: {} examples → {}",
                    examples.len(),
                    out_path.display()
                );
            }
        }
    }

    // Record telemetry after auto-memory so its token cost is included.
    {
        let pricing = aegis_core::ModelPricing::resolve(&model);
        let cost = pricing.estimate(&total);
        let record = aegis_core::telemetry::TelemetryRecord {
            timestamp: aegis_core::telemetry::now_iso8601(),
            session_id: session_id_for_telemetry,
            model: model.to_string(),
            provider: String::new(),
            input_tokens: total.input_tokens,
            output_tokens: total.output_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_write_tokens: total.cache_write_tokens,
            turns: turn_count,
            cost_usd: cost.total_usd(),
            tool_calls: std::collections::HashMap::new(),
        };
        let _ = aegis_core::telemetry::append_record(&record);
    }

    eprintln!("[goblin] bye");
    Ok(())
}

/// Drain completed background agents and print their results to stderr.
fn drain_background_results(agent: &Agent<'_>) {
    let completed = agent.ctx().background_agents.drain_completed();
    for bg in completed {
        match bg.result {
            Ok(text) => {
                eprintln!(
                    "\n\x1b[38;2;122;240;227m[background agent completed: {}]\x1b[0m",
                    bg.description
                );
                eprintln!("{text}");
            }
            Err(err) => {
                eprintln!(
                    "\n\x1b[37m[background agent failed: {}]\x1b[0m {err}",
                    bg.description
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
