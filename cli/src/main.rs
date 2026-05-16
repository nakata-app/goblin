use clap::Parser;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, IsTerminal, Write};

#[derive(Parser)]
#[command(name = "goblin", about = "Goblin AI terminal client")]
struct Args {
    /// Mesaj metni (verilmezse stdin veya interaktif mod)
    text: Vec<String>,

    /// Model seç (ör: deepseek-ai/deepseek-v4-pro)
    #[arg(short, long)]
    model: Option<String>,

    /// Goblin HTTP adresi (varsayılan: config.toml'dan)
    #[arg(long)]
    host: Option<String>,

    /// Goblin HTTP portu
    #[arg(long)]
    port: Option<u16>,

    /// Bearer token (varsayılan: config.toml'dan, ya da GOBLIN_TOKEN env)
    #[arg(long)]
    token: Option<String>,
}

#[derive(Deserialize, Default)]
struct GoblinConfig {
    #[serde(default)]
    http: HttpSection,
}

#[derive(Deserialize, Default)]
struct HttpSection {
    #[serde(default)]
    bind: String,
    #[serde(default)]
    token: String,
}

#[derive(Serialize)]
struct MessageReq {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Deserialize)]
struct MessageResp {
    content: String,
    model: String,
    tokens_in: u32,
    tokens_out: u32,
}

fn load_config() -> (String, u16, String) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{}/.goblin/config.toml", home);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let cfg: GoblinConfig = toml::from_str(&raw).unwrap_or_default();

    let (host, port) = if cfg.http.bind.is_empty() {
        ("127.0.0.1".into(), 1789u16)
    } else {
        let mut parts = cfg.http.bind.splitn(2, ':');
        let h = parts.next().unwrap_or("127.0.0.1").to_string();
        let p = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1789);
        (h, p)
    };

    (host, port, cfg.http.token)
}

fn send(
    client: &reqwest::blocking::Client,
    host: &str,
    port: u16,
    token: &str,
    text: String,
    model: Option<String>,
) -> Result<MessageResp, String> {
    let url = format!("http://{}:{}/message", host, port);
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&MessageReq { text, model })
        .send()
        .map_err(|e| format!("Bağlantı hatası: {} (Goblin açık mı?)", e))?;

    let status = resp.status();
    if status == 401 {
        return Err("401 Unauthorized — token yanlış".into());
    }
    if status == 503 {
        return Err("503 — Goblin başlatılmış ama provider ayarlı değil".into());
    }
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body));
    }

    resp.json::<MessageResp>()
        .map_err(|e| format!("Yanıt parse hatası: {}", e))
}

fn main() {
    let args = Args::parse();

    let (cfg_host, cfg_port, cfg_token) = load_config();

    let host = args.host.unwrap_or(cfg_host);
    let port = args.port.unwrap_or(cfg_port);
    let token = args.token
        .or_else(|| std::env::var("GOBLIN_TOKEN").ok())
        .unwrap_or(cfg_token);

    if token.is_empty() {
        eprintln!("Hata: token bulunamadı. ~/.goblin/config.toml içinde [http] token = \"...\" ekle veya GOBLIN_TOKEN ortam değişkeni kullan.");
        std::process::exit(1);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("http client");

    // 1) Argümandan metin geldiyse tek seferlik gönder
    if !args.text.is_empty() {
        let text = args.text.join(" ");
        match send(&client, &host, port, &token, text, args.model) {
            Ok(r) => {
                println!("{}", r.content);
                eprintln!("\x1b[2m[{}  ↑{}  ↓{}]\x1b[0m", r.model, r.tokens_in, r.tokens_out);
            }
            Err(e) => { eprintln!("Hata: {}", e); std::process::exit(1); }
        }
        return;
    }

    // 2) Pipe ile stdin geldiyse oku, interaktif değil
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        let mut piped = String::new();
        for line in stdin.lock().lines() {
            piped.push_str(&line.expect("stdin okuma hatası"));
            piped.push('\n');
        }
        let text = piped.trim().to_string();
        if text.is_empty() { return; }

        match send(&client, &host, port, &token, text, args.model) {
            Ok(r) => {
                println!("{}", r.content);
                eprintln!("\x1b[2m[{}  ↑{}  ↓{}]\x1b[0m", r.model, r.tokens_in, r.tokens_out);
            }
            Err(e) => { eprintln!("Hata: {}", e); std::process::exit(1); }
        }
        return;
    }

    // 3) İnteraktif REPL modu
    println!("Goblin REPL — {}:{}", host, port);
    println!("Çıkmak için: exit veya Ctrl+C\n");

    let stdout = io::stdout();
    loop {
        print!(">>> ");
        stdout.lock().flush().ok();

        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Err(e) => { eprintln!("Okuma hatası: {}", e); break; }
            _ => {}
        }

        let text = line.trim().to_string();
        if text.is_empty() { continue; }
        if text == "exit" || text == "quit" { break; }

        match send(&client, &host, port, &token, text, args.model.clone()) {
            Ok(r) => {
                println!("\n{}\n", r.content);
                eprintln!("\x1b[2m[{}  ↑{}  ↓{}]\x1b[0m", r.model, r.tokens_in, r.tokens_out);
            }
            Err(e) => eprintln!("Hata: {}", e),
        }
    }
}
