import { useState, useRef, useEffect, useCallback } from 'react';
import type { Message, ToolCall, GoblinState } from '../types';
import './styles/app.css';

function generateId(): string {
  return Math.random().toString(36).substring(2, 10);
}

function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString('tr-TR', { hour: '2-digit', minute: '2-digit' });
}

const GOBLIN_STATES: Record<GoblinState, { emoji: string; text: string; detail: string }> = {
  idle: { emoji: '👺', text: 'Hazır', detail: 'komut bekleniyor' },
  thinking: { emoji: '🤔', text: 'Düşünüyor', detail: 'model yanıtlıyor...' },
  reading: { emoji: '📖', text: 'Okuyor', detail: 'dosya taranıyor...' },
  writing: { emoji: '✍️', text: 'Yazıyor', detail: 'dosya düzenleniyor...' },
  searching: { emoji: '🔍', text: 'Araştırıyor', detail: 'aranıyor...' },
  running: { emoji: '⚡', text: 'Çalıştırıyor', detail: 'bash komutu...' },
  error: { emoji: '😱', text: 'Hata!', detail: 'bir şeyler ters gitti' },
  success: { emoji: '😎', text: 'Tamam!', detail: 'işlem başarılı' },
};

function App() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [goblinState, setGoblinState] = useState<GoblinState>('idle');
  const [rightPanelContent, setRightPanelContent] = useState('');
  const [model, setModel] = useState('deepseek-v4-flash');
  const [cost, setCost] = useState(0);
  const [turnCount, setTurnCount] = useState(0);
  const chatAreaRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (chatAreaRef.current) {
      chatAreaRef.current.scrollTop = chatAreaRef.current.scrollHeight;
    }
  }, [messages]);

  const handleSend = useCallback(() => {
    const text = input.trim();
    if (!text) return;

    const userMsg: Message = {
      id: generateId(),
      role: 'user',
      content: text,
      timestamp: Date.now(),
    };

    setMessages(prev => [...prev, userMsg]);
    setInput('');
    setGoblinState('thinking');
    setTurnCount(prev => prev + 1);

    // TODO: actual agent loop - for now echo back
    setTimeout(() => {
      const assistantMsg: Message = {
        id: generateId(),
        role: 'assistant',
        content: `[agent stub] received: "${text}"\n\nAgent loop not yet connected. Faz 2 will wire this to the LLM provider.`,
        timestamp: Date.now(),
        toolCalls: [],
      };
      setMessages(prev => [...prev, assistantMsg]);
      setGoblinState('idle');
      setRightPanelContent(prev => prev + `\n> ${text}\n[stub] agent not connected yet\n`);
    }, 800);
  }, [input]);

  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  }, [handleSend]);

  const goblin = GOBLIN_STATES[goblinState];
  const isAnimating = goblinState !== 'idle';

  return (
    <div className="app">
      {/* LEFT PANEL */}
      <div className="left-panel">
        <div className="panel-header">
          <span className="panel-header-title">goblin</span>
          <div className="panel-header-actions">
            <button className="header-btn">/map</button>
            <button className="header-btn">/cost</button>
            <button className="header-btn">/sessions</button>
          </div>
        </div>

        <div className="chat-area" ref={chatAreaRef}>
          {messages.length === 0 && (
            <div style={{ color: 'var(--text-dim)', textAlign: 'center', marginTop: 80, fontSize: 13 }}>
              Goblin hazır. Bir şey sor veya bir görev ver.
            </div>
          )}
          {messages.map(msg => (
            <div key={msg.id} className={`message message-${msg.role}`}>
              <div className="message-content">{msg.content}</div>
              <div className="message-meta">{formatTime(msg.timestamp)}</div>
              {msg.toolCalls?.map(tc => (
                <div key={tc.id} className="tool-call">
                  <div className={`tool-call-icon ${tc.status}`} />
                  <span>{tc.name}</span>
                  {tc.status === 'running' && <span style={{ color: 'var(--warning)' }}>...</span>}
                </div>
              ))}
            </div>
          ))}
        </div>

        {/* Goblin character strip */}
        <div className="goblin-strip">
          <div className={`goblin-avatar ${isAnimating ? 'animating' : ''}`}>
            {goblin.emoji}
          </div>
          <div className="goblin-status">
            <div className="goblin-status-text">{goblin.text}</div>
            <div className="goblin-status-detail">{goblin.detail}</div>
          </div>
        </div>

        {/* Input area */}
        <div className="input-area">
          <div className="input-row">
            <textarea
              ref={inputRef}
              className="chat-input"
              value={input}
              onChange={e => setInput(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Bir şey sor veya görev ver... (Enter: gönder, Shift+Enter: yeni satır)"
              rows={1}
            />
            <button
              className="send-btn"
              onClick={handleSend}
              disabled={!input.trim() || goblinState === 'thinking'}
            >
              Gönder
            </button>
          </div>
        </div>
      </div>

      {/* RIGHT PANEL */}
      <div className="right-panel">
        <div className="panel-header">
          <span className="panel-header-title">çıktı</span>
          <div className="panel-header-actions">
            <button className="header-btn">kopyala</button>
            <button className="header-btn">temizle</button>
          </div>
        </div>
        <div className="right-content">{rightPanelContent}</div>
      </div>

      {/* STATUS BAR */}
      <div className="status-bar" style={{ position: 'fixed', bottom: 0, left: 0, right: 0 }}>
        <div className="status-left">
          <div className="status-indicator">
            <div className={`status-dot ${goblinState === 'thinking' ? 'thinking' : goblinState === 'error' ? 'error' : ''}`} />
            <span>{goblin.text}</span>
          </div>
          <span>model: {model}</span>
        </div>
        <div className="status-right">
          <span>turn: {turnCount}</span>
          <span>cost: ${cost.toFixed(4)}</span>
        </div>
      </div>
    </div>
  );
}

export default App;
