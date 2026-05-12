import { useState, useCallback, useEffect, useRef } from 'react';
import { useChatStore } from './stores/chatStore';
import { useAgentStore } from './stores/agentStore';
import { useSessionStore } from './stores/sessionStore';
import { useAgent } from './hooks/useAgent';
import { useGoblinState } from './hooks/useGoblinState';
import { ChatPanel } from './components/ChatPanel';
import { GoblinCharacter } from './components/GoblinCharacter';
import { GoblinLive } from './components/GoblinLive';
import { InputBar } from './components/InputBar';
import { RightTabs } from './components/RightTabs';
import { StatusBar } from './components/StatusBar';
import { CommandPalette } from './components/CommandPalette';
import { Sidebar } from './components/Sidebar';
import { SessionPicker } from './components/SessionPicker';
import type { GoblinState } from './types';
import './styles/app.css';

const GOBLIN_STATE_TEXT: Record<GoblinState, string> = {
  idle: 'Ready',
  thinking: 'Thinking...',
  reading: 'Reading...',
  writing: 'Writing...',
  searching: 'Searching...',
  running: 'Running...',
  error: 'Error!',
  success: 'Done!',
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

  const [cmdOpen, setCmdOpen] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [showSessionPicker, setShowSessionPicker] = useState(false);

  // Fetch sessions on mount and show picker if there are recent ones
  useEffect(() => {
    fetchSessions().then(() => {
      const recent = useSessionStore.getState().sessions.filter(s => s.messageCount > 0);
      if (recent.length > 0) {
        setShowSessionPicker(true);
      }
    });
  }, []);  // eslint-disable-line react-hooks/exhaustive-deps

  const [leftPanelWidth, setLeftPanelWidth] = useState(32);
  const [rightPanelWidth, setRightPanelWidth] = useState(30);
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
      await createSession();
      clearConversation();
      setRightPanel('');
      useChatStore.getState().clearThinking();
      useChatStore.getState().clearTasks();
      useAgentStore.getState().reset();
    } catch (err) {
      console.error('New session failed:', err);
    }
  }, [createSession, clearConversation, setRightPanel]);

  const handleSelectSession = useCallback(async (id: string) => {
    if (id === activeSessionId) return;
    try {
      const data = await switchSession(id);
      if (!data) return;

      clearMessages();
      setRightPanel('');
      useChatStore.getState().clearThinking();
      useChatStore.getState().clearTasks();
      useAgentStore.getState().reset();

      if (data.messages && data.messages.length > 0) {
        data.messages.forEach((m) => {
          addMessage({
            id: Math.random().toString(36).substring(2, 10),
            role: m.role as 'user' | 'assistant',
            content: m.content,
            timestamp: Date.now(),
          });
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
    } catch (err) {
      console.error('Session switch failed:', err);
    }
  }, [activeSessionId, switchSession, clearMessages, setRightPanel, addMessage]);

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
      if (e.key === 'Escape') {
        setCmdOpen(false);
        setSidebarOpen(false);
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

      {cmdOpen && <CommandPalette onCommand={handleCommand} onClose={() => setCmdOpen(false)} />}

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
          <span className="panel-header-title">goblin</span>
          <div className="panel-header-actions">
            <button className="header-btn" onClick={() => setSidebarOpen(true)}>sessions</button>
            <button className="header-btn" onClick={() => setCmdOpen(true)}>⌘K</button>
            <button className="header-btn" onClick={() => clearConversation()}>clear</button>
            <button className="header-btn" onClick={handleNewSession}>new</button>
          </div>
        </div>

        <ChatPanel messages={messages} />

        <GoblinCharacter
          emotionalState={emotionalState}
          presenceState={presenceState}
          animationIntent={animationIntent}
        />

        <InputBar
          input={input}
          onInputChange={setInput}
          onSend={handleSend}
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
      <div className="panel-resize-handle" onMouseDown={handleResizeRightMouseDown} />
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
      />
    </div>
  );
}

export default App;
