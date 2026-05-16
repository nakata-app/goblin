# Goblin - Build Phases

## Faz 1: Foundation
- [x] Tauri + React + TypeScript iskeleti
- [x] Dark theme UI layout (sol: chat + karakter, sag: output)
- [x] Goblin character strip (state-based emoji, placeholder)
- [x] Chat input + mesaj baloncukları
- [x] Status bar (model, turn, cost)
- [x] Zustand store'ları (chatStore, agentStore, sessionStore)
- [x] Tauri IPC: frontend <-> Rust backend baglantisi
- [x] E2E test altyapisi (vitest + playwright)

## Faz 2: Agent Motoru
- [x] Rust: provider trait + OpenAI-compatible implementasyon
- [x] Rust: core conversation loop (LLM call -> tool dispatch -> result -> continue)
- [x] Rust: system prompt builder
- [x] Rust: context window management (token estimation + trim)
- [x] Rust: tool dispatch framework (tool registry stub)
- [x] Frontend: useAgent hook (Tauri IPC ile agent loop)
- [x] E2E: agent loop tam tur testi (prompt -> LLM -> tool -> sonuc) [22 tests, vitest mock]

## Faz 3: Memory + Mnemonics (Native) ✅
- [x] Rust: SQLite schema (memories, observations, learned, sessions, FTS5)
- [x] Rust: auto-observe (her tool call observation'a yazilir)
- [x] Rust: auto-inject (her turn relevant memory'ler system prompt'a eklenir)
- [x] Rust: reinforcement (tool reddi -> learned tablosu)
- [x] Rust: compact policy (30 gun erisilmeyen tier-1 arsiv)
- [x] Rust: per-proje scope (.goblin/ klasoru)
- [x] Rust: mnemonics_add, mnemonics_retrieve, mnemonics_observe, mnemonics_learn
- [x] E2E: memory write -> read -> inject dogrulama
- [x] E2E: observation otomatik kayit dogrulama
- [x] E2E: reinforcement sayaci dogrulama

## Faz 4: Dosya + Shell Tool'lari ✅
- [x] Rust: read_file (satir numarali, offset/limit)
- [x] Rust: write_file (tam uzerine yazma, parent dir create)
- [x] Rust: edit_file (fuzzy match, tek nokta degisiklik, replace-all)
- [x] Rust: multi_edit (atomik coklu duzenleme, rollback)
- [x] Rust: grep (regex destekli icerik arama, file filter)
- [x] Rust: glob (dosya/dizin bulma, .git/node_modules ignore)
- [x] Rust: bash (komut calistir, stdout/stderr capture, exit code)
- [x] Rust: bash_background (arka plan surec, check, kill)
- [x] E2E: dosya yaz -> oku -> karsilastir
- [x] E2E: edit_file fuzzy match dogrulama
- [x] E2E: bash komut calistirma + timeout dogrulama

## Faz 5: Provider Katmani ✅
- [x] Rust: DeepSeek provider (OpenAI-compatible)
- [x] Rust: NVIDIA NIM provider (OpenAI-compatible)
- [x] Rust: Anthropic provider (Messages API)
- [x] Rust: GLM provider (ZhipuAI, OpenAI-compatible)
- [x] Rust: Gemini provider (Google AI)
- [x] Rust: Generic provider (Ollama, vLLM, OpenRouter, Groq, Mistral, Together, Fireworks, Perplexity, xAI, LM Studio, LocalAI - her OpenAI-compatible endpoint)
- [x] Rust: auto-routing (fast/strong/vision, keyword-based heuristic + message length)
- [x] Rust: cost tracking (token sayimi + fiyat hesabi, deepseek/gpt/claude pricing)
- [x] Rust: credential pooling (coklu API key rotasyonu, key_pool + get_key_for_provider)
- [x] E2E: her provider'a gercek API call, openai.rs'de real_deepseek_v4_pro/flash testleri mevcut, #[ignore] ile calistir: cargo test -- --ignored
- [x] E2E: auto-routing karar dogrulama, config/mod.rs'de 12 unit test gecti (auto_route_*)
- [x] E2E: cost tracking dogrulama [vitest]

## Faz 6: Session Sistemi ✅
- [x] Rust: session store (SQLite)
- [x] Rust: session resume (session_switch Tauri komutu)
- [x] Rust: FTS5 tam metin arama (session_search)
- [x] Rust: session export (JSONL/kayit dosyaya yaz)
- [x] Frontend: sidebar'da session listesi
- [x] E2E: session olustur -> kapat -> resume -> mesajlar ayni

## Faz 7: Web + Browser Tool'lari ✅
- [x] Rust: web_search (DuckDuckGo scraping)
- [x] Rust: web_fetch (URL icerik cekme, JSON detect, 15K truncate)
- [x] Rust: browser_navigate, click, type, scroll, snapshot, press
- [x] Rust: browser_vision (screenshot PNG, base64 output)
- [x] Rust: browser_console (JS evaluate, await promise, 10K truncate)
- [x] E2E: browser ac -> tikla -> sonuc dogrula, browser.rs::browser_navigate_click_verify, #[ignore], cargo test browser_ -- --ignored (Chrome gerekli, gecti)

## Faz 8: Cron Sistemi ✅
- [x] Rust: job scheduler (schedule parse - 5-field cron, */step, range, list)
- [x] Rust: agent mode (prompt + agent loop ile calistir)
- [x] Rust: script mode (bash ile calistir, stdout/stderr capture)
- [x] Rust: cron_jobs SQLite tablosu (id, schedule, prompt, mode, enabled, last_run, run_count, last_output, last_error)
- [x] Rust: Tauri komutlari (cron_add, cron_list, cron_get, cron_delete, cron_toggle, cron_run_now)
- [x] Rust: background scheduler (60sn interval, tokio::spawn)
- [x] Test: 11 cron parser + scheduler testi gecti

## Faz 9: Diger Tool'lar ✅
- [x] delegation (delegate_task)
- [x] git (status, diff, commit, log, pr_create)
- [x] vision (vision_analyze)
- [x] tts (text_to_speech)
- [x] skills (skill_list, view, manage)
- [x] mcp (MCP client: connect, list_tools, call_tool, install)
- [x] obsidian (vault read/write/search/stats)
- [x] peer (CC inter-agent: send, broadcast, status, coordinate)
- [x] premortem (risk analysis)
- [x] eisenhower (matrix)
- [x] Test: 42 Rust test + 22 vitest = 64 ✅

## Faz 10: UI Polish + Shipping ✅
- [x] Command palette (/map, /cost, /sessions, /help, /shortcuts, /model, /premortem, /eisenhower, 15 komut, kategori gruplu)
- [x] Syntax highlighting in output panel (markdown render, code blocks, inline code, headings, tables, stderr/exit markers)
- [x] Goblin sprite animation (CSS keyframes: particle orbit, idle breathe, sparkle pulse, bounce, ring spin)
- [x] Keyboard shortcuts (⌘K palet, ⌘N yeni, ⌘⇧S sessions, ⌘⇧C kopyala, ⌘/ kisayollar, Esc kapat)
- [x] Glass morphism effects (backdrop-filter, 2 katman derinlik, border pulse, text glow)
- [x] Tauri bundle (macOS .dmg config, overlay title bar, traffic light pozisyonu, updater artifacts)
- [x] Test: 42 Rust + 22 vitest = 64 ✅

## Faz 11: System Tray Daemon ✅
- [x] Rust: daemon.rs, TrayIconBuilder + menu (Show/Hide/Status/Quit)
- [x] Rust: sol-klik toggle pencere goster/gizle
- [x] Rust: close = minimize to tray (CloseRequested -> prevent_close + hide)
- [x] Rust: tray-status-update event (frontend'e real-time durum aktarimi)
- [x] Cargo.toml: tray-icon + image-png feature
- [x] Test: 197 Rust + 62 vitest = 259 ✅

## Faz 12: Rekabet Analizi Eksikleri ✅
- [x] Provider streaming: Anthropic SSE, NVIDIA SSE, GLM SSE (OpenAI-compatible reuse)
- [x] MCP server mode: stdio JSON-RPC, tools/list, tools/call, initialize
- [x] Multi-agent routing: AgentProfile config, trigger-based dispatch, per-agent tools/model
- [x] Agent hierarchy: task depth tracking, parent_id, subtask tree builder, depth limit
- [x] Skill marketplace: builtin registry (11 skills), search by query/tags, publish manifest
- [x] Docker sandbox: sandbox_exec (isolated container), sandbox_list, memory/cpu/network limits
- [x] Windows support: shell (cmd /C fallback), voice record (ffmpeg), TTS playback (powershell)
