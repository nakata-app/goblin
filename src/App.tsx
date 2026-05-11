import { useState, useCallback, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChatStore } from './stores/chatStore';
import { useAgentStore } from './stores/agentStore';
import { useSessionStore } from './stores/sessionStore';
import { useAgent } from './hooks/useAgent';
import { ChatPanel } from './components/ChatPanel';
import { GoblinCharacter } from './components/GoblinCharacter';
import { InputBar } from './components/InputBar';
import { OutputPanel } from './components/OutputPanel';
import { StatusBar } from './components/StatusBar';
import { CommandPalette } from './components/CommandPalette';
import { Sidebar } from './components/Sidebar';
import type { GoblinState } from './types';
import './styles/app.css';

const GOBLIN_STATE_TEXT: Record<GoblinState, { text: string; detail: string }> = {
  idle: { text: 'Hazir', detail: 'komut bekleniyor' },
  thinking: { text: 'Dusunuyor', detail: 'model yanitliyor...' },
  reading: { text: 'Okuyor', detail: 'dosya taranıyor...' },
  writing: { text: 'Yaziyor', detail: 'dosya duzenleniyor...' },
  searching: { text: 'Arastiriyor', detail: 'araniyor...' },
  running: { text: 'Calistiriyor', detail: 'bash komutu...' },
  error: { text: 'Hata!', detail: 'bir seyler ters gitti' },
  success: { text: 'Tamam!', detail: 'islem basarili' },
};

function App() {
  const messages = useChatStore((s) => s.messages);
  const input = useChatStore((s) => s.input);
  const setInput = useChatStore((s) => s.setInput);
  const rightPanelContent = useChatStore((s) => s.rightPanelContent);
  const setRightPanel = useChatStore((s) => s.setRightPanel);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const addMessage = useChatStore((s) => s.addMessage);
  const clearMessages = useChatStore((s) => s.clearMessages);

  const goblinState = useAgentStore((s) => s.goblinState);
  const model = useAgentStore((s) => s.model);
  const cost = useAgentStore((s) => s.cost);
  const turnCount = useAgentStore((s) => s.turnCount);
  const tokensIn = useAgentStore((s) => s.tokensIn);
  const tokensOut = useAgentStore((s) => s.tokensOut);
  const activeTool = useAgentStore((s) => s.activeTool);
  const error = useAgentStore((s) => s.error);

  const { sendMessage, clearConversation } = useAgent();

  const sessions = useSessionStore((s) => s.sessions);
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const fetchSessions = useSessionStore((s) => s.fetchSessions);
  const switchSession = useSessionStore((s) => s.switchSession);
  const createSession = useSessionStore((s) => s.createSession);

  const [cmdOpen, setCmdOpen] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const isAnimating = goblinState !== 'idle';
  const goblin = GOBLIN_STATE_TEXT[goblinState];

  const handleSend = useCallback(() => {
    const text = input.trim();
    if (!text || isStreaming) return;
    setInput('');
    sendMessage(text);
  }, [input, isStreaming, setInput, sendMessage]);

  const handleNewSession = useCallback(async () => {
    try {
      await createSession();
      clearConversation();
      setRightPanel('');
      useAgentStore.getState().reset();
    } catch (err) {
      console.error('New session failed:', err);
    }
  }, [createSession, clearConversation, setRightPanel]);

  const handleDislike = useCallback(async (content: string) => {
    try {
      await invoke('reinforce', { preference: content.substring(0, 500) });
    } catch (err) {
      console.error('Reinforce failed:', err);
    }
  }, []);

  const handleSelectSession = useCallback(async (id: string) => {
    if (id === activeSessionId) return;
    try {
      const data = await switchSession(id);
      if (!data) return;

      clearMessages();
      setRightPanel('');
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
        navigator.clipboard.writeText(rightPanelContent).catch(() => {});
        break;
      case 'sessions':
        setSidebarOpen(true);
        break;
      case 'cost':
        setRightPanel(
          `Maliyet Raporu\n${'='.repeat(30)}\n` +
          `Toplam token:  ${(tokensIn + tokensOut).toLocaleString()}\n` +
          `Girdi token:   ${tokensIn.toLocaleString()}\n` +
          `Cikti token:   ${tokensOut.toLocaleString()}\n` +
          `Toplam maliyet: $${cost.toFixed(6)}\n` +
          `Tur sayisi:    ${turnCount}\n` +
          `Model:         ${model}`
        );
        break;
      case 'model-fast':
        useAgentStore.getState().setModel('deepseek-v4-flash');
        break;
      case 'model-pro':
        useAgentStore.getState().setModel('deepseek-v4-pro');
        break;
      default:
        break;
    }
  }, [handleNewSession, setRightPanel, rightPanelContent, tokensIn, tokensOut, cost, turnCount, model]);

  // Fetch sessions on mount
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
      if (e.key === 'Escape') {
        setCmdOpen(false);
        setSidebarOpen(false);
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [handleNewSession]);

  return (
    <div className="app">
      {/* Sidebar */}
      <Sidebar
        isOpen={sidebarOpen}
        onToggle={() => setSidebarOpen((v) => !v)}
        sessions={sessions}
        activeSessionId={activeSessionId}
        onSelectSession={handleSelectSession}
      />

      {/* Command Palette */}
      {cmdOpen && <CommandPalette onCommand={handleCommand} onClose={() => setCmdOpen(false)} />}

      {/* LEFT PANEL */}
      <div className="left-panel">
        <div className="panel-header">
          <span className="panel-header-title">goblin</span>
          <div className="panel-header-actions">
            <button className="header-btn" onClick={() => setSidebarOpen(true)}>oturumlar</button>
            <button className="header-btn" onClick={() => setCmdOpen(true)}>⌘K</button>
            <button className="header-btn" onClick={() => clearConversation()}>temizle</button>
            <button className="header-btn" onClick={handleNewSession}>yeni</button>
          </div>
        </div>

        <ChatPanel messages={messages} onDislike={handleDislike} />

        <GoblinCharacter
          state={goblinState}
          text={goblin.text}
          detail={goblin.detail}
          isAnimating={isAnimating}
        />

        <InputBar
          input={input}
          onInputChange={setInput}
          onSend={handleSend}
          disabled={isAnimating || isStreaming}
        />
      </div>

      {/* RIGHT PANEL */}
      <OutputPanel
        content={rightPanelContent}
        onCopy={() => navigator.clipboard.writeText(rightPanelContent).catch(() => {})}
        onClear={() => setRightPanel('')}
      />

      {/* STATUS BAR */}
      <StatusBar
        state={goblinState}
        stateText={goblin.text}
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
