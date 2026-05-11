# Goblin - Build Phases

## Faz 1: Foundation
- [x] Tauri + React + TypeScript iskeleti
- [x] Dark theme UI layout (sol: chat + karakter, sag: output)
- [x] Goblin character strip (state-based emoji, placeholder)
- [x] Chat input + mesaj baloncukları
- [x] Status bar (model, turn, cost)
- [ ] Zustand store'ları (chatStore, agentStore)
- [ ] Tauri IPC: frontend <-> Rust backend baglantisi
- [ ] E2E test altyapisi (vitest + playwright)

## Faz 2: Agent Motoru
- [x] Rust: provider trait + OpenAI-compatible implementasyon
- [x] Rust: core conversation loop (LLM call -> tool dispatch -> result -> continue)
- [x] Rust: system prompt builder
- [x] Rust: context window management (token estimation + trim)
- [x] Rust: tool dispatch framework (tool registry stub)
- [ ] Frontend: useAgent hook (Tauri IPC ile agent loop)
- [ ] E2E: agent loop tam tur testi (prompt -> LLM -> tool -> sonuc)

## Faz 3: Memory + Mnemonics (Native)
- [ ] Rust: SQLite schema (memories, observations, learned, sessions, jobs)
- [ ] Rust: auto-observe (her tool call observation'a yazilir)
- [ ] Rust: auto-inject (her turn relevant memory'ler system prompt'a eklenir)
- [ ] Rust: reinforcement (tool reddi -> learned tablosu)
- [ ] Rust: compact policy (30 gun erisilmeyen tier-1 arsiv)
- [ ] Rust: per-proje scope (.goblin/ klasoru)
- [ ] Rust: mnemonics_add, mnemonics_retrieve, mnemonics_observe, mnemonics_learn
- [ ] E2E: memory write -> read -> inject dogrulama
- [ ] E2E: observation otomatik kayit dogrulama
- [ ] E2E: reinforcement sayaci dogrulama

## Faz 4: Dosya + Shell Tool'lari
- [ ] Rust: read_file (satir numarali, offset/limit)
- [ ] Rust: write_file (tam uzerine yazma)
- [ ] Rust: edit_file (fuzzy match, tek nokta degisiklik)
- [ ] Rust: multi_edit (atomik coklu duzenleme)
- [ ] Rust: grep (ripgrep destekli icerik arama)
- [ ] Rust: glob (dosya/dizin bulma)
- [ ] Rust: bash (komut calistir, timeout, sandbox)
- [ ] Rust: bash_background (arka plan surec, notify)
- [ ] E2E: dosya yaz -> oku -> karsilastir
- [ ] E2E: edit_file fuzzy match dogrulama
- [ ] E2E: bash komut calistirma + timeout dogrulama

## Faz 5: Provider Katmani
- [ ] Rust: DeepSeek provider (OpenAI-compatible) -- done in Faz 2
- [ ] Rust: NVIDIA NIM provider
- [ ] Rust: Anthropic provider
- [ ] Rust: GLM provider
- [ ] Rust: auto-routing (fast/strong/vision)
- [ ] Rust: cost tracking (token sayimi + fiyat hesabi)
- [ ] Rust: credential pooling (coklu API key rotasyonu)
- [ ] E2E: her provider'a gercek API call
- [ ] E2E: auto-routing karar dogrulama
- [ ] E2E: cost tracking dogrulama

## Faz 6: Session Sistemi
- [ ] Rust: session store (SQLite)
- [ ] Rust: session resume (--resume, --continue)
- [ ] Rust: FTS5 tam metin arama (session_search)
- [ ] Rust: session export (JSONL)
- [ ] Frontend: sidebar'da session listesi
- [ ] E2E: session olustur -> kapat -> resume -> mesajlar ayni

## Faz 7: Web + Browser Tool'lari
- [ ] Rust: web_search (arama motoru)
- [ ] Rust: web_fetch (URL icerik cekme)
- [ ] Rust: browser_navigate, click, type, scroll, snapshot, press
- [ ] Rust: browser_vision (screenshot + LLM analiz)
- [ ] Rust: browser_console (JS calistir/okuma)
- [ ] E2E: browser ac -> tikla -> sonuc dogrula

## Faz 8: Cron Sistemi
- [ ] Rust: job scheduler (schedule parse)
- [ ] Rust: agent mode (prompt + skills yukle, calistir)
- [ ] Rust: script mode (no_agent=true, stdout direkt)

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
