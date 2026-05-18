import { useRef, useEffect, memo } from 'react';
import type { Message } from '../types';
import { useAgentStore } from '../stores/agentStore';
import { useChatStore } from '../stores/chatStore';

export function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString('tr-TR', { hour: '2-digit', minute: '2-digit' });
}

function renderChat(text: string): string {
  let out = text;

  // Extract fenced code blocks to protect them from inline replacements
  const blocks: string[] = [];
  out = out.replace(/```(\w*)\n?([\s\S]*?)```/g, (_m, lang, code) => {
    const escaped = esc(code.trim());
    const dataB64 = btoa(unescape(encodeURIComponent(code.trim())));
    const html =
      `<pre class="chat-code-block">` +
      `<button class="chat-code-copy" data-copy="${dataB64}" title="Copy">📋</button>` +
      (lang ? `<span class="chat-code-lang">${esc(lang)}</span>` : '') +
      `<code class="chat-code">${escaped}</code>` +
      `</pre>`;
    blocks.push(html);
    return `\x00BLOCK${blocks.length - 1}\x00`;
  });

  out = esc(out);

  // Restore code blocks
  out = out.replace(/\x00BLOCK(\d+)\x00/g, (_m, i) => blocks[Number(i)]);

  // Inline code
  out = out.replace(/`([^`]+)`/g, '<code class="chat-inline-code">$1</code>');

  // Bold+italic, bold, italic
  out = out.replace(/\*\*\*(.+?)\*\*\*/g, '<strong><em>$1</em></strong>');
  out = out.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/\*(.+?)\*/g, '<em>$1</em>');

  // Headings
  out = out.replace(/^### (.+)$/gm, '<h3 class="chat-heading chat-h3">$1</h3>');
  out = out.replace(/^## (.+)$/gm, '<h2 class="chat-heading chat-h2">$1</h2>');
  out = out.replace(/^# (.+)$/gm, '<h1 class="chat-heading chat-h1">$1</h1>');

  // Blockquote
  out = out.replace(/^&gt; (.+)$/gm, '<div class="chat-quote">$1</div>');

  // Horizontal rule
  out = out.replace(/^---$/gm, '<hr class="chat-hr">');

  // Unordered list
  out = out.replace(/^- (.+)$/gm, '<div class="chat-li">• $1</div>');
  out = out.replace(/^\d+\. (.+)$/gm, '<div class="chat-li-num">$1</div>');

  // Links
  out = out.replace(/\[(.+?)\]\((.+?)\)/g, '<a class="chat-link" href="$2" target="_blank" rel="noopener">$1</a>');

  // Paragraphs: double newline -> paragraph break, single newline -> <br>
  out = out.replace(/\n\n+/g, '</p><p>');
  out = out.replace(/\n/g, '<br/>');

  return `<p>${out}</p>`;
}

function esc(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

const GREETINGS = [
  "Ask something or give a task.",
  "Ready to work. What's next?",
  "At your service. Go ahead.",
  "Standing by. What do you need?",
  "Goblin here. Task me.",
  "Ready. Hit me with it.",
  "Awake and waiting.",
  "Let's build something.",
];

function randomGreeting(): string {
  return GREETINGS[Math.floor(Math.random() * GREETINGS.length)];
}

interface ChatPanelProps {
  messages: Message[];
  onContinue?: () => void;
}

export const ChatPanel = memo(function ChatPanel({ messages, onContinue }: ChatPanelProps) {
  const chatRef = useRef<HTMLDivElement>(null);
  const shouldAutoScroll = useRef(true);
  const goblinState = useAgentStore((s) => s.goblinState);
  const activeTool = useAgentStore((s) => s.activeTool);
  const lastMessage = messages[messages.length - 1];
  const showThinkingBubble =
    (goblinState === 'thinking' || goblinState === 'running' || goblinState === 'streaming') &&
    (!lastMessage || lastMessage.role === 'user');

  useEffect(() => {
    const el = chatRef.current;
    if (!el || !shouldAutoScroll.current) return;
    el.scrollTop = el.scrollHeight;
  }, [messages]);

  const handleScroll = useRef(() => {
    const el = chatRef.current;
    if (!el) return;
    const isNearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 50;
    shouldAutoScroll.current = isNearBottom;
  }).current;

  const handleClick = (e: React.MouseEvent) => {
    const t = e.target as HTMLElement;
    const btn = t.closest('.chat-code-copy') as HTMLButtonElement | null;
    if (btn) {
      const b64 = btn.getAttribute('data-copy') || '';
      try {
        const code = decodeURIComponent(escape(atob(b64)));
        navigator.clipboard?.writeText(code).then(() => {
          const orig = btn.textContent;
          btn.textContent = '✓';
          setTimeout(() => { btn.textContent = orig; }, 1200);
        });
      } catch { /* noop */ }
    }
  };

  return (
    <div className="chat-area" ref={chatRef} onScroll={handleScroll} onClick={handleClick}>
      {messages.length === 0 && (
        <div className="chat-empty">
          <div className="chat-empty-icon">👺</div>
          <div className="chat-empty-title">Goblin ready</div>
          <div className="chat-empty-sub">{randomGreeting()}</div>
        </div>
      )}
      {messages.map((msg) => (
        <div key={msg.id} className={`message message-${msg.role}${msg.queued ? ' message-queued' : ''}`}>
          <div className="message-content">
            <div className="chat-markdown" dangerouslySetInnerHTML={{ __html: renderChat(msg.content) }} />
            {msg.toolCalls && msg.toolCalls.length > 0 && (
              <div className="message-tools">
                {msg.toolCalls.map((tc) => (
                  <button
                    key={tc.id}
                    className={`tool-badge tool-${tc.status}`}
                    onClick={() => {
                      const name = tc.name || tc.function?.name || 'tool';
                      const argsStr = JSON.stringify(tc.args ?? {}, null, 2);
                      const resStr = tc.result ? tc.result.slice(0, 4000) : '(no result captured)';
                      const md =
                        `## Tool: \`${name}\`\n\n` +
                        `**Status:** ${tc.status}\n\n` +
                        `### Args\n\n\`\`\`json\n${argsStr}\n\`\`\`\n\n` +
                        `### Result\n\n\`\`\`\n${resStr}\n\`\`\``;
                      useChatStore.getState().setRightPanel(md);
                      useChatStore.getState().setActiveTab('output');
                    }}
                    title="Click for details"
                  >
                    <span className="tool-badge-icon">
                      {tc.status === 'running' ? '⋯' : tc.status === 'done' ? '✓' : tc.status === 'error' ? '✗' : '○'}
                    </span>
                    <span className="tool-badge-name">{tc.name || tc.function?.name}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
          <div className="message-meta">
            {formatTime(msg.timestamp)}
          </div>
        </div>
      ))}
      {!showThinkingBubble && lastMessage?.role === 'assistant' && goblinState === 'idle' && onContinue && (
        <div className="continue-row">
          <button className="continue-chip" onClick={onContinue} title="Continue the response">
            ↻ Continue
          </button>
        </div>
      )}
      {showThinkingBubble && (
        <div className="message message-assistant message-thinking" aria-live="polite">
          <div className="message-content thinking-bubble">
            <span className="thinking-dot" />
            <span className="thinking-dot" />
            <span className="thinking-dot" />
            {activeTool && (
              <span className="thinking-tool">{activeTool}</span>
            )}
          </div>
        </div>
      )}
    </div>
  );
});
