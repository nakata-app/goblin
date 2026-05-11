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
  idle: { text: 'Ready', detail: 'waiting for command' },
  thinking: { text: 'Thinking', detail: 'model responding...' },
  reading: { text: 'Reading', detail: 'scanning files...' },
  writing: { text: 'Writing', detail: 'editing files...' },
  searching: { text: 'Searching', detail: 'searching...' },
  running: { text: 'Running', detail: 'executing command...' },
  error: { text: 'Error!', detail: 'something went wrong' },
  success: { text: 'Done!', detail: 'operation successful' },
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
        setRightPanel('## Model Changed\n\n**DeepSeek Flash** — optimized for fast responses.');
        break;
      case 'model-pro':
        useAgentStore.getState().setModel('deepseek-v4-pro');
        setRightPanel('## Model Changed\n\n**DeepSeek Pro** — optimized for complex analysis and coding.');
        break;
      case 'shortcuts':
        setRightPanel(
          `## Keyboard Shortcuts\n\n` +
          `### General\n` +
          `| Command | Shortcut |\n|--------|--------|\n` +
          `| Command palette | \`⌘K\` |\n` +
          `| New session | \`⌘N\` |\n` +
          `| Show sessions | \`⌘⇧S\` |\n` +
          `| Copy output | \`⌘⇧C\` |\n` +
          `| Close (palette/sidebar) | \`Esc\` |\n\n` +
          `### Chat\n` +
          `| Command | Shortcut |\n|--------|--------|\n` +
          `| Send message | \`Enter\` |\n` +
          `| New line | \`Shift+Enter\` |\n` +
          `| Blur input | \`Esc\` |\n\n` +
          `### Command Palette\n` +
          `| Command | Shortcut |\n|--------|--------|\n` +
          `| Move down | \`↓\` |\n` +
          `| Move up | \`↑\` |\n` +
          `| Select & run | \`Enter\` |`
        );
        break;
      case 'help':
        setRightPanel(
          `## Goblin Help\n\n` +
          `### Getting Started\n` +
          `- **⌘K** to open command palette\n` +
          `- **⌘N** to start a new session\n` +
          `- Type your message in chat, press **Enter** to send\n\n` +
          `### Tools\n` +
          `Goblin automatically uses tools when needed:\n` +
          `- \`read_file\`, \`write_file\`, \`edit_file\` — file operations\n` +
          `- \`bash\` — command execution\n` +
          `- \`git_status\`, \`git_diff\`, \`git_commit\` — git operations\n` +
          `- \`web_search\`, \`web_fetch\` — web search\n` +
          `- \`premortem\`, \`eisenhower\` — analysis tools\n\n` +
          `### Provider\n` +
          `Configure API keys in ~/.goblin/config.toml.`
        );
        break;
      case 'export':
        setRightPanel(
          `## Session Export\n\n` +
          `To export a session, call the **session_export** command from the Rust backend.\n\n` +
          `\`\`\`bash\n# Via Tauri API:\ninvoke('session_export', { outputPath: 'file.jsonl' })\n\`\`\`\n\n` +
          `Output will be in JSONL format.`
        );
        break;
      case 'premortem':
        setRightPanel(
          `## Premortem Analysis\n\n` +
          `To run a premortem, type in chat:\n\n` +
          `> "Run a premortem on this plan: **[plan details]**"\n\n` +
          `Goblin will automatically use the \`premortem\` tool to analyze every risk category.`
        );
        break;
      case 'eisenhower':
        setRightPanel(
          `## Eisenhower Matrix\n\n` +
          `To prioritize tasks, type in chat:\n\n` +
          `> "Place these tasks on the Eisenhower matrix:\n` +
          `> - [urgent+important] Fix production bug\n` +
          `> - [important] Write tests\n` +
          `> - [urgent] Reply to customer email\n` +
          `> - [ ] Browse Reddit"\n\n` +
          `Goblin will use the \`eisenhower\` tool to classify tasks.`
        );
        break;
      case 'repo-status':
        setRightPanel('To see git status, type "show git status" in chat.');
        break;
      case 'repo-log':
        setRightPanel('To see recent commits, type "show recent commits" in chat.');
        break;
      case 'map':
        setRightPanel(
          'To see the project map, type "show project file structure" in chat.\n\n' +
          'Goblin will use \`glob\` and \`read_file\` tools to analyze the structure.'
        );
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
      if (mod && e.shiftKey && e.key === 'S') {
        e.preventDefault();
        setSidebarOpen(true);
      }
      if (mod && e.shiftKey && e.key === 'C') {
        e.preventDefault();
        navigator.clipboard.writeText(rightPanelContent).catch(() => {});
      }
      if (mod && e.key === '/') {
        e.preventDefault();
        if (!rightPanelContent) {
          setRightPanel(
            `## Keyboard Shortcuts\n\n` +
            `| Command | Shortcut |\n|--------|--------|\n` +
            `| Command palette | ⌘K |\n` +
            `| New session | ⌘N |\n` +
            `| Show sessions | ⌘⇧S |\n` +
            `| Copy output | ⌘⇧C |\n` +
            `| Close | Esc |\n` +
            `| Send | Enter |\n` +
            `| New line | Shift+Enter |`
          );
        }
      }
      if (e.key === 'Escape') {
        setCmdOpen(false);
        setSidebarOpen(false);
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [handleNewSession, rightPanelContent, setRightPanel]);

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
            <button className="header-btn" onClick={() => setSidebarOpen(true)}>sessions</button>
            <button className="header-btn" onClick={() => setCmdOpen(true)}>⌘K</button>
            <button className="header-btn" onClick={() => clearConversation()}>clear</button>
            <button className="header-btn" onClick={handleNewSession}>new</button>
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
        goblinState={goblinState}
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
