import { useState, useCallback, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChatStore } from './stores/chatStore';
import { useAgentStore } from './stores/agentStore';
import { useSessionStore } from './stores/sessionStore';
import { useTabsStore } from './stores/tabsStore';
import type { TabSnapshot } from './stores/tabsStore';
import { useAgent } from './hooks/useAgent';
import { useGoblinState } from './hooks/useGoblinState';
import { useProjectStore } from './stores/projectStore';
import { ProjectPicker } from './components/ProjectPicker';
import { ApprovalModal } from './components/ApprovalModal';
import { ChatPanel } from './components/ChatPanel';
import { GoblinCharacter } from './components/GoblinCharacter';
import { GoblinLive } from './components/GoblinLive';
import { InputBar } from './components/InputBar';
import { RightTabs } from './components/RightTabs';
import { StatusBar } from './components/StatusBar';
import { CommandPalette } from './components/CommandPalette';
import { ConfigPanel } from './components/ConfigPanel';
import { Sidebar } from './components/Sidebar';
import { SessionPicker } from './components/SessionPicker';
import { WhatsappPanel } from './components/WhatsappPanel';
import { MemoryPanel } from './components/MemoryPanel';
import { useMemoryStore } from './stores/memoryStore';
import { TabBar } from './components/TabBar';
import type { GoblinState } from './types';
import './styles/app.css';

const MODEL_GROUPS: { label: string; options: { id: string; label: string }[] }[] = [
  {
    label: 'DeepSeek',
    options: [
      { id: 'deepseek-v4-flash', label: 'Fast' },
      { id: 'deepseek-v4-pro', label: 'Pro' },
    ],
  },
  {
    label: 'Anthropic',
    options: [
      { id: 'claude-haiku-4-5', label: 'Haiku 4.5' },
      { id: 'claude-sonnet-4-6', label: 'Sonnet 4.6' },
      { id: 'claude-opus-4-7', label: 'Opus 4.7' },
    ],
  },
  {
    label: 'NVIDIA NIM',
    options: [
      { id: 'deepseek-ai/deepseek-v4-pro', label: 'DeepSeek V4 Pro' },
    ],
  },
  {
    label: 'GLM',
    options: [
      { id: 'glm-4.6-flash', label: 'Flash' },
      { id: 'glm-4.6-air', label: 'Air' },
    ],
  },
];

function shortLabel(id: string): string {
  if (id.includes('opus')) return 'Opus';
  if (id.includes('sonnet')) return 'Sonnet';
  if (id.includes('haiku')) return 'Haiku';
  if (id.includes('pro')) return 'Pro';
  if (id.includes('air')) return 'Air';
  if (id.includes('flash')) return 'Fast';
  return id.split('/').pop() || id;
}

const GOBLIN_STATE_TEXT: Record<GoblinState, string> = {
  idle: 'Ready',
  thinking: 'Thinking...',
  reading: 'Reading...',
  writing: 'Writing...',
  searching: 'Searching...',
  running: 'Running...',
  error: 'Error!',
  success: 'Done!',
  streaming: 'Streaming...',
};

function App() {
  const messages = useChatStore((s) => s.messages);
  const input = useChatStore((s) => s.input);
  const setInput = useChatStore((s) => s.setInput);
  const rightPanelContent = useChatStore((s) => s.rightPanelContent);
  const setRightPanel = useChatStore((s) => s.setRightPanel);
  const addMessage = useChatStore((s) => s.addMessage);
  const clearMessages = useChatStore((s) => s.clearMessages);

  useEffect(() => {
    const isTauri = '__TAURI__' in window || '__TAURI_INTERNALS__' in window;
    if (isTauri) {
      document.body.classList.add('tauri-overlay');
    }
  }, []);

  const goblinState = useAgentStore((s) => s.goblinState);
  const model = useAgentStore((s) => s.model);
  const cost = useAgentStore((s) => s.cost);
  const turnCount = useAgentStore((s) => s.turnCount);
  const tokensIn = useAgentStore((s) => s.tokensIn);
  const tokensOut = useAgentStore((s) => s.tokensOut);
  const activeTool = useAgentStore((s) => s.activeTool);
  const error = useAgentStore((s) => s.error);

  const { sendMessage, clearConversation } = useAgent();

  const {
    emotionalState,
    presenceState,
    animationIntent,
  } = useGoblinState();

  const sessions = useSessionStore((s) => s.sessions);
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const fetchSessions = useSessionStore((s) => s.fetchSessions);
  const switchSession = useSessionStore((s) => s.switchSession);
  const createSession = useSessionStore((s) => s.createSession);

  const tabsHasTab = useTabsStore((s) => s.hasTab);
  const tabsAdd = useTabsStore((s) => s.addTab);
  const tabsUpdate = useTabsStore((s) => s.updateSnapshot);
  const tabsGet = useTabsStore((s) => s.getSnapshot);
  const tabsRemove = useTabsStore((s) => s.removeTab);

  const buildSnapshotForCurrent = useCallback((sid: string): TabSnapshot => {
    const chat = useChatStore.getState();
    const agent = useAgentStore.getState();
    const meta = useSessionStore.getState().sessions.find((s) => s.id === sid);
    return {
      messages: chat.messages,
      tokensIn: agent.tokensIn,
      tokensOut: agent.tokensOut,
      cost: agent.cost,
      turnCount: agent.turnCount,
      model: agent.model,
      title: meta?.title || '',
    };
  }, []);

  const applySnapshot = useCallback((snap: TabSnapshot) => {
    const chat = useChatStore.getState();
    chat.clearMessages();
    chat.clearThinking();
    chat.clearTasks();
    chat.clearDecisions();
    setRightPanel('');
    snap.messages.forEach((m) => chat.addMessage(m));
    useAgentStore.setState({
      tokensIn: snap.tokensIn,
      tokensOut: snap.tokensOut,
      cost: snap.cost,
      turnCount: snap.turnCount,
      model: snap.model || useAgentStore.getState().model,
      goblinState: 'idle',
      activeTool: null,
      error: null,
    });
  }, [setRightPanel]);

  const cwd = useProjectStore((s) => s.cwd);

  const [cmdOpen, setCmdOpen] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [showSessionPicker, setShowSessionPicker] = useState(false);
  const [whatsappOpen, setWhatsappOpen] = useState(false);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [onboardOpen, setOnboardOpen] = useState(() => {
    try { return localStorage.getItem('goblin.onboarded') !== '1'; } catch { return true; }
  });
  const dismissOnboarding = useCallback(() => {
    setOnboardOpen(false);
    try { localStorage.setItem('goblin.onboarded', '1'); } catch { /* noop */ }
  }, []);

  const [costWarn, setCostWarn] = useState<string | null>(null);
  const lastWarnedRef = useRef(0);
  useEffect(() => {
    const cap = parseFloat(localStorage.getItem('goblin.costCap') || '0.50');
    if (!Number.isFinite(cap) || cap <= 0) return;
    if (cost >= cap && lastWarnedRef.current < cap) {
      lastWarnedRef.current = cap;
      setCostWarn(`Session cost passed $${cap.toFixed(2)} (now $${cost.toFixed(4)})`);
    }
  }, [cost]);

  // Fetch sessions on mount and show picker if there are recent ones.
  // Also resolve the backend's current session id so the first send
  // routes through send_message_in_session with a real id instead of
  // falling back to the legacy send_message (which would orphan the
  // first conversation from the tab cache).
  useEffect(() => {
    fetchSessions().then(async () => {
      useChatStore.getState().fetchTasks();
      try {
        const currentId = await invoke<string>('session_current');
        if (currentId) {
          useSessionStore.getState().setActiveSessionId(currentId);
          const meta = useSessionStore.getState().sessions.find((s) => s.id === currentId);
          useTabsStore.getState().addTab(currentId, {
            messages: [],
            tokensIn: 0,
            tokensOut: 0,
            cost: 0,
            turnCount: 0,
            model: useAgentStore.getState().model,
            title: meta?.title || '',
          });
        }
      } catch {
        // Non-tauri envs (vitest, browser preview) — silent.
      }
      const recent = useSessionStore.getState().sessions.filter(s => s.messageCount > 0);
      if (recent.length > 0) {
        setShowSessionPicker(true);
      }
    });
  }, []);  // eslint-disable-line react-hooks/exhaustive-deps

  const [leftPanelWidth, setLeftPanelWidth] = useState(() => {
    const v = parseFloat(localStorage.getItem('goblin.leftPanelWidth') || '');
    return Number.isFinite(v) && v >= 16 && v <= 40 ? v : 32;
  });
  const [rightPanelWidth, setRightPanelWidth] = useState(() => {
    const v = parseFloat(localStorage.getItem('goblin.rightPanelWidth') || '');
    return Number.isFinite(v) && v >= 18 && v <= 50 ? v : 30;
  });

  useEffect(() => {
    localStorage.setItem('goblin.leftPanelWidth', String(leftPanelWidth));
  }, [leftPanelWidth]);
  useEffect(() => {
    localStorage.setItem('goblin.rightPanelWidth', String(rightPanelWidth));
  }, [rightPanelWidth]);
  const appRef = useRef<HTMLDivElement>(null);
  const resizingRef = useRef<'left' | 'right' | null>(null);
  const startXRef = useRef(0);
  const startWidthRef = useRef(32);

  const handleResizeLeftMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    resizingRef.current = 'left';
    startXRef.current = e.clientX;
    startWidthRef.current = leftPanelWidth;
    document.body.classList.add('resizing');
  }, [leftPanelWidth]);

  const handleResizeRightMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    resizingRef.current = 'right';
    startXRef.current = e.clientX;
    startWidthRef.current = rightPanelWidth;
    document.body.classList.add('resizing');
  }, [rightPanelWidth]);

  useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (!resizingRef.current) return;
      const dx = e.clientX - startXRef.current;
      const appWidth = appRef.current?.clientWidth ?? window.innerWidth;
      if (appWidth <= 0) return;
      const side = resizingRef.current;
      if (side === 'left') {
        const newWidthPct = Math.max(16, Math.min(40, startWidthRef.current + (dx / appWidth) * 100));
        setLeftPanelWidth(Math.round(newWidthPct * 10) / 10);
      } else {
        const newWidthPct = Math.max(18, Math.min(50, startWidthRef.current - (dx / appWidth) * 100));
        setRightPanelWidth(Math.round(newWidthPct * 10) / 10);
      }
    };
    const handleMouseUp = () => {
      if (!resizingRef.current) return;
      resizingRef.current = null;
      document.body.classList.remove('resizing');
    };
    document.addEventListener('mousemove', handleMouseMove);
    document.addEventListener('mouseup', handleMouseUp);
    return () => {
      document.removeEventListener('mousemove', handleMouseMove);
      document.removeEventListener('mouseup', handleMouseUp);
    };
  }, []);

  const stateText = GOBLIN_STATE_TEXT[goblinState] ?? 'Ready';

  const handleSend = useCallback(() => {
    const text = input.trim();
    if (!text) return;
    setInput('');
    sendMessage(text);
  }, [input, setInput, sendMessage]);

  const handleNewSession = useCallback(async () => {
    try {
      // Snapshot the outgoing session before we wipe state.
      const outgoing = useSessionStore.getState().activeSessionId;
      if (outgoing) {
        tabsUpdate(outgoing, buildSnapshotForCurrent(outgoing));
      }

      await createSession();
      clearConversation();
      setRightPanel('');
      useChatStore.getState().clearThinking();
      useChatStore.getState().clearTasks();
      useChatStore.getState().fetchTasks();
      useAgentStore.getState().reset();

      // After createSession() the freshest entry in sessions[] is the
      // new one; grab its id and open it as a tab.
      const fresh = useSessionStore.getState().sessions[0];
      if (fresh) {
        useSessionStore.getState().setActiveSessionId(fresh.id);
        tabsAdd(fresh.id, {
          messages: [],
          tokensIn: 0,
          tokensOut: 0,
          cost: 0,
          turnCount: 0,
          model: useAgentStore.getState().model,
          title: fresh.title || '',
        });
      }
    } catch (err) {
      console.error('New session failed:', err);
    }
  }, [createSession, clearConversation, setRightPanel, tabsAdd, tabsUpdate, buildSnapshotForCurrent]);

  const handleSelectSession = useCallback(async (id: string) => {
    if (id === activeSessionId) return;
    try {
      // 1. Snapshot outgoing into its tab cache (if it is a tab).
      if (activeSessionId && tabsHasTab(activeSessionId)) {
        tabsUpdate(activeSessionId, buildSnapshotForCurrent(activeSessionId));
      }

      // 2. Fast path: tab already cached → no backend roundtrip beyond
      //    session_switch (which the backend still needs so subsequent
      //    `send_message` invokes the right session).
      const cached = tabsGet(id);
      if (cached) {
        await switchSession(id);
        applySnapshot(cached);
        useChatStore.getState().fetchTasks();
        return;
      }

      // 3. Cold path: fetch from backend, then add as a tab.
      const data = await switchSession(id);
      if (!data) return;

      clearMessages();
      setRightPanel('');
      useChatStore.getState().clearThinking();
      useChatStore.getState().clearTasks();
      useChatStore.getState().fetchTasks();
      useAgentStore.getState().reset();

      const loadedMessages: { id: string; role: 'user' | 'assistant'; content: string; timestamp: number }[] = [];
      if (data.messages && data.messages.length > 0) {
        data.messages.forEach((m) => {
          const msg = {
            id: Math.random().toString(36).substring(2, 10),
            role: m.role as 'user' | 'assistant',
            content: m.content,
            timestamp: Date.now(),
          };
          loadedMessages.push(msg);
          addMessage(msg);
        });
      }

      if (data.tokensIn || data.tokensOut) {
        useAgentStore.getState().addTokens(data.tokensIn, data.tokensOut);
      }
      if (data.cost) {
        useAgentStore.getState().addCost(data.cost);
      }
      if (data.model) {
        useAgentStore.getState().setModel(data.model);
      }

      // Cache the freshly-loaded session as a tab.
      tabsAdd(id, {
        messages: loadedMessages,
        tokensIn: data.tokensIn ?? 0,
        tokensOut: data.tokensOut ?? 0,
        cost: data.cost ?? 0,
        turnCount: 0,
        model: data.model || useAgentStore.getState().model,
        title: data.title || '',
      });
    } catch (err) {
      console.error('Session switch failed:', err);
    }
  }, [activeSessionId, switchSession, clearMessages, setRightPanel, addMessage, tabsHasTab, tabsUpdate, tabsGet, tabsAdd, applySnapshot, buildSnapshotForCurrent]);

  const handleCloseTab = useCallback(async (id: string) => {
    const wasActive = id === activeSessionId;
    const nextId = tabsRemove(id);

    if (wasActive) {
      if (nextId) {
        await handleSelectSession(nextId);
      } else {
        // No tabs left — wipe the view, leave backend session intact.
        clearMessages();
        setRightPanel('');
        useChatStore.getState().clearThinking();
        useChatStore.getState().clearTasks();
        useAgentStore.getState().reset();
        useSessionStore.getState().setActiveSessionId('');
      }
    }
  }, [activeSessionId, tabsRemove, handleSelectSession, clearMessages, setRightPanel]);

  const handleCommand = useCallback((cmd: string) => {
    switch (cmd) {
      case 'new':
        handleNewSession();
        break;
      case 'clear':
        setRightPanel('');
        break;
      case 'copy':
        if (navigator.clipboard) {
          navigator.clipboard.writeText(rightPanelContent).catch(() => {});
        }
        break;
      case 'sessions':
        setSidebarOpen(true);
        break;
      case 'cost':
        setRightPanel(
          `## Cost Report\n\n` +
          `| Metric | Value |\n|--------|-------|\n` +
          `| Total tokens | ${(tokensIn + tokensOut).toLocaleString()} |\n` +
          `| Input tokens | ${tokensIn.toLocaleString()} |\n` +
          `| Output tokens | ${tokensOut.toLocaleString()} |\n` +
          `| Total cost | $${cost.toFixed(6)} |\n` +
          `| Turn count | ${turnCount} |\n` +
          `| Model | ${model} |`
        );
        break;
      case 'model-fast':
        useAgentStore.getState().setModel('deepseek-v4-flash');
        setRightPanel('## Model Changed\n\n**DeepSeek Flash** - optimized for fast responses.');
        break;
      case 'model-pro':
        useAgentStore.getState().setModel('deepseek-v4-pro');
        setRightPanel('## Model Changed\n\n**DeepSeek Pro** - optimized for complex analysis and coding.');
        break;
      default:
        break;
    }
  }, [handleNewSession, setRightPanel, rightPanelContent, tokensIn, tokensOut, cost, turnCount, model]);

  const handlePickerSelect = useCallback(async (id: string) => {
    setShowSessionPicker(false);
    await handleSelectSession(id);
  }, [handleSelectSession]);
  const handlePickerNew = useCallback(async () => {
    setShowSessionPicker(false);
    await handleNewSession();
  }, [handleNewSession]);

  useEffect(() => {
    fetchSessions();
  }, [fetchSessions]);

  // Global keyboard shortcuts
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key === 'k') {
        e.preventDefault();
        setCmdOpen(true);
      }
      if (mod && e.key === 'n') {
        e.preventDefault();
        handleNewSession();
      }
      if (mod && e.shiftKey && e.key === 'S') {
        e.preventDefault();
        setSidebarOpen(true);
      }
      if (mod && e.key === '/') {
        e.preventDefault();
        setShortcutsOpen((v) => !v);
      }
      if (mod && e.key === 'm') {
        e.preventDefault();
        const { open, setOpen } = useMemoryStore.getState();
        setOpen(!open);
      }
      // ⌘1-9 — switch to tab N (1-indexed)
      if (mod && /^[1-9]$/.test(e.key)) {
        const idx = parseInt(e.key, 10) - 1;
        const tabs = useTabsStore.getState().openTabs;
        const target = tabs[idx];
        if (target) {
          e.preventDefault();
          handleSelectSession(target);
        }
      }
      if (e.key === 'Escape') {
        setCmdOpen(false);
        setSidebarOpen(false);
        setShortcutsOpen(false);
        setModelMenuOpen(false);
        setConfigOpen(false);
        setShowSessionPicker(false);
        setOnboardOpen(false);
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [handleNewSession]);

  return (
    <div className="app" ref={appRef}>
      <Sidebar
        isOpen={sidebarOpen}
        onToggle={() => setSidebarOpen((v) => !v)}
        sessions={sessions}
        activeSessionId={activeSessionId}
        onSelectSession={handleSelectSession}
      />

      <ApprovalModal />

      {cmdOpen && <CommandPalette onCommand={handleCommand} onClose={() => setCmdOpen(false)} />}

      {costWarn && (
        <div className="cost-toast">
          <span className="cost-toast-icon">⚠</span>
          <span className="cost-toast-text">{costWarn}</span>
          <button className="cost-toast-action" onClick={() => { setConfigOpen(true); setCostWarn(null); }}>Adjust cap</button>
          <button className="cost-toast-close" onClick={() => setCostWarn(null)}>×</button>
        </div>
      )}

      {onboardOpen && !showSessionPicker && (
        <div className="onboard-toast">
          <div className="onboard-step onboard-step-col">
            <div className="onboard-step-header">
              <span className="onboard-num">1</span>
              <span>
                <strong>Select a project</strong> — Goblin reads files, writes code, understands git
                {cwd && <span className="onboard-cwd-ok"> ✓ {cwd.split('/').pop()}</span>}
              </span>
            </div>
            <ProjectPicker inline />
          </div>
          <div className="onboard-step"><span className="onboard-num">2</span> Choose model — the <strong>Fast</strong> / <strong>Pro</strong> pill in the header switches it</div>
          <div className="onboard-step"><span className="onboard-num">3</span> <kbd>⌘K</kbd> command palette, <kbd>⌘/</kbd> all shortcuts</div>
          <button className="onboard-dismiss" onClick={dismissOnboarding}>Got it</button>
        </div>
      )}

      {shortcutsOpen && (
        <div className="shortcuts-overlay" onClick={() => setShortcutsOpen(false)}>
          <div className="shortcuts-panel" onClick={(e) => e.stopPropagation()}>
            <div className="shortcuts-header">
              <span>Keyboard Shortcuts</span>
              <button className="shortcuts-close" onClick={() => setShortcutsOpen(false)}>×</button>
            </div>
            <div className="shortcuts-grid">
              <div className="shortcuts-row"><kbd>⌘K</kbd><span>Command palette</span></div>
              <div className="shortcuts-row"><kbd>⌘N</kbd><span>New session</span></div>
              <div className="shortcuts-row"><kbd>⌘⇧S</kbd><span>Sessions sidebar</span></div>
              <div className="shortcuts-row"><kbd>⌘/</kbd><span>This cheat sheet</span></div>
              <div className="shortcuts-row"><kbd>⌘M</kbd><span>Memory panel</span></div>
              <div className="shortcuts-row"><kbd>⌘1</kbd>–<kbd>9</kbd><span>Switch to tab N</span></div>
              <div className="shortcuts-row"><kbd>/</kbd><span>Open palette (empty input)</span></div>
              <div className="shortcuts-row"><kbd>Enter</kbd><span>Send message</span></div>
              <div className="shortcuts-row"><kbd>⇧Enter</kbd><span>Newline in input</span></div>
              <div className="shortcuts-row"><kbd>Esc</kbd><span>Close panel / cancel</span></div>
            </div>
          </div>
        </div>
      )}

      <ConfigPanel
        isOpen={configOpen}
        onToggle={() => setConfigOpen((v) => !v)}
      />

      <WhatsappPanel
        isOpen={whatsappOpen}
        onToggle={() => setWhatsappOpen(false)}
      />

      <MemoryPanel />

      {showSessionPicker && (
        <SessionPicker
          sessions={sessions}
          onSelect={handlePickerSelect}
          onNew={handlePickerNew}
        />
      )}

      <div className="app-main">
      {/* LEFT: Chat */}
      <div className="panel-chat" style={{ width: `${leftPanelWidth}%`, minWidth: 260, maxWidth: '45%' }}>
        <div className="panel-header">
          <div className="panel-header-left">
            <button className="header-icon-btn" onClick={() => setSidebarOpen(true)} title="Sessions">
              <svg width="14" height="14" viewBox="0 0 16 16" fill="none"><rect x="1" y="2" width="14" height="2" rx="1" fill="currentColor"/><rect x="1" y="7" width="14" height="2" rx="1" fill="currentColor"/><rect x="1" y="12" width="9" height="2" rx="1" fill="currentColor"/></svg>
            </button>
            <span className="panel-header-title">goblin</span>
          </div>
          <div className="panel-header-actions">
            <ProjectPicker />
            <div className="model-picker">
              <button
                className={`header-pill ${model.includes('pro') || model.includes('opus') || model.includes('sonnet') ? 'header-pill-pro' : 'header-pill-fast'}`}
                onClick={() => setModelMenuOpen((v) => !v)}
                title={`Current: ${model}`}
              >
                <span className="header-pill-dot" />
                {shortLabel(model)}
                <span className="header-pill-caret">▾</span>
              </button>
              {modelMenuOpen && (
                <div className="model-menu" onClick={(e) => e.stopPropagation()}>
                  {MODEL_GROUPS.map((g) => (
                    <div key={g.label} className="model-group">
                      <div className="model-group-label">{g.label}</div>
                      {g.options.map((opt) => (
                        <button
                          key={opt.id}
                          className={`model-item ${model === opt.id ? 'model-item-active' : ''}`}
                          onClick={() => {
                            useAgentStore.getState().setModel(opt.id);
                            setModelMenuOpen(false);
                          }}
                        >
                          <span className="model-item-name">{opt.label}</span>
                          <span className="model-item-id">{opt.id}</span>
                        </button>
                      ))}
                    </div>
                  ))}
                </div>
              )}
            </div>
            <button className="header-icon-btn" onClick={() => setSidebarOpen(true)} title="Sessions">
              <svg width="15" height="15" viewBox="0 0 16 16" fill="none"><rect x="1" y="2" width="14" height="2.5" rx="1.25" fill="currentColor"/><rect x="1" y="6.75" width="14" height="2.5" rx="1.25" fill="currentColor"/><rect x="1" y="11.5" width="9" height="2.5" rx="1.25" fill="currentColor"/></svg>
            </button>
            <button className="header-icon-btn" onClick={() => setWhatsappOpen(true)} title="WhatsApp">
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/></svg>
            </button>
            <button className="header-icon-btn" onClick={() => useMemoryStore.getState().setOpen(true)} title="Memory (⌘M)">
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z"/><path d="M12 7v5l3 3"/></svg>
            </button>
            <button className="header-icon-btn" onClick={() => setConfigOpen(true)} title="Settings">
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>
            </button>
          </div>
        </div>

        <TabBar
          onSelect={handleSelectSession}
          onClose={handleCloseTab}
          onNew={handleNewSession}
        />

        <ChatPanel
          messages={messages}
        />

        <GoblinCharacter
          emotionalState={emotionalState}
          presenceState={presenceState}
          animationIntent={animationIntent}
        />

        <InputBar
          input={input}
          onInputChange={setInput}
          onSend={handleSend}
          onOpenPalette={() => setCmdOpen(true)}
          onFileAttach={(file) => {
            // Image files become real multimodal payloads (queued on
            // chatStore, picked up at send time). Non-image files still
            // get the legacy "filename note in input" fallback so the
            // model at least knows the user dropped something.
            const allowed = ['image/png', 'image/jpeg', 'image/webp', 'image/gif'];
            if (allowed.includes(file.type)) {
              const reader = new FileReader();
              reader.onload = () => {
                const result = typeof reader.result === 'string' ? reader.result : '';
                const comma = result.indexOf(',');
                const base64 = comma >= 0 ? result.slice(comma + 1) : '';
                if (!base64) return;
                useChatStore.getState().addPendingAttachment({
                  id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
                  name: file.name,
                  mime_type: file.type,
                  data: base64,
                  bytes: file.size,
                });
              };
              reader.readAsDataURL(file);
              return;
            }
            const kb = (file.size / 1024).toFixed(1);
            const note = `📎 ${file.name} (${file.type || 'unknown'}, ${kb} KB)`;
            const cur = useChatStore.getState().input;
            useChatStore.getState().setInput(cur ? `${cur}\n${note}\n` : `${note}\n`);
          }}
        />
      </div>

      {/* LEFT RESIZE HANDLE */}
      <div className="panel-resize-handle" onMouseDown={handleResizeLeftMouseDown} />

      {/* CENTER: Live Character */}
      <div className="panel-center">
        <GoblinLive
          emotionalState={emotionalState}
          presenceState={presenceState}
          animationIntent={animationIntent}
        />
      </div>

      {/* RIGHT: Tabbed utility */}
      <div className="panel-resize-handle panel-resize-handle-right" onMouseDown={handleResizeRightMouseDown} />
      <div className="panel-right" style={{ width: `${rightPanelWidth}%`, minWidth: 240, maxWidth: '50%' }}>
        <RightTabs />
      </div>
      </div>

      {/* STATUS BAR */}
      <StatusBar
        state={goblinState}
        stateText={stateText}
        model={model}
        turnCount={turnCount}
        cost={cost}
        tokensIn={tokensIn}
        tokensOut={tokensOut}
        activeTool={activeTool}
        error={error}
        onRetry={() => {
          const msgs = useChatStore.getState().messages;
          const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
          if (lastUser) {
            useAgentStore.getState().setError(null);
            sendMessage(lastUser.content);
          }
        }}
      />
    </div>
  );
}

export default App;
