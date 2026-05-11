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
- [ ] Rust: multi_edit (atomik coklu duzenleme)
- [x] Rust: grep (regex destekli icerik arama, file filter)
- [x] Rust: glob (dosya/dizin bulma, .git/node_modules ignore)
- [x] Rust: bash (komut calistir, stdout/stderr capture, exit code)
- [ ] Rust: bash_background (arka plan surec, notify)
- [x] E2E: dosya yaz -> oku -> karsilastir
- [x] E2E: edit_file fuzzy match dogrulama
- [x] E2E: bash komut calistirma + timeout dogrulama

## Faz 5: Provider Katmani 🔧
- [x] Rust: DeepSeek provider (OpenAI-compatible)
- [ ] Rust: NVIDIA NIM provider
- [ ] Rust: Anthropic provider
- [ ] Rust: GLM provider
- [ ] Rust: auto-routing (fast/strong/vision)
- [x] Rust: cost tracking (token sayimi + fiyat hesabi, deepseek/gpt/claude pricing)
- [ ] Rust: credential pooling (coklu API key rotasyonu)
- [ ] E2E: her provider'a gercek API call
- [ ] E2E: auto-routing karar dogrulama
- [x] E2E: cost tracking dogrulama [vitest]

## Faz 6: Session Sistemi ✅
- [x] Rust: session store (SQLite)
- [x] Rust: session resume (session_switch Tauri komutu)
- [x] Rust: FTS5 tam metin arama (session_search)
- [ ] Rust: session export (JSONL dosyaya yaz)
- [x] Frontend: sidebar'da session listesi
- [x] E2E: session olustur -> kapat -> resume -> mesajlar ayni

## Faz 7: Web + Browser Tool'lari 🔧
- [x] Rust: web_search (DuckDuckGo scraping)
- [x] Rust: web_fetch (URL icerik cekme, JSON detect, 15K truncate)
- [ ] Rust: browser_navigate, click, type, scroll, snapshot, press
- [ ] Rust: browser_vision (screenshot + LLM analiz)
- [ ] Rust: browser_console (JS calistir/okuma)
- [ ] E2E: browser ac -> tikla -> sonuc dogrula

## Faz 8: Cron Sistemi ✅
- [x] Rust: job scheduler (schedule parse - 5-field cron, */step, range, list)
- [x] Rust: agent mode (prompt + agent loop ile calistir)
- [x] Rust: script mode (bash ile calistir, stdout/stderr capture)
- [x] Rust: cron_jobs SQLite tablosu (id, schedule, prompt, mode, enabled, last_run, run_count, last_output, last_error)
- [x] Rust: Tauri komutlari (cron_add, cron_list, cron_get, cron_delete, cron_toggle, cron_run_now)
- [x] Rust: background scheduler (60sn interval, tokio::spawn)
- [x] Test: 11 cron parser + scheduler testi gecti

## Faz 9: Diger Tool'lar
- [ ] delegation (delegate_task)
- [ ] git (status, diff, commit, log, pr_create)
- [ ] vision (vision_analyze)
- [ ] tts (text_to_speech)
- [ ] skills (skill_list, view, manage)
- [ ] mcp (MCP client)
- [ ] obsidian (vault read/write/search)
- [ ] peer (CC inter-agent)
- [ ] premortem (risk analysis)
- [ ] eisenhower (matrix)

## Faz 10: UI Polish + Shipping
- [ ] Command palette (/map, /cost, /sessions)
- [ ] Syntax highlighting in output panel
- [ ] Goblin sprite animation (CSS keyframes)
- [ ] Keyboard shortcuts
- [ ] Glass morphism effects
- [ ] Tauri bundle (macOS .dmg)
