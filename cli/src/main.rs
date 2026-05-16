mod agent;
mod config;
mod context;
mod tools;

use clap::Parser;
use std::io::{self, BufRead, IsTerminal, Write};

#[derive(Parser)]
#[command(
    name = "goblin",
    about = "Goblin AI — standalone terminal agent",
    long_about = "Goblin AI agent in your terminal. No app needed.\n\n\
                  Examples:\n  \
                  goblin \"fix the bug in main.rs\"\n  \
                  cat error.log | goblin \"what went wrong?\"\n  \
                  goblin   (interactive REPL)"
)]
struct Args {
    /// Message (omit for interactive REPL or stdin pipe)
    text: Vec<String>,

    /// Model override (e.g. deepseek-v4-pro, claude-sonnet-4-6)
    #[arg(short, long)]
    model: Option<String>,

    /// Working directory (defaults to current dir)
    #[arg(short, long)]
    cwd: Option<String>,

    /// Print config path and resolved provider, then exit
    #[arg(long)]
    check: bool,
}

fn load_soul() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join(".goblin").join("soul.md");
    std::fs::read_to_string(path).ok()
}

fn build_system(cwd: &str, soul: Option<&str>) -> String {
    let proj_ctx = context::build_project_context(cwd);
    let base = format!(
        "You are Goblin, an AI coding assistant running in the terminal. \
         You have access to tools: bash, read_file, write_file, edit_file, grep, glob. \
         Use them to complete tasks. Be concise and direct.\n\n{}",
        proj_ctx
    );
    match soul {
        Some(s) if !s.is_empty() => format!("{}\n\n---\n\n{}", base, s),
        _ => base,
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let cfg = config::Config::load();

    let cwd = args.cwd
        .unwrap_or_else(|| std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string()));

    if args.check {
        let home = std::env::var("HOME").unwrap_or_default();
        let cfg_path = format!("{}/.goblin/config.toml", home);
        eprintln!("Config: {}", cfg_path);
        eprintln!("Exists: {}", std::path::Path::new(&cfg_path).exists());
        eprintln!("Default model: {}", cfg.agent.default_model);
        let check_hint = args.model.as_deref().unwrap_or(&cfg.agent.default_model).to_string();
        match cfg.resolve_provider(Some(&check_hint)) {
            Some(p) => eprintln!("Provider: {} @ {}", p.model, p.base_url),
            None => eprintln!("Provider: NONE — check config.toml"),
        }
        return;
    }

    let model_hint = args.model.as_deref().unwrap_or(&cfg.agent.default_model).to_string();
    let provider = match cfg.resolve_provider(Some(&model_hint)) {
        Some(p) => p,
        None => {
            eprintln!("No provider configured.");
            eprintln!("Create ~/.goblin/config.toml:");
            eprintln!();
            eprintln!("[providers.openai]");
            eprintln!("api_key = \"sk-...\"");
            eprintln!("base_url = \"https://api.deepseek.com/v1\"");
            eprintln!("models = [\"deepseek-v4-flash\"]");
            std::process::exit(1);
        }
    };

    let soul = load_soul();
    let system = build_system(&cwd, soul.as_deref());

    // Single-shot: args provided
    if !args.text.is_empty() {
        let message = args.text.join(" ");
        let mut ag = agent::Agent::new(provider, cfg.agent.max_tool_rounds, cwd, Some(system));
        match ag.send(&message).await {
            Ok(_) => {}
            Err(e) => { eprintln!("\x1b[31mError: {}\x1b[0m", e); std::process::exit(1); }
        }
        return;
    }

    // Pipe mode: stdin is not a terminal
    if !io::stdin().is_terminal() {
        let mut piped = String::new();
        for line in io::stdin().lock().lines() {
            piped.push_str(&line.expect("stdin read error"));
            piped.push('\n');
        }
        let message = piped.trim().to_string();
        if message.is_empty() { return; }
        let mut ag = agent::Agent::new(provider, cfg.agent.max_tool_rounds, cwd, Some(system));
        match ag.send(&message).await {
            Ok(_) => {}
            Err(e) => { eprintln!("\x1b[31mError: {}\x1b[0m", e); std::process::exit(1); }
        }
        return;
    }

    // Interactive REPL — persistent conversation
    println!("\x1b[1mGoblin\x1b[0m \x1b[2m{}\x1b[0m", cwd);
    println!("\x1b[2mModel: {}  |  type 'exit' to quit\x1b[0m\n", cfg.agent.default_model);

    let mut ag = agent::Agent::new(provider, cfg.agent.max_tool_rounds, cwd, Some(system));

    loop {
        print!("\x1b[1;32m❯\x1b[0m ");
        io::stdout().flush().ok();

        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) => break,
            Err(e) => { eprintln!("Read error: {}", e); break; }
            _ => {}
        }

        let message = line.trim().to_string();
        if message.is_empty() { continue; }
        if message == "exit" || message == "quit" || message == ":q" { break; }

        println!();
        if let Err(e) = ag.send(&message).await {
            eprintln!("\x1b[31mError: {}\x1b[0m", e);
        }
        println!();
    }
}
