# Goblin - Build Phases

## Faz 1: Foundation (MEVCUT)
- [x] Tauri + React + TypeScript iskeleti
- [x] Dark theme UI layout (sol: chat + karakter, sag: output)
- [x] Goblin character strip (state-based emoji, placeholder)
- [x] Chat input + mesaj baloncukları
- [x] Status bar (model, turn, cost)
- [ ] Zustand store'ları (chatStore, agentStore)
- [ ] Tauri IPC: frontend <-> Rust backend bağlantısı
- [ ] E2E test altyapısı (vitest + playwright)

## Faz 2: Agent Motoru
- [ ] Rust: provider trait + OpenAI-compatible implementasyon
- [ ] Rust: core conversation loop (LLM call -> tool dispatch -> result -> continue)
- [ ] Rust: system prompt builder
- [ ] Rust: context window management (compression)
- [ ] Rust: tool dispatch framework (tool registry, schema, handler)
- [ ] Frontend: useAgent hook (Tauri IPC ile agent loop)
- [ ] E2E: agent loop tam tur testi (prompt -> LLM -> tool -> sonuç)

## Faz 3: Memory + Mnemonics (Native)
- [ ] Rust: SQLite schema (memories, observations, learned, sessions, jobs)
- [ ] Rust: auto-observe (her tool call observation'a yazılır)
- [ ] Rust: auto-inject (her turn relevant memory'ler system prompt'a eklenir)
- [ ] Rust: reinforcement (tool reddi -> learned tablosu)
- [ ] Rust: compact policy (30 gün erişilmeyen tier-1 arşiv)
- [ ] Rust: per-proje scope (.goblin/ klasörü)
- [ ] Rust: mnemonics_add, mnemonics_retrieve, mnemonics_observe, mnemonics_learn
- [ ] E2E: memory write -> read -> inject doğrulama
- [ ] E2E: observation otomatik kayıt doğrulama
- [ ] E2E: reinforcement sayacı doğrulama

## Faz 4: Dosya + Shell Tool'ları
- [ ] Rust: read_file (satır numaralı, offset/limit)
- [ ] Rust: write_file (tam üzerine yazma)
- [ ] Rust: edit_file (fuzzy match, tek nokta değişiklik)
- [ ] Rust: multi_edit (atomik çoklu düzenleme)
- [ ] Rust: grep (ripgrep destekli içerik arama)
- [ ] Rust: glob (dosya/dizin bulma)
- [ ] Rust: bash (komut çalıştır, timeout, sandbox)
- [ ] Rust: bash_background (arka plan süreç, notify)
- [ ] E2E: dosya yaz -> oku -> karşılaştır
- [ ] E2E: edit_file fuzzy match doğrulama
- [ ] E2E: bash komut çalıştırma + timeout doğrulama

## Faz 5: Provider Katmanı
- [ ] Rust: DeepSeek provider (OpenAI-compatible)
- [ ] Rust: NVIDIA NIM provider
- [ ] Rust: Anthropic provider
- [ ] Rust: GLM provider
- [ ] Rust: auto-routing (fast/strong/vision)
- [ ] Rust: cost tracking (token sayımı + fiyat hesabı)
- [ ] Rust: credential pooling (çoklu API key rotasyonu)
- [ ] E2E: her provider'a gerçek API call
- [ ] E2E: auto-routing karar doğrulama
- [ ] E2E: cost tracking doğrulama

## Faz 6: Session Sistemi
- [ ] Rust: session store (SQLite)
- [ ] Rust: session resume (--resume, --continue)
- [ ] Rust: FTS5 tam metin arama (session_search)
- [ ] Rust: session export (JSONL)
- [ ] Frontend: sidebar'da session listesi
- [ ] E2E: session oluştur -> kapat -> resume -> mesajlar aynı

## Faz 7: Web + Browser Tool'ları
- [ ] Rust: web_search (arama motoru)
- [ ] Rust: web_fetch (URL içerik çekme)
- [ ] Rust: browser_navigate, click, type, scroll, snapshot, press
- [ ] Rust: browser_vision (screenshot + LLM analiz)
- [ ] Rust: browser_console (JS çalıştır/okuma)
- [ ] E2E: browser aç -> tıkla -> sonuç doğrula

## Faz 8: Cron Sistemi
- [ ] Rust: job scheduler (schedule parse)
- [ ] Rust: agent mode (prompt + skills yükle, çalıştır)
- [ ] Rust: script mode (no_agent=true, stdout direkt)
- [ ] Rust: delivery (origin, local, all, platform:chat_id)
- [ ] Rust: notify_on_complete + watch_patterns
- [ ] E2E: cron oluştur -> bekle -> çalıştı doğrula

## Faz 9: Delegation
- [ ] Rust: delegate_task (child process spawn)
- [ ] Rust: max_concurrent_children (paralel limit)
- [ ] Rust: toolset restriction (child sadece belirli tool'lar)
- [ ] Rust: context injection (parent -> child bilgi geçişi)
- [ ] Rust: max_spawn_depth (nested sınırı)
- [ ] E2E: delegate -> child çalıştı -> sonuç döndü

## Faz 10: MCP + Obsidian + Peer
- [ ] Rust: MCP client (stdio/HTTP server bağlantısı)
- [ ] Rust: Obsidian vault (read, write, search)
- [ ] Rust: peer communication (CC inter-agent mesajlaşma)
- [ ] E2E: MCP server bağlan -> tool çağır -> sonuç al
- [ ] E2E: Obsidian vault oku -> yaz -> doğrula

## Faz 11: TTS + Vision + Skills
- [ ] Rust: TTS (edge-tts, openai, elevenlabs, minimax)
- [ ] Rust: vision_analyze (görüntü -> LLM)
- [ ] Rust: skill sistemi (YAML frontmatter + markdown)
- [ ] Rust: skill_list, skill_view, skill_manage
- [ ] E2E: TTS ses üret -> dosya var mı doğrula
- [ ] E2E: skill oluştur -> yükle -> doğrula

## Faz 12: Git + Diğer Tool'lar
- [ ] Rust: git_status, git_diff, git_commit, git_log
- [ ] Rust: pr_create (branch + push + PR)
- [ ] Rust: todo (görev listesi)
- [ ] Rust: cost/budget (maliyet takibi)

## Faz 13: Karakter Animasyonu
- [ ] Sprite sheet entegrasyonu (kullanıcı ChatGPT ile üretecek)
- [ ] CSS/SVG animasyonlar (düşünüyor, arıyor, yazıyor, hata, başarı)
- [ ] Agent durum -> karakter durum eşleşmesi
- [ ] Mikro animasyonlar (göz kırpma, nefes, sallanma)

## Faz 14: Platform Delivery
- [ ] Telegram gateway
- [ ] Discord gateway
- [ ] Slack gateway
- [ ] SMS gateway

## Kurallar
- Reward hacking YASAK. Happy path test değil, gerçek E2E.
- Her tool call DB'ye yazıldı mı? Inject edildi mi? Doğrula.
- UI referans tasarım 1:1, ama bu bilgi kodda/yorumda/commit'te YOK.
- Her faz bitince skill olarak kaydet, sonraki session'dan devam et.
