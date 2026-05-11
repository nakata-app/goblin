import { useRef, useEffect } from 'react';
import type { Message } from '../types';

function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString('tr-TR', { hour: '2-digit', minute: '2-digit' });
}

interface ChatPanelProps {
  messages: Message[];
  onDislike?: (content: string) => void;
}

export function ChatPanel({ messages, onDislike }: ChatPanelProps) {
  const chatRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (chatRef.current) {
      chatRef.current.scrollTop = chatRef.current.scrollHeight;
    }
  }, [messages]);

  return (
    <div className="chat-area" ref={chatRef}>
      {messages.length === 0 && (
        <div className="chat-empty">
          <div className="chat-empty-icon">👺</div>
          <div className="chat-empty-title">Goblin hazir</div>
          <div className="chat-empty-sub">Bir sey sor veya bir gorev ver.</div>
        </div>
      )}
      {messages.map((msg) => (
        <div key={msg.id} className={`message message-${msg.role}`}>
          <div className="message-content">
            {msg.content}
            {msg.toolCalls && msg.toolCalls.length > 0 && (
              <div className="message-tools">
                {msg.toolCalls.map((tc) => (
                  <div key={tc.id} className={`tool-badge tool-${tc.status}`}>
                    <span className="tool-badge-icon">
                      {tc.status === 'running' ? '⋯' : tc.status === 'done' ? '✓' : tc.status === 'error' ? '✗' : '○'}
                    </span>
                    <span className="tool-badge-name">{tc.name || tc.function?.name}</span>
                  </div>
                ))}
              </div>
            )}
          </div>
          <div className="message-meta">
            {formatTime(msg.timestamp)}
            {msg.role === 'assistant' && onDislike && (
              <button
                className="msg-dislike-btn"
                title="Hatali / yanlis yonlendirme"
                onClick={() => onDislike(msg.content)}
              >
                👎
              </button>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
