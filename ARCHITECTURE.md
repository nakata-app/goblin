# Goblin - Architecture

## Overview
Goblin is a desktop AI agent app. Tauri (Rust backend) + React/TypeScript (frontend).
Combines the best of Metis/Aegis (Rust agent tools, TUI, auto-routing, mnemonics, cost tracking)
with Hermes (cron, delegation, browser, TTS, platform delivery, session search, skills).

## Stack
- Frontend: React 19 + TypeScript + Vite
- Backend: Tauri 2 (Rust)
- Database: SQLite (via Tauri plugin)
- State: Zustand
- Styling: CSS (dark theme, reference design enforced)

## Directory Structure

```
goblin/
├── src/                        # Frontend (React)
│   ├── components/
│   │   ├── ChatPanel.tsx       # Left panel: messages
│   │   ├── GoblinCharacter.tsx # Character animation strip
│   │   ├── InputBar.tsx        # Message input
│   │   ├── OutputPanel.tsx     # Right panel: tool output
│   │   ├── StatusBar.tsx       # Bottom status bar
│   │   └── Sidebar.tsx         # Session history sidebar
│   ├── hooks/
│   │   ├── useAgent.ts         # Agent loop hook
│   │   └── useGoblinState.ts   # Character state hook
│   ├── stores/
│   │   ├── chatStore.ts        # Message state (Zustand)
│   │   └── agentStore.ts       # Agent/tool state (Zustand)
│   ├── styles/
│   │   ├── global.css
│   │   └── app.css
│   ├── types/
│   │   └── index.ts
│   ├── App.tsx
│   └── main.tsx
├── src-tauri/                  # Backend (Rust)
│   ├── src/
│   │   ├── main.rs
│   │   ├── agent/
│   │   │   ├── loop.rs         # Core conversation loop
│   │   │   ├── prompt.rs       # System prompt builder
│   │   │   └── context.rs      # Context window management
│   │   ├── tools/
│   │   │   ├── mod.rs
│   │   │   ├── file_ops.rs     # read_file, write_file, edit_file, multi_edit
│   │   │   ├── search.rs       # grep, glob
│   │   │   ├── shell.rs        # bash, bash_background
│   │   │   ├── web.rs          # web_search, web_fetch
│   │   │   ├── browser.rs      # browser_navigate, click, type, scroll, snapshot, vision
│   │   │   ├── memory.rs       # memory_add, search, remove, stats + auto-observe + auto-inject
│   │   │   ├── session.rs      # session_search, session_list
│   │   │   ├── cron.rs         # cron_create, list, remove, run
│   │   │   ├── delegation.rs   # delegate_task
│   │   │   ├── git.rs          # status, diff, commit, log, pr_create
│   │   │   ├── vision.rs       # vision_analyze
│   │   │   ├── tts.rs          # text_to_speech
│   │   │   ├── skills.rs       # skill_list, view, manage
│   │   │   ├── todo.rs         # task list
│   │   │   ├── mnemonics.rs    # mnemonics_add, retrieve, observe, learn (native)
│   │   │   ├── mcp.rs          # MCP client (connect external servers)
│   │   │   ├── obsidian.rs     # Obsidian vault read/write/search
│   │   │   ├── peer.rs         # Peer communication (CC inter-agent)
│   │   │   ├── premortem.rs    # Risk analysis: assume failure 6mo out, work backward
│   │   │   └── eisenhower.rs   # Eisenhower matrix: urgency/importance task quadrant
│   │   ├── memory/
│   │   │   ├── mod.rs
│   │   │   ├── db.rs           # SQLite schema, CRUD
│   │   │   ├── observe.rs      # Auto-observe every tool call
│   │   │   ├── inject.rs       # Auto-inject relevant memories per turn
│   │   │   ├── reinforcement.rs # Learn from user rejections
│   │   │   └── compact.rs      # Pruning policy (age + tier + access)
│   │   ├── provider/
│   │   │   ├── mod.rs          # Provider trait + routing
│   │   │   ├── openai.rs       # OpenAI-compatible (DeepSeek, GLM, etc.)
│   │   │   ├── anthropic.rs    # Anthropic API
│   │   │   ├── nvidia.rs       # NVIDIA NIM
│   │   │   └── auto_route.rs   # Auto-routing (fast/strong/vision)
│   │   ├── session/
│   │   │   ├── mod.rs
│   │   │   ├── store.rs        # SQLite session store
│   │   │   └── search.rs       # FTS5 full-text search
│   │   ├── config/
│   │   │   └── mod.rs          # config.toml parsing
│   │   └── cron/
│   │       ├── mod.rs
│   │       ├── scheduler.rs    # Job scheduler
│   │       └── runner.rs       # Agent vs script mode
│   ├── Cargo.toml
│   └── tauri.conf.json
├── test/
│   ├── e2e/
│   │   ├── agent_loop.test.ts
│   │   ├── memory.test.ts
│   │   └── tools.test.ts
│   └── unit/
│       ├── memory.test.ts
│       └── provider.test.ts
├── package.json
├── tsconfig.json
├── vite.config.ts
└── ARCHITECTURE.md
```

## Memory Schema (SQLite)

```sql
CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  ns TEXT NOT NULL,           -- namespace: sessions, proj:xxx, reference, feedback
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
4. If project dir has .goblin/ -> merge project-scoped memories too

## Compact Policy

- tier 1, not accessed in 30 days -> archive
- tier 2+, never auto-archive
- Sessions older than 90 days -> compress summary only

## Premortem Flow

1. Given a plan/decision/commit -> assume it failed 6 months from now
2. Identify all root causes: technical, operational, dependency, human error
3. For each cause: how it manifests, prevention/mitigation, detection criteria, owner
4. Return risk list + revised plan with blind spots exposed
5. Store premortem results -> memory (tier 2+), linked to session

## Eisenhower Matrix

1. Given a task/issue list -> classify by urgency + importance into 4 quadrants:
   - Q1: Do First (urgent + important)
   - Q2: Schedule (not urgent + important)
   - Q3: Delegate (urgent + not important)
   - Q4: Eliminate (not urgent + not important)
2. Persist matrix state per session/project -> revisitable
3. Agent can suggest reclassification based on changing context

## Provider Auto-Routing

- Fast model: deepseek-v4-flash (coding, simple tasks)
- Strong model: deepseek-v4-pro (complex reasoning)
- Vision: llama-3.2-90b-vision or provider's vision model
- Route based on: task complexity, image input, user override

## Character Animation

- Goblin character displayed in left panel strip
- States: idle, thinking, reading, writing, searching, running, error, success
- Animation: CSS keyframes + sprite sheet (user will generate via ChatGPT)
- State mapped from agent's current tool activity

## Phases

See TODO.md for phase-by-phase build plan.
