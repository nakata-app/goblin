//! REPL agent construction — threads the config, permission, tools,
//! and UI widgets into a ready-to-run `Agent<a>`.
//!
//! Kept separate from repl.rs so the turn loop there can focus on
//! dispatch instead of wiring. `build_agent` is the only public entry;
//! everything it returns is standard `aegis_core` types.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use aegis_api::{ChatProvider, StreamEvent};
use aegis_core::{Agent, AgentConfig, Permission, SessionStore, ToolContext, ToolRegistry};

use crate::agent_spawner;
use crate::markdown::MdRenderer;

use super::format::{canonical_tool_name, format_tool_call, trim_tool_preview};
#[cfg(unix)]
use super::overlay::InputOverlay;
use super::spinner::ThinkingSpinner;

/// Builds an agent bound to a session. If `session` is `Some`, the
/// agent adopts it as-is (used by `/fork` to continue on a just-copied
/// branch). If `None`, a fresh session id is minted — the behaviour
/// the REPL entry point and `/clear` both want.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_agent<'a>(
    client: &'a dyn ChatProvider,
    registry: &'a ToolRegistry,
    workspace: &Path,
    config: AgentConfig,
    permission: Arc<dyn Permission>,
    session: Option<SessionStore>,
    plan_state: Option<Arc<Mutex<aegis_core::PlanState>>>,
    md: Arc<Mutex<MdRenderer>>,
    spawner_client: Arc<dyn ChatProvider>,
    spawner_registry: Arc<ToolRegistry>,
    sandbox: aegis_core::SandboxMode,
    spinner: Arc<ThinkingSpinner>,
    #[cfg(unix)] overlay: Arc<InputOverlay>,
    #[cfg(feature = "ctx")] blob_handles: Option<(
        Arc<aegis_core::BlobStore>,
        Arc<aegis_core::BlobIndex>,
    )>,
) -> Result<Agent<'a>> {
    let session = match session {
        Some(s) => s,
        None => {
            let id = SessionStore::new_id();
            let s = SessionStore::open(workspace, &id).context("could not open REPL session")?;
            eprintln!("\x1b[1;32m[goblin] repl session={id}\x1b[0m");
            s
        }
    };
    let mut ctx = ToolContext::new(workspace.to_path_buf()).with_user_input(std::sync::Arc::new(
        |question, options| {
            // ask_user accepts &[&str], options is &[String]; borrow as &str slice.
            let opts: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
            match ask_user(question, &opts) {
                Ok(s) => Some(s),
                Err(_) => None,
            }
        },
    ));
    if let Some(ps) = plan_state {
        ctx = ctx.with_plan_state(ps);
    }
    ctx.bash.sandbox = sandbox;
    let hooks = aegis_core::load_hooks(workspace);
    ctx = ctx.with_hooks(hooks);

    // Wire the shared agent-spawner so `agent` and `parallel_agents` tools
    // can fan out work onto subagents. See `crate::agent_spawner` for the
    // single source of truth shared by REPL/TUI/one-shot/IDE.
    let spawner = agent_spawner::build(
        Arc::clone(&spawner_client),
        Arc::clone(&spawner_registry),
        workspace,
        config.clone(),
        Arc::clone(&permission),
        ctx.background_agents.clone(),
    );
    ctx = ctx.with_agent_spawner(spawner);

    const SR: &str = "\x1b[0m";

    let mut agent = Agent::new(client, registry, ctx, config)
        .with_permission(permission)
        .with_guardrail(aegis_core::guardrail::load_default(workspace))
        .with_session(session);
    #[cfg(feature = "ctx")]
    if let Some((store, index)) = blob_handles {
        agent = agent.with_blob_handles(store, index);
    }
    Ok(agent
        .with_stream_callback({
            let md = Arc::clone(&md);
            let spinner = Arc::clone(&spinner);
            #[cfg(unix)] let overlay = Arc::clone(&overlay);
            move |event| {
                spinner.note_event();
                match event {
                StreamEvent::TextDelta(text) => {
                    #[cfg(unix)] overlay.before_output();
                    // Hold term_lock while writing to stdout so the overlay
                    // thread's stderr writes cannot interleave and corrupt
                    // the terminal cursor / scroll region.
                    #[cfg(unix)] let _term_lk = overlay.term_lock.lock().unwrap();
                    if let Ok(mut r) = md.lock() {
                        r.push(&text);
                    }
                }
                StreamEvent::ThinkingDelta(_text) => {}
                StreamEvent::ToolCall {
                    name,
                    arguments_preview,
                } => {
                    if let Ok(mut r) = md.lock() {
                        r.finish();
                    }
                    let _ = std::io::stdout().flush();
                    #[cfg(unix)] overlay.before_output();
                    let canonical = canonical_tool_name(&name);
                    let arg = format_tool_call(&name, &arguments_preview);
                    eprintln!("\x1b[38;2;220;220;210m●{SR} \x1b[38;2;220;220;210m{canonical}{SR}\x1b[2m  {arg}{SR}");
                }
                StreamEvent::ToolResult {
                    name: _,
                    preview,
                    is_error,
                } => {
                    let trimmed_preview = trim_tool_preview(&preview);
                    #[cfg(unix)] overlay.before_output();
                    // Drop empty previews entirely — `[stdout]` with no
                    // body should not produce a hanging `⎿` line.
                    if trimmed_preview.is_empty() {
                        #[cfg(unix)] overlay.after_output();
                    } else if is_error {
                        eprintln!("  \x1b[31m⎿ {trimmed_preview}{SR}");
                        #[cfg(unix)] overlay.after_output();
                    } else {
                        eprintln!("  \x1b[2m⎿ {trimmed_preview}{SR}");
                        #[cfg(unix)] overlay.after_output();
                    }
                }
                StreamEvent::Usage(_) => {}
                StreamEvent::RetryReset => {
                    if let Ok(mut r) = md.lock() {
                        // Preserve the provider-specific dedup setting
                        // across a retry — rebuilding the renderer from
                        // scratch would re-enable long-line dedup on
                        // z.ai / GLM sessions and swallow verbatim
                        // repeated lines again.
                        let keep_dedup = r.dedup_enabled();
                        *r = crate::markdown::MdRenderer::new();
                        r.set_dedup_enabled(keep_dedup);
                    }
                    #[cfg(unix)] overlay.before_output();
                    eprint!("\r\x1b[2K\x1b[2m(retrying…){SR}\r\x1b[2K");
                    let _ = std::io::stderr().flush();
                }
                }
            }
        }))
}

/// Show an interactive terminal menu. Renders the prompt and a numbered
/// list of options; the user navigates with ↑/↓ and confirms with Enter.
/// Returns the selected option text, or `io::Error` if the terminal
/// is not available or raw-mode fails.
pub fn ask_user(prompt: &str, options: &[&str]) -> io::Result<String> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;

    let mut selected = 0usize;

    loop {
        // ── render ──────────────────────────────────────────────
        write!(stdout, "{}\r\n", prompt)?;
        for (i, opt) in options.iter().enumerate() {
            if i == selected {
                write!(stdout, "\x1b[7m {}. {} \x1b[0m\r\n", i + 1, opt)?;
            } else {
                write!(stdout, " {}. {}\r\n", i + 1, opt)?;
            }
        }
        // Move cursor back up so we can redraw on the next iteration.
        let lines = options.len() + 1; // prompt + one per option
        write!(stdout, "{}", cursor::MoveUp(lines as u16))?;
        stdout.flush()?;

        // ── input ────────────────────────────────────────────────
        match event::read()? {
            Event::Key(kv) if kv.kind == KeyEventKind::Press => match kv.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = selected.saturating_add(1).min(options.len() - 1);
                }
                KeyCode::Enter => {
                    // Clear the rendered menu.
                    let clear_lines = options.len() + 1;
                    write!(stdout, "{}", cursor::MoveUp(clear_lines as u16))?;
                    for _ in 0..clear_lines {
                        write!(stdout, "{}", Clear(ClearType::CurrentLine))?;
                        write!(stdout, "{}", cursor::MoveDown(1))?;
                    }
                    write!(stdout, "{}", cursor::MoveUp(clear_lines as u16))?;
                    stdout.flush()?;
                    disable_raw_mode()?;
                    return Ok(options[selected].to_string());
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    disable_raw_mode()?;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                _ => {}
            },
            _ => {}
        }
    }
}
