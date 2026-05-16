# Goblin - Architecture

## Overview
Goblin is a desktop AI agent app. Tauri (Rust backend) + React/TypeScript (frontend).

## Stack
- Frontend: React 19 + TypeScript + Vite
- Backend: Tauri 2 (Rust)
- Database: SQLite (via Tauri plugin)
- State: Zustand
- Styling: CSS (dark theme)

## Directory Structure

```
goblin/
├── src/                          # Frontend (React)
│   ├── components/
│   │   ├── App.tsx               # Root layout, tab routing
│   │   ├── ChatPanel.tsx         # Left panel: message thread
│   │   ├── InputBar.tsx          # Chat input + dropzone
│   │   ├── OutputPanel.tsx       # Right panel: tool/streaming output
│   │   ├── RightTabs.tsx         # Tab switcher (Output / WhatsApp / ...)
│   │   ├── StatusBar.tsx         # Bottom bar: model, cost, turn
│   │   ├── TabBar.tsx            # Multi-session tabs
│   │   ├── Sidebar.tsx           # Session history sidebar
│   │   ├── CommandPalette.tsx    # ⌘K command palette (15 commands)
│   │   ├── ConfigPanel.tsx       # Settings panel
│   │   ├── SessionPicker.tsx     # Session resume picker
│   │   ├── ErrorBoundary.tsx
│   │   ├── GoblinCharacter.tsx   # CSS-animated goblin sprite (state-based)
│   │   ├── GoblinLive.tsx        # Procedural 2D goblin (canvas)
│   │   ├── Goblin3D.tsx          # Three.js 3D goblin (optional)
│   │   └── WhatsappPanel.tsx     # WhatsApp contacts + conversation view
│   ├── hooks/
│   │   ├── useAgent.ts           # Agent loop hook (Tauri IPC)
│   │   └── useGoblinState.ts     # Character animation state
│   ├── stores/
│   │   ├── chatStore.ts          # Message state (Zustand)
│   │   ├── agentStore.ts         # Agent/tool running state
│   │   ├── characterStore.ts     # Goblin emotional/animation state
│   │   ├── sessionStore.ts       # Active session metadata
│   │   └── tabsStore.ts          # Multi-tab state
│   ├── __tests__/                # Vitest unit + E2E tests
│   │   ├── agent-loop.e2e.test.ts
│   │   ├── pure-functions.test.ts
│   │   ├── sessionStore.test.ts
│   │   └── stores.test.ts
│   ├── styles/
│   │   ├── global.css
│   │   └── app.css
│   └── types/index.ts
├── e2e/                          # Playwright smoke tests
│   ├── app.spec.ts
│   └── smoke.spec.ts
├── src-tauri/                    # Backend (Rust)
│   ├── src/
│   │   ├── lib.rs                # All Tauri commands registered here
│   │   ├── main.rs
│   │   ├── daemon.rs             # System tray daemon
│   │   ├── headless.rs           # Headless/CLI mode
│   │   ├── task.rs               # TaskStore (in-memory task tracking)
│   │   ├── agent/
│   │   │   ├── loop.rs           # Core conversation loop (LLM → tool → continue)
│   │   │   ├── prompt.rs         # System prompt builder + memory injection
│   │   │   ├── context.rs        # Context window management (token trim)
│   │   │   └── soul.rs           # Goblin personality layer
│   │   ├── tools/
│   │   │   ├── mod.rs            # Tool registry + dispatch
│   │   │   ├── file_ops.rs       # read_file, write_file, edit_file, multi_edit
│   │   │   ├── search.rs         # grep, glob
│   │   │   ├── shell.rs          # bash, bash_background
│   │   │   ├── web.rs            # web_search, web_fetch
│   │   │   ├── browser.rs        # browser_navigate, click, type, scroll, snapshot, browser_vision
│   │   │   ├── media.rs          # vision_analyze, text_to_speech, voice_record
│   │   │   ├── git.rs            # status, diff, commit, log, pr_create
│   │   │   ├── skills.rs         # skill_list, skill_view, skill_manage, skill_search
│   │   │   ├── mcp.rs            # MCP client (connect external MCP servers)
│   │   │   ├── mcp_server.rs     # MCP server mode (expose Goblin as MCP server)
│   │   │   ├── vault.rs          # obsidian_read, obsidian_write, obsidian_search, vault_stats
│   │   │   ├── peer.rs           # peer_send, peer_broadcast, peer_status, peer_coordinate
│   │   │   ├── sandbox.rs        # sandbox_exec, sandbox_list (Docker isolation)
│   │   │   ├── meta.rs           # delegate_task, premortem, eisenhower
│   │   │   └── compactor.rs      # Context compaction helpers
│   │   ├── provider/
│   │   │   ├── mod.rs            # Provider trait + ProviderResponse types
│   │   │   ├── openai.rs         # OpenAI-compatible (DeepSeek, GLM, Ollama, etc.)
│   │   │   ├── anthropic.rs      # Anthropic Messages API (SSE streaming)
│   │   │   ├── nvidia.rs         # NVIDIA NIM (SSE streaming)
│   │   │   ├── gemini.rs         # Google Gemini
│   │   │   └── glm.rs            # ZhipuAI GLM
│   │   ├── memory/
│   │   │   ├── db.rs             # SQLite schema + CRUD (memories, observations, learned)
│   │   │   ├── embed.rs          # Embedding for semantic search
│   │   │   ├── observe.rs        # Auto-observe every tool call
│   │   │   ├── inject.rs         # Auto-inject relevant memories per turn
│   │   │   ├── reinforcement.rs  # Learn from user rejections (learned table)
│   │   │   └── compact.rs        # Pruning policy (age + tier + access count)
│   │   ├── session/
│   │   │   ├── store.rs          # SQLite session store + JSONL messages
│   │   │   └── search.rs         # FTS5 full-text session search
│   │   ├── mnemonics/
│   │   │   └── mod.rs            # Bridge to external mnemonics binary (MCP)
│   │   ├── cron/
│   │   │   └── mod.rs            # Cron scheduler + agent/script runner
│   │   ├── channel/
│   │   │   ├── mod.rs            # Channel trait + routing
│   │   │   ├── telegram.rs       # Telegram bot channel
│   │   │   └── webhook.rs        # Generic webhook channel
│   │   ├── whatsapp/
│   │   │   ├── mod.rs            # WhatsApp bridge (WIP)
│   │   │   └── db.rs             # WhatsApp conversation SQLite store
│   │   ├── config/
│   │   │   └── mod.rs            # config.toml parsing + AgentProfile routing
│   │   ├── http/
│   │   │   └── mod.rs            # Embedded HTTP server
│   │   ├── mcp/
│   │   │   └── mod.rs            # MCP protocol types
│   │   └── plugin/
│   │       └── mod.rs            # Plugin host
│   ├── Cargo.toml
│   └── tauri.conf.json
├── package.json
├── vite.config.ts
├── vitest.config.ts
├── playwright.config.ts
├── TODO.md
└── ARCHITECTURE.md
```

## Memory Schema (SQLite)

```sql
CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  ns TEXT NOT NULL,           -- namespace: proj:xxx, global, feedback
  tier INTEGER DEFAULT 1,    -- 1=normal, 2=important, 3=critical
  text TEXT NOT NULL,
  meta TEXT,                  -- JSON metadata
  created INTEGER NOT NULL,
  last_accessed INTEGER NOT NULL,
  access_count INTEGER DEFAULT 0
);

CREATE TABLE observations (
  id TEXT PRIMARY KEY,
  ts INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  tool_name TEXT NOT NULL,
  args_summary TEXT,
  result_summary TEXT,
  success BOOLEAN NOT NULL
);

CREATE TABLE learned (
  id TEXT PRIMARY KEY,
  preference TEXT NOT NULL,
  reinforcement_count INTEGER DEFAULT 1,
  last_seen INTEGER NOT NULL
);

CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  title TEXT,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  model TEXT,
  provider TEXT,
  cost REAL DEFAULT 0,
  tokens_in INTEGER DEFAULT 0,
  tokens_out INTEGER DEFAULT 0,
  messages TEXT               -- JSONL
);

CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  schedule TEXT NOT NULL,
  prompt TEXT,
  script TEXT,
  no_agent BOOLEAN DEFAULT 0,
  enabled BOOLEAN DEFAULT 1,
  last_run INTEGER,
  next_run INTEGER,
  delivery TEXT DEFAULT 'origin',
  workdir TEXT,
  skills TEXT,                -- JSON array
  context_from TEXT           -- JSON array of job IDs
);

-- FTS5 for full-text search
CREATE VIRTUAL TABLE sessions_fts USING fts5(title, messages);
CREATE VIRTUAL TABLE memories_fts USING fts5(text, ns);
```

## Auto-Observation Flow

1. Every tool call -> observation record written (no agent decision needed)
2. Fields: timestamp, session, tool name, args summary, result summary, success/fail
3. If user rejects a tool result -> learned table incremented

## Auto-Inject Flow

1. Every turn start -> query memories by ns+tier relevance
2. Query learned preferences by reinforcement_count DESC
3. Inject into system prompt as structured block
4. If project dir has .goblin/ -> merge project-scoped memories

## Compact Policy

- tier 1, not accessed in 30 days -> archive
- tier 2+, never auto-archive
- Sessions older than 90 days -> compress to summary only

## Provider Auto-Routing

Config-driven via `~/.goblin/config.toml` `[agent_profiles]`. Each profile has:
- `models`: list of preferred models
- `triggers`: keyword patterns that activate this profile
- `tools`: allowed tool list

Default routing (without profiles):
- Fast tasks: deepseek-v4-flash
- Complex tasks: deepseek-v4-pro
- Vision: llama-3.2-90b-vision or provider's vision model

Route decision lives in `config/mod.rs::route_to_agent()`, currently wired in config
but not yet called from the agent loop (pending integration).

## WhatsApp (WIP)

WhatsApp bridge runs as a sidecar. Status: basic send/receive + SQLite persistence works,
auto-reply agent loop integrated. Not production-ready, untracked files, no feature flag.

## Phases

See TODO.md for phase-by-phase build plan.
