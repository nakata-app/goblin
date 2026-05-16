// @ts-check
/// <reference lib="dom" />

const vscode = acquireVsCodeApi();

const messagesEl = /** @type {HTMLElement} */ (document.getElementById('messages'));
const inputEl = /** @type {HTMLTextAreaElement} */ (document.getElementById('input'));
const sendBtn = /** @type {HTMLButtonElement} */ (document.getElementById('send-btn'));
const statusDot = /** @type {HTMLElement} */ (document.getElementById('status-dot'));
const statusText = /** @type {HTMLElement} */ (document.getElementById('status-text'));

let thinking = false;

// Auto-resize textarea
inputEl.addEventListener('input', () => {
  inputEl.style.height = 'auto';
  inputEl.style.height = Math.min(inputEl.scrollHeight, 120) + 'px';
});

// Send on Enter, newline on Shift+Enter
inputEl.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    send();
  }
});

sendBtn.addEventListener('click', send);

function send() {
  const text = inputEl.value.trim();
  if (!text || thinking) return;

  appendUser(text);
  inputEl.value = '';
  inputEl.style.height = 'auto';
  setThinking(true);

  vscode.postMessage({ type: 'send', text });
}

function appendUser(text) {
  const el = document.createElement('div');
  el.className = 'msg msg-user';
  el.innerHTML = `<div class="msg-label">You</div><div class="msg-content">${escapeHtml(text)}</div>`;
  messagesEl.appendChild(el);
  scrollBottom();
}

function appendGoblin(content, model, tokensIn, tokensOut) {
  const el = document.createElement('div');
  el.className = 'msg msg-goblin';
  const meta = model ? `<div class="msg-meta">${escapeHtml(model)} · ${tokensIn}↑ ${tokensOut}↓</div>` : '';
  el.innerHTML = `<div class="msg-label">Goblin</div><div class="msg-content">${renderMarkdown(content)}</div>${meta}`;
  messagesEl.appendChild(el);
  scrollBottom();
}

function appendError(text) {
  const el = document.createElement('div');
  el.className = 'msg msg-error';
  el.textContent = `⚠ ${text}`;
  messagesEl.appendChild(el);
  scrollBottom();
}

function setThinking(on) {
  thinking = on;
  sendBtn.disabled = on;

  const existing = document.getElementById('thinking-indicator');
  if (on && !existing) {
    const el = document.createElement('div');
    el.id = 'thinking-indicator';
    el.className = 'thinking';
    el.innerHTML = `Goblin düşünüyor <span class="thinking-dots"><span>.</span><span>.</span><span>.</span></span>`;
    messagesEl.appendChild(el);
    scrollBottom();
  } else if (!on && existing) {
    existing.remove();
  }
}

function scrollBottom() {
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function setStatus(connected, label) {
  statusDot.className = connected ? 'connected' : '';
  statusText.textContent = label;
}

// Basic markdown: code blocks, inline code, bold, newlines
function renderMarkdown(text) {
  let html = escapeHtml(text);

  // Fenced code blocks ```lang\n...\n```
  html = html.replace(/```[\w]*\n?([\s\S]*?)```/g, (_, code) => {
    return `<pre><code>${code}</code></pre>`;
  });

  // Inline code `...`
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');

  // Bold **...**
  html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');

  // Newlines → <br> (outside pre blocks)
  html = html.replace(/\n/g, '<br>');

  return html;
}

function escapeHtml(text) {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Messages from extension host
window.addEventListener('message', (event) => {
  const msg = event.data;
  switch (msg.type) {
    case 'response':
      setThinking(false);
      appendGoblin(msg.content, msg.model, msg.tokens_in, msg.tokens_out);
      break;

    case 'error':
      setThinking(false);
      appendError(msg.text);
      break;

    case 'status':
      setStatus(msg.connected, msg.label);
      break;

    case 'clear':
      messagesEl.innerHTML = '';
      break;

    case 'inject':
      // Selection injected from editor
      inputEl.value = msg.text;
      inputEl.style.height = 'auto';
      inputEl.style.height = Math.min(inputEl.scrollHeight, 120) + 'px';
      inputEl.focus();
      break;
  }
});

// Ask extension to check connection on load
vscode.postMessage({ type: 'ready' });
inputEl.focus();
