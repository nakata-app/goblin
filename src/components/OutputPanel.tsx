import { useRef, useEffect, useMemo } from 'react';
import type { GoblinState } from '../types';

export function highlightMarkdown(text: string): string {
  let html = text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');

  const lines = html.split('\n');
  const result: string[] = [];
  let inCodeBlock = false;
  let codeLang = '';

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    if (line.startsWith('```')) {
      if (inCodeBlock) {
        result.push('</code></pre></div>');
        inCodeBlock = false;
      } else {
        codeLang = line.slice(3).trim();
        result.push(`<div class="highlight-block"><div class="highlight-header"><span class="highlight-lang">${codeLang || 'code'}</span></div><pre class="highlight-code"><code>`);
        inCodeBlock = true;
      }
      result.push('\n');
      continue;
    }

    if (inCodeBlock) {
      result.push(line + '\n');
      continue;
    }

    let processed = line;

    processed = processed.replace(/`([^`]+)`/g, '<code class="inline-code">$1</code>');

    processed = processed.replace(/\*\*\*([^*]+)\*\*\*/g, '<strong><em>$1</em></strong>');
    processed = processed.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
    processed = processed.replace(/\*([^*]+)\*/g, '<em>$1</em>');

    if (processed.match(/^#{1,6}\s/)) {
      const level = (processed.match(/^(#+)/) || [''])[0].length;
      const text = processed.replace(/^#+\s*/, '');
      processed = `<h${level} class="md-heading h${level}">${text}</h${level}>`;
    }
    else if (processed.match(/^[-*_]{3,}\s*$/)) {
      processed = '<hr class="md-hr" />';
    }
    else if (processed.startsWith('&gt;')) {
      processed = `<blockquote class="md-quote">${processed.replace(/^&gt;\s?/, '')}</blockquote>`;
    }
    else if (processed.match(/^[\s]*[-*+]\s/)) {
      processed = processed.replace(/^([\s]*)([-*+])\s(.*)/, '$1<span class="md-bullet">$2</span> $3');
    }
    else if (processed.match(/^[\s]*\d+\.\s/)) {
      processed = processed.replace(/^([\s]*)(\d+)(\.\s)(.*)/, '$1<span class="md-number">$2.</span> $4');
    }
    else {
      processed = processed.replace(
        /\[([^\]]+)\]\(([^)]+)\)/g,
        '<a class="md-link" href="$2" target="_blank" rel="noopener">$1</a>'
      );
    }

    if (processed.includes('[stderr]')) {
      processed = processed.replace(/\[stderr\]/, '<span class="shell-stderr">[stderr]</span>');
    }
    if (processed.includes('[exit code:')) {
      processed = processed.replace(/\[exit code: (\d+)\]/, '<span class="shell-exit">[exit code: $1]</span>');
    }

    result.push(processed + '\n');
  }

  if (inCodeBlock) {
    result.push('</code></pre></div>');
  }

  return result.join('');
}

interface OutputPanelProps {
  content: string;
  onCopy: () => void;
  onClear: () => void;
  goblinState?: GoblinState;
  width?: number;
}

export function OutputPanel({ content, onCopy, onClear, goblinState, width }: OutputPanelProps) {
  const outputRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (outputRef.current) {
      const el = outputRef.current;
      const isNearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 50;
      if (isNearBottom) {
        el.scrollTop = el.scrollHeight;
      }
    }
  }, [content]);

  const hasContent = content.trim().length > 0;
  const highlighted = useMemo(() => hasContent ? highlightMarkdown(content) : '', [content, hasContent]);
  const state = goblinState || 'idle';

  const STATE_EMOJI: Record<string, string> = {
    idle: '👺',
    thinking: '🤔',
    reading: '📖',
    writing: '✍️',
    searching: '🔍',
    running: '⚡',
    error: '😱',
    success: '😎',
  };

  const STATE_LABEL: Record<string, string> = {
    idle: 'Ready',
    thinking: 'Thinking...',
    reading: 'Reading...',
    writing: 'Writing...',
    searching: 'Searching...',
    running: 'Running...',
    error: 'Error',
    success: 'Done!',
  };

  const isAnimating = state !== 'idle' && state !== 'error' && state !== 'success';

  return (
    <div className="right-panel" style={width ? { width: `${width}%` } : undefined}>
      <div className="panel-header">
        <span className="panel-header-title">output</span>
        <div className="panel-header-actions">
          <button className="header-btn" onClick={onCopy} disabled={!hasContent}>
            copy
          </button>
          <button className="header-btn" onClick={onClear} disabled={!hasContent}>
            clear
          </button>
        </div>
      </div>
      <div className="right-content" ref={outputRef}>
        {hasContent ? (
          <div
            className="output-rendered"
            dangerouslySetInnerHTML={{ __html: highlighted }}
          />
        ) : (
          <div className={`goblin-live ${isAnimating ? 'goblin-live-active' : ''} goblin-live-${state}`}>
            <div className="goblin-live-container">
              <div className="goblin-live-ring-outer">
                <div className="goblin-live-ring-inner">
                  <div className={`goblin-live-face ${isAnimating ? 'goblin-live-pulse' : 'goblin-live-breathe'}`}>
                    <span className="goblin-live-emoji">{STATE_EMOJI[state]}</span>
                  </div>
                </div>
              </div>
              <div className="goblin-live-label">{STATE_LABEL[state]}</div>
              <div className="goblin-live-hint">⌘K for command palette</div>
            </div>
            {isAnimating && (
              <div className="goblin-live-particles">
                {Array.from({ length: 8 }, (_, i) => (
                  <div
                    key={i}
                    className="goblin-live-particle"
                    style={{
                      '--angle': `${i * 45}deg`,
                      '--delay': `${i * 0.15}s`,
                      '--size': `${6 + i % 3 * 3}px`,
                    } as React.CSSProperties}
                  />
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
