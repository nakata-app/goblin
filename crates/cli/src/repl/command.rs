//! Slash-command parsing and display.
//!
//! `ReplCommand` is the result of parsing a single line of REPL input.
//! `parse` handles every `/cmd` path; anything else becomes
//! `ReplCommand::Prompt(line)`. `HELP_TEXT` is the canonical list
//! printed by `/help` and checked by tests to keep SLASH_COMMANDS and
//! the help screen in sync.

/// Outcome of parsing a single line of REPL input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReplCommand {
    /// Empty input — redraw the prompt.
    Empty,
    /// Send this string to the agent as a user message.
    Prompt(String),
    /// `/exit` or `/quit`.
    Exit,
    /// `/clear` — wipe the transcript and start a new session.
    Clear,
    /// `/cost` — print cumulative token + dollar usage.
    Cost,
    /// `/cost off` — suppress the per-turn cost footer.
    CostOff,
    /// `/cost on` — re-enable the per-turn cost footer.
    CostOn,
    /// `/fork [name] [n]` — branch the conversation with optional name
    /// and message count.
    Fork {
        name: Option<String>,
        take: Option<usize>,
    },
    /// `/overthink` — toggle extended thinking mode.
    Overthink,
    /// `/plan` — toggle plan mode (read-only tools only).
    Plan,
    /// `/skills` — list available skills.
    Skills,
    /// `/sessions` — list every session on disk under this workspace,
    /// newest first.
    Sessions,
    /// `/tree` — show the branch tree of all sessions.
    Tree,
    /// `/session` — show info about the current session.
    SessionInfo,
    /// `/update` — check for and install updates.
    Update,
    /// `/stats` — show usage telemetry dashboard.
    Stats,
    /// `/resume <id>` — switch the REPL to an existing session on
    /// disk. The prior transcript is replayed as the starting point
    /// so the next prompt continues from wherever that session left
    /// off. Missing or empty argument becomes [`Self::Unknown`].
    Resume(String),
    /// `/compact` — force an immediate compaction of the transcript.
    Compact,
    /// `/skill-install <source>` — install skills from a git repo, URL, or path.
    SkillInstall(String),
    /// `/skill-uninstall <name>` — remove an installed skill.
    SkillUninstall(String),
    /// `/skill-search <query>` — search available skills.
    SkillSearch(String),
    /// `/learn-skill [name]` — extract a reusable skill from the current session.
    LearnSkill(Option<String>),
    /// `/skill-rate <name> good|bad` — record outcome for a skill.
    SkillRate { name: String, good: bool },
    /// `/skill-improve <name>` — ask LLM to rewrite an underperforming skill prompt.
    SkillImprove(String),
    /// `/provider <name>` — switch provider (rebuilds agent).
    ProviderSwitch(String),
    /// `/model <name>` — switch model without changing provider.
    ModelSwitch(String),
    /// `/help` — print the slash command list.
    Help,
    /// `/` alone — compact command menu.
    SlashMenu,
    /// `/providers` — list available providers and their status.
    Providers,
    /// `/image <path>` — attach an image to the next prompt.
    /// Raw user arg (unresolved) — expanded by `path_input::resolve_many`
    /// when handled, so `~/`, quoted paths, multi-path, and `file://`
    /// URLs all work.
    Image(String),
    /// `/images` — list currently attached images.
    Images,
    /// `/images clear` — clear all attached images.
    ImagesClear,
    /// `/paste` — attach an image from the system clipboard (macOS).
    Paste,
    /// `/files [path]` — browse project files and directories with interactive tree view.
    Files(Option<String>),
    /// `/view <path>` — preview file contents with syntax highlighting.
    View(String),
    /// `/search <pattern>` — search files for text pattern with grep-like interface.
    Search(String),
    /// `/tasks` — list all user tasks.
    Tasks,
    /// `/task add <text>` — add a new user task.
    TaskAdd(String),
    /// `/task done <id>` — mark a user task as done.
    TaskDone(u32),
    /// `/task rm <id>` — delete a user task.
    TaskRm(u32),
    /// `/task clear` — remove all completed tasks.
    TaskClear,
    /// `! <command>` — run a shell command in the REPL.
    Shell(String),
    /// `/btw <note>` — drop a "by the way" context note into the
    /// session transcript without calling the model. The note is
    /// persisted as a user-role message wrapped with a marker so the
    /// model sees it on the next real turn but doesn't reply to it
    /// immediately. Mid-stream typing of `/btw ...` is queued by the
    /// post-stream drain and processed on the next REPL loop iteration,
    /// so it never interrupts a running turn.
    Btw(String),
    /// `/key <env_var> <value>` — set an API key for a provider at runtime.
    /// Example: `/key OPENAI_API_KEY sk-...`
    SetKey { env_var: String, value: String },
    /// `/models` — show available models and let user pick one
    ModelMenu,
    /// `/swarm [N] <prompt>` — spawn N parallel agents working on the same prompt from different angles.
    /// Agents work independently with different perspectives, then results are aggregated.
    /// N defaults to 3, minimum 2, maximum 10.
    /// `/swarm [N] [quorum:M] <prompt>` — N parallel agents, consensus requires M agreements.
    Swarm {
        prompt: String,
        n: usize,
        quorum: usize,
    },
    /// `/glm <prompt>` — send a one-shot query to the GLM model as a side consultation.
    /// The response is displayed and injected into the main agent's context.
    Consult { provider: String, prompt: String },
    /// `/race <prompt>` — send the same prompt to 3 strong models in parallel,
    /// show all responses, and inject the best one into the main agent context.
    Race(String),
    /// `/budget` — print today's spend (prior sessions + current),
    /// plus the configured `daily_budget_usd` ceiling if one is set.
    /// Purely informational — no side-effects, no model call.
    Budget,
    /// `/dag` — show the tool-call chain for the current session as an ASCII tree.
    Dag,
    /// `/map [max_files]` — print the tree-sitter repo map for the current
    /// workspace. Optional integer argument caps the number of source files
    /// scanned (default 200). Unlike calling the `repo_map` tool via the
    /// model, this runs locally and prints straight to stderr so the user
    /// can inspect structure without spending tokens.
    Map(Option<usize>),
    /// `/insights` — extract non-obvious learnings from the current session
    /// and save them to memory via memory_save calls.
    Insights,
    /// `/learn <text>` — immediately save a rule/insight to memory.
    Learn(String),
    /// `/advisor` — switch to architecture/code review advisor mode.
    /// In this mode the system prompt focuses on analysis; tools are blocked.
    Advisor,
    /// `/advisor off` — exit advisor mode and restore the normal agent.
    AdvisorOff,
    /// `/autotune` — toggle adaptive temperature on/off for this session.
    AutotuneToggle,
    /// `/security` — show autonomous security status (limits, counters, kill switch state).
    /// `/security kill` — trigger the kill switch immediately.
    /// `/security resume` — resume after kill switch.
    Security(Option<String>),
    /// `/multi-model` — toggle multi-model evaluation (GODMODE)
    MultiModelToggle,
    /// `/perturbation` — toggle prompt perturbation (GODMODE)
    PerturbationToggle,
    /// `/parallel` — toggle parallel models (GODMODE)
    ParallelToggle,
    /// `/api-keys` — toggle API key management (GODMODE)
    ApiKeysToggle,
    /// `/export-ft [output.jsonl]` — export session history as OpenAI
    /// fine-tuning JSONL. Each user→assistant pair becomes one training
    /// example. Tool calls and system messages are filtered out.
    ExportFt(Option<String>),
    /// `/godmode` — toggle all GODMODE features at once
    GodmodeToggle,
    /// `/browser` — attach Playwright MCP (browser automation) for this session.
    Browser,
    /// `/computer` — attach open-computer-use MCP (full OS control) for this session.
    Computer,
    /// `/context` — show session context summary (system prompt, recent messages, etc.).
    Context,
    /// `/tokens` — show detailed token usage breakdown.
    Tokens,
    /// `/history` — show recent message history.
    History,
    /// `/copy` — copy the last assistant message to clipboard (OSC 52).
    Copy,
    /// `/something-we-don't-know`. The string is the unknown command name
    /// (without the leading slash) so the caller can render it.
    Unknown(String),
}

/// Known model aliases: maps human-friendly fragments to wire model ids.
/// Checked in order — first match wins.
const MODEL_ALIASES: &[(&[&str], &str)] = &[
    // DeepSeek
    (&["deepseek", "v4", "flash"], "deepseek-v4-flash"),
    (&["deepseek", "v4", "pro"], "deepseek-v4-pro"),
    (&["deepseek", "v4"], "deepseek-v4-flash"),
    (&["deepseek", "v3.2"], "deepseek-v3.2"),
    (&["deepseek", "3.2"], "deepseek-v3.2"),
    (&["deepseek", "v3"], "deepseek-v3.2"),
    (&["deepseek", "flash"], "deepseek-v4-flash"),
    (&["deepseek", "pro"], "deepseek-v4-pro"),
    (&["deepseek", "chat"], "deepseek-chat"),
    (&["deepseek", "reasoner"], "deepseek-reasoner"),
    (&["deepseek", "r1"], "deepseek-reasoner"),
    // Gemini
    (&["gemini", "2.5", "flash"], "gemini-2.5-flash"),
    (&["gemini", "2.5", "pro"], "gemini-2.5-pro"),
    (&["gemini", "flash"], "gemini-2.5-flash"),
    (&["gemini", "pro"], "gemini-2.5-pro"),
    // OpenAI
    (&["gpt", "4.1"], "gpt-4.1"),
    (&["gpt", "4.1", "mini"], "gpt-4.1-mini"),
    (&["gpt", "4.1", "nano"], "gpt-4.1-nano"),
    (&["gpt", "4o"], "gpt-4o"),
    (&["gpt", "4o", "mini"], "gpt-4o-mini"),
    (&["o3"], "o3"),
    (&["o4", "mini"], "o4-mini"),
    // Anthropic
    (&["opus"], "claude-opus-4-6"),
    (&["sonnet"], "claude-sonnet-4-6"),
    (&["haiku"], "claude-haiku-4-5-20251001"),
    // Gemini
    (&["gemini"], "gemini-2.5-flash"),
    (&["flash"], "gemini-2.5-flash"),
    (&["gemini", "flash"], "gemini-2.5-flash"),
    (&["gemini", "2.5"], "gemini-2.5-flash"),
    (&["gemini", "2.5", "flash"], "gemini-2.5-flash"),
    (&["pro"], "gemini-2.5-pro"),
    (&["gemini", "2.5", "pro"], "gemini-2.5-pro"),
    (&["gemini", "pro"], "gemini-2.5-pro"),
    (&["3.1"], "gemini-3.1-pro-preview"),
    (&["gemini", "3.1"], "gemini-3.1-pro-preview"),
    (&["3.1", "pro"], "gemini-3.1-pro-preview"),
    (&["gemini", "3.1", "pro"], "gemini-3.1-pro-preview"),
    // GLM
    (&["glm", "5.1"], "glm-5.1"),
    (&["glm", "5", "turbo"], "glm-5-turbo"),
    (&["glm", "5v"], "glm-5v-turbo"),
    (&["glm", "5"], "glm-5"),
    (&["glm", "4.7"], "glm-4.7"),
    (&["glm", "turbo"], "glm-5-turbo"),
    // MiniMax — direct API format
    (&["minimax", "m2.7", "fast"], "MiniMax-M2.7-highspeed"),
    (&["minimax", "m2.7"], "MiniMax-M2.7"),
    (&["minimax", "m2.5", "fast"], "MiniMax-M2.5-highspeed"),
    (&["minimax", "m2.5"], "MiniMax-M2.5"),
    (&["minimax", "m2.1"], "MiniMax-M2.1"),
    (&["minimax", "m2"], "MiniMax-M2"),
    (&["minimax", "text"], "MiniMax-Text-01"),
    // MiniMax via OpenRouter
    (&["or", "minimax", "2.7"], "minimax/minimax-text-01"),
    (
        &["or", "minimax", "2.5", "free"],
        "minimax/minimax-text-01:free",
    ),
    (&["or", "minimax"], "minimax/minimax-text-01"),
    // OpenAI via OpenRouter
    (&["or", "o3"], "openai/o3"),
    (&["or", "gpt", "4.1"], "openai/gpt-4.1"),
    (&["or", "o4", "mini"], "openai/o4-mini"),
    // Anthropic via OpenRouter
    (&["or", "opus"], "anthropic/claude-opus-4-6"),
    (&["or", "sonnet"], "anthropic/claude-sonnet-4-6"),
    (&["or", "haiku"], "anthropic/claude-haiku-4-5-20251001"),
    // Google via OpenRouter
    (&["or", "gemini", "pro"], "google/gemini-2.5-pro"),
    (&["or", "gemini", "flash"], "google/gemini-2.5-flash"),
    (&["or", "gemini"], "google/gemini-2.5-pro"),
    // DeepSeek via OpenRouter
    (&["or", "r1"], "deepseek/deepseek-r1"),
    (&["or", "deepseek"], "deepseek/deepseek-chat"),
    // Grok
    (&["grok", "2"], "grok-2-latest"),
    (&["grok", "3"], "grok-3"),
];

/// Normalize a free-form model name into a wire-format model id.
/// If the input matches a known alias (all fragments present,
/// case-insensitive), return the canonical id. Otherwise return
/// the input with spaces replaced by hyphens.
pub fn normalize_model_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    for (fragments, canonical) in MODEL_ALIASES {
        if fragments
            .iter()
            .all(|f| words.iter().any(|w| w.contains(f)))
        {
            return canonical.to_string();
        }
    }

    // Fallback: replace spaces with hyphens (e.g. "deepseek chat" → "deepseek-chat")
    words.join("-")
}

impl ReplCommand {
    /// Pure parser — no I/O, no global state — so we can unit test it
    /// without dragging rustyline or a real agent into the picture.
    pub(super) fn parse(line: &str) -> Self {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Self::Empty;
        }
        // `! command` — shell escape
        if let Some(rest) = trimmed.strip_prefix('!') {
            let cmd = rest.trim();
            if !cmd.is_empty() {
                return Self::Shell(cmd.to_string());
            }
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            // Just the head word — `/cost extra junk` should still be
            // recognised as `/cost` so we don't punish typos with a hard
            // error. The tail is ignored in v0.5; later sessions can
            // teach individual commands to take arguments.
            let mut parts = rest.split_whitespace();
            let head = parts.next().unwrap_or("");
            return match head {
                "exit" | "quit" => Self::Exit,
                "clear" => Self::Clear,
                "cost" => match rest.trim() {
                    "off" => Self::CostOff,
                    "on" => Self::CostOn,
                    _ => Self::Cost,
                },
                "help" | "?" => Self::Help,
                "" => Self::SlashMenu,
                "fork" => {
                    // `/fork` → copy entire transcript with auto-id
                    // `/fork N` → copy first N messages with auto-id
                    // `/fork name` → copy entire transcript with custom name
                    // `/fork name N` → copy first N messages with custom name
                    let mut name: Option<String> = None;
                    let mut take: Option<usize> = None;
                    if let Some(arg1) = parts.next() {
                        if let Ok(n) = arg1.parse::<usize>() {
                            take = Some(n);
                        } else {
                            name = Some(arg1.to_string());
                            if let Some(arg2) = parts.next() {
                                take = arg2.parse::<usize>().ok();
                            }
                        }
                    }
                    Self::Fork { name, take }
                }
                "sessions" => Self::Sessions,
                "tree" => Self::Tree,
                "session" => Self::SessionInfo,
                "update" => Self::Update,
                "stats" => Self::Stats,
                "overthink" => Self::Overthink,
                "plan" => Self::Plan,
                "compact" => Self::Compact,
                "dag" => Self::Dag,
                "budget" => Self::Budget,
                "map" => {
                    let max = parts.next().and_then(|s| s.parse::<usize>().ok());
                    Self::Map(max)
                }
                "insights" => Self::Insights,
                "learn" => {
                    let text = parts.collect::<Vec<_>>().join(" ");
                    Self::Learn(text)
                }
                "export-ft" => {
                    let path = parts.next().map(|s| s.to_string());
                    Self::ExportFt(path)
                }
                "advisor" => {
                    let arg = parts.next();
                    match arg {
                        Some("off") => Self::AdvisorOff,
                        None => Self::Advisor,
                        _ => Self::Advisor,
                    }
                }
                "autotune" => Self::AutotuneToggle,
                "security" => {
                    let sub = parts.next().map(|s| s.to_string());
                    Self::Security(sub)
                }
                "multi-model" => Self::MultiModelToggle,
                "perturbation" => Self::PerturbationToggle,
                "parallel" => Self::ParallelToggle,
                "api-keys" => Self::ApiKeysToggle,
                "godmode" => Self::GodmodeToggle,
                "browser" => Self::Browser,
                "computer" => Self::Computer,
                "skills" => Self::Skills,
                "skill-install" => match parts.next() {
                    Some(source) => Self::SkillInstall(source.to_string()),
                    None => Self::Unknown("skill-install".to_string()),
                },
                "skill-uninstall" => match parts.next() {
                    Some(name) => Self::SkillUninstall(name.to_string()),
                    None => Self::Unknown("skill-uninstall".to_string()),
                },
                "skill-search" => {
                    let query: String = parts.collect::<Vec<_>>().join(" ");
                    if query.is_empty() {
                        Self::Unknown("skill-search".to_string())
                    } else {
                        Self::SkillSearch(query)
                    }
                }
                "learn-skill" => {
                    let name = parts.next().map(|s| s.to_string());
                    Self::LearnSkill(name)
                }
                "skill-rate" => match parts.next() {
                    Some(name) => match parts.next() {
                        Some("good") => Self::SkillRate {
                            name: name.to_string(),
                            good: true,
                        },
                        Some("bad") => Self::SkillRate {
                            name: name.to_string(),
                            good: false,
                        },
                        _ => Self::Unknown("skill-rate".to_string()),
                    },
                    None => Self::Unknown("skill-rate".to_string()),
                },
                "skill-improve" => match parts.next() {
                    Some(name) => Self::SkillImprove(name.to_string()),
                    None => Self::Unknown("skill-improve".to_string()),
                },
                "resume" => match parts.next() {
                    Some(id) => Self::Resume(id.to_string()),
                    None => Self::Unknown("resume".to_string()),
                },
                "provider" => match parts.next() {
                    Some(name) => Self::ProviderSwitch(name.to_string()),
                    None => Self::Unknown("provider".to_string()),
                },
                "key" => match parts.next() {
                    Some(env_var) => {
                        let value = parts.collect::<Vec<_>>().join(" ");
                        if value.is_empty() {
                            Self::Unknown("key".to_string())
                        } else {
                            Self::SetKey {
                                env_var: env_var.to_string(),
                                value,
                            }
                        }
                    }
                    None => Self::Unknown("key".to_string()),
                },
                "providers" => Self::Providers,
                "model" => {
                    let raw: String = parts.collect::<Vec<_>>().join(" ");
                    if raw.trim().is_empty() {
                        Self::ModelMenu
                    } else {
                        Self::ModelSwitch(normalize_model_name(&raw))
                    }
                }
                "models" => Self::ModelMenu,
                "swarm" => {
                    let rest: String = parts.collect::<Vec<_>>().join(" ");
                    if rest.trim().is_empty() {
                        Self::Unknown("swarm".to_string())
                    } else {
                        // Parse: /swarm [N] [quorum:M] <prompt>
                        // N and quorum: can appear in any order before the prompt text.
                        let mut words = rest.split_whitespace().peekable();
                        let mut n: usize = 3;
                        let mut quorum: usize = 0; // 0 = no quorum required
                        let mut consumed = 0usize;
                        // Try to consume up to 2 leading tokens as N or quorum:M
                        for _ in 0..2 {
                            let w = match words.peek() {
                                Some(w) => *w,
                                None => break,
                            };
                            if let Some(m_str) = w.strip_prefix("quorum:") {
                                if let Ok(m) = m_str.parse::<usize>() {
                                    quorum = m;
                                    words.next();
                                    consumed += w.len() + 1;
                                    continue;
                                }
                            }
                            if let Ok(parsed_n) = w.parse::<usize>() {
                                if (2..=10).contains(&parsed_n) {
                                    n = parsed_n;
                                    words.next();
                                    consumed += w.len() + 1;
                                    continue;
                                } else {
                                    eprintln!("[aegis] swarm: N must be 2-10, got {parsed_n}");
                                    break;
                                }
                            }
                            break;
                        }
                        let _ = consumed; // consumed used for clarity only
                        let prompt = words.collect::<Vec<_>>().join(" ");
                        if prompt.is_empty() {
                            Self::Unknown("swarm".to_string())
                        } else {
                            // quorum=0 means "no quorum" → show all results
                            // quorum > n → clamp to n
                            let effective_quorum = quorum.min(n);
                            Self::Swarm {
                                prompt,
                                n,
                                quorum: effective_quorum,
                            }
                        }
                    }
                }
                // `/glm <prompt>` and `/consult <provider> <prompt>`
                "glm" => {
                    let prompt: String = parts.collect::<Vec<_>>().join(" ");
                    if prompt.trim().is_empty() {
                        Self::Unknown("glm".to_string())
                    } else {
                        Self::Consult {
                            provider: "glm".to_string(),
                            prompt,
                        }
                    }
                }
                "consult" => match parts.next() {
                    Some(prov) => {
                        let prompt: String = parts.collect::<Vec<_>>().join(" ");
                        if prompt.trim().is_empty() {
                            Self::Unknown("consult".to_string())
                        } else {
                            Self::Consult {
                                provider: prov.to_string(),
                                prompt,
                            }
                        }
                    }
                    None => Self::Unknown("consult".to_string()),
                },
                "race" => {
                    let prompt: String = parts.collect::<Vec<_>>().join(" ");
                    if prompt.trim().is_empty() {
                        Self::Unknown("race".to_string())
                    } else {
                        Self::Race(prompt)
                    }
                }
                "image" => {
                    let raw: String = parts.collect::<Vec<_>>().join(" ");
                    if raw.trim().is_empty() {
                        Self::Unknown("image".to_string())
                    } else {
                        Self::Image(raw)
                    }
                }
                "images" => match parts.next() {
                    Some("clear") => Self::ImagesClear,
                    _ => Self::Images,
                },
                "paste" => Self::Paste,
                "btw" => {
                    let text: String = parts.collect::<Vec<_>>().join(" ");
                    if text.trim().is_empty() {
                        Self::Unknown("btw".to_string())
                    } else {
                        Self::Btw(text)
                    }
                }
                "files" => {
                    let path = parts.next().map(|s| s.to_string());
                    Self::Files(path)
                }
                "view" => match parts.next() {
                    Some(path) => Self::View(path.to_string()),
                    None => Self::Unknown("view".to_string()),
                },
                "search" => {
                    let pattern: String = parts.collect::<Vec<_>>().join(" ");
                    if pattern.is_empty() {
                        Self::Unknown("search".to_string())
                    } else {
                        Self::Search(pattern)
                    }
                }
                "tasks" => Self::Tasks,
                "task" => match parts.next() {
                    Some("add") => {
                        let text: String = parts.collect::<Vec<_>>().join(" ");
                        if text.is_empty() {
                            Self::Unknown("task add".to_string())
                        } else {
                            Self::TaskAdd(text)
                        }
                    }
                    Some("done") => match parts.next().and_then(|s| s.parse::<u32>().ok()) {
                        Some(id) => Self::TaskDone(id),
                        None => Self::Unknown("task done".to_string()),
                    },
                    Some("rm") | Some("remove") | Some("delete") => {
                        match parts.next().and_then(|s| s.parse::<u32>().ok()) {
                            Some(id) => Self::TaskRm(id),
                            None => Self::Unknown("task rm".to_string()),
                        }
                    }
                    Some("clear") => Self::TaskClear,
                    Some("list") | None => Self::Tasks,
                    Some(other) => Self::Unknown(format!("task {other}")),
                },
                "context" => Self::Context,
                "tokens" => Self::Tokens,
                "history" => Self::History,
                "copy" => Self::Copy,
                other => Self::Unknown(other.to_string()),
            };
        }
        Self::Prompt(trimmed.to_string())
    }
}

/// Help text printed by `/help`. Lifted out so the test that asserts
/// every command appears in it can scan a static string.
pub(super) const HELP_TEXT: &str = "\
  /help        show this list
  /cost        print cumulative tokens + dollars for this REPL session
  /clear       wipe the conversation and start a fresh session
  /fork [name] [n]  branch the conversation (optional name, keep first n messages)
  /session     show info about the current session (id, parent, children)
  /tree        show the session branch tree
  /update      check for and install updates
  /stats       show usage telemetry dashboard (cost, tokens, tools)
  /overthink   toggle extended thinking mode (chain-of-thought before answer)
  /plan        toggle plan mode (read-only tools only, no mutations)
  /skills      list available skills (invoke with /<skill-name> [args])
  /compact     force an immediate transcript compaction
  /skill-install <source>  install skills from git repo, URL, or local path
  /skill-uninstall <name>  remove an installed skill
  /skill-search <query>    search skills by name, description, or tag
  /learn-skill [name]      extract a reusable skill from the current session
  /skill-rate <name> good|bad  record success or failure for a skill
  /skill-improve <name>    ask LLM to rewrite an underperforming skill prompt
  /providers   list all available providers and their status
  /provider <name>  switch provider (deepseek, openai, grok, glm, gemini, anthropic, etc.)
  /key <ENV_VAR> <value>  set an API key at runtime (e.g. /key OPENAI_API_KEY sk-...)
  /model <name>     switch model without changing provider
  /swarm [N] [quorum:M] <prompt>  run N parallel agents; require M agreements for consensus (default N=3, no quorum)
  /glm <prompt>    consult GLM model (side query, result injected into context)
  /consult <provider> <prompt>  consult any provider as a side query
  /race <prompt>   query 3 strong models in parallel, inject best response into context
  /browser     attach Playwright MCP (browser automation) for this session
  /computer    attach open-computer-use MCP (full OS control) for this session
  /image <path>  attach an image to the next prompt (repeat for multiple)
  /images      list attached images (/images clear to remove all)
  /files [path] browse project files and directories with interactive tree view
  /view <path>  preview file contents with syntax highlighting
  /search <pattern> search files for text pattern with grep-like interface
  /tasks       list your tasks
  /task add <text>  add a new task
  /task done <id>   mark a task as done
  /task rm <id>     delete a task
  /task clear       remove all completed tasks
  /dag         show tool-call chain for this session as an ASCII tree
  /map [N]     print the tree-sitter repo map (scan up to N files, default 200)
  /budget      show today's spend (prior sessions + current) vs configured daily budget
  /autotune    toggle adaptive temperature on/off for this session
  /security    show autonomous security status (limits, counters, kill switch)
  /security kill    trigger kill switch — stop all autonomous tool calls
  /security resume  resume after kill switch
  /insights    extract non-obvious learnings from session and save to memory
  /learn <text>  immediately save a rule or insight to memory
  /export-ft [file]  export session as OpenAI fine-tuning JSONL (default: ft_export.jsonl)
  /advisor     switch to architecture review mode (mutating tools blocked)
  /advisor off exit advisor mode and restore full tool access
  /multi-model toggle multi-model evaluation (GODMODE)
  /perturbation toggle prompt perturbation (GODMODE)
  /parallel    toggle parallel models (GODMODE)
  /api-keys    toggle API key management (GODMODE)
  /godmode     toggle all GODMODE features at once
  /context    show session context summary (system prompt, recent messages, etc.)
  /tokens     show detailed token usage breakdown
  /history    show recent message history
  /copy       copy last assistant message to system clipboard (OSC 52)
  /btw <note>  drop a context note into the session without a reply
  /sessions    list every session stored under this workspace
  /resume <id> switch the REPL to an existing session on disk
  /exit /quit  leave the REPL (Ctrl-D also works)
  ! <command>   run a shell command (e.g. ! git status)
Anything else is sent to the model as your next message.
Tab completes slash commands and file paths.";

/// Print coloured help text. Commands in cyan, descriptions dim.
pub(super) fn print_help() {
    eprintln!("\x1b[1mmetis REPL\x1b[0m — slash commands:\n");
    for line in HELP_TEXT.lines() {
        if let Some(rest) = line.trim_start().strip_prefix('/') {
            // Find the boundary between command and description.
            if let Some(pos) = rest.find("  ") {
                let cmd = &rest[..pos];
                let desc = rest[pos..].trim();
                eprintln!("  \x1b[36m/{cmd}\x1b[0m  \x1b[2m{desc}\x1b[0m");
            } else {
                eprintln!("  \x1b[36m/{rest}\x1b[0m");
            }
        } else if line.trim_start().starts_with('!') {
            eprintln!("  \x1b[33m{}\x1b[0m", line.trim());
        } else if !line.trim().is_empty() {
            eprintln!("\x1b[2m{line}\x1b[0m");
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_model_name_opus() {
        assert_eq!(normalize_model_name("opus"), "claude-opus-4-6");
        assert_eq!(normalize_model_name("Opus"), "claude-opus-4-6");
        assert_eq!(normalize_model_name("or opus"), "claude-opus-4-6");
    }
}
