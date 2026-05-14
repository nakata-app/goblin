import { useRef, useCallback, useEffect, useState } from 'react';

interface InputBarProps {
  input: string;
  onInputChange: (value: string) => void;
  onSend: () => void;
  disabled?: boolean;
  onFileAttach?: (file: File) => void;
  onOpenPalette?: () => void;
}

export function InputBar({ input, onInputChange, onSend, disabled, onFileAttach, onOpenPalette }: InputBarProps) {
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const [focused, setFocused] = useState(false);
  const [attachedFileName, setAttachedFileName] = useState<string | null>(null);

  const adjustHeight = useCallback(() => {
    const el = inputRef.current;
    if (el) {
      el.style.height = 'auto';
      el.style.height = Math.min(el.scrollHeight, 150) + 'px';
    }
  }, []);

  useEffect(() => {
    adjustHeight();
  }, [input, adjustHeight]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        if (!disabled) onSend();
      }
      if (e.key === 'Escape') {
        inputRef.current?.blur();
      }
      // Slash on empty input -> open command palette (intuitive shortcut)
      if (e.key === '/' && !input && onOpenPalette) {
        e.preventDefault();
        onOpenPalette();
      }
    },
    [onSend, disabled, input, onOpenPalette]
  );

  const [dragOver, setDragOver] = useState(false);

  return (
    <div
      className={`input-area ${focused ? 'input-focused' : ''}${dragOver ? ' input-dragover' : ''}`}
      onDragOver={(e) => {
        if (Array.from(e.dataTransfer.types).includes('Files')) {
          e.preventDefault();
          setDragOver(true);
        }
      }}
      onDragLeave={() => setDragOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setDragOver(false);
        const f = e.dataTransfer.files?.[0];
        if (f) {
          setAttachedFileName(f.name);
          onFileAttach?.(f);
        }
      }}
    >
      {attachedFileName && (
        <div className="attach-preview">
          <span className="attach-name">{attachedFileName}</span>
          <button className="attach-remove" onClick={() => setAttachedFileName(null)}>×</button>
        </div>
      )}
      <div className="input-row">
        <button
          className="attach-btn"
          onClick={() => fileRef.current?.click()}
          disabled={disabled}
          title="Attach file"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
            <path d="M8.3 1.7L3.3 6.7a3 3 0 004.2 4.2L13 5.5A2 2 0 0010.2 2.8L5.8 7.2a1 1 0 001.4 1.4l3.5-3.5" strokeLinecap="round" strokeLinejoin="round"/>
          </svg>
        </button>
        <input
          ref={fileRef}
          type="file"
          className="attach-input"
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) {
              setAttachedFileName(f.name);
              onFileAttach?.(f);
            }
            e.target.value = '';
          }}
        />
        <textarea
          ref={inputRef}
          className="chat-input"
          value={input}
          onChange={(e) => { onInputChange(e.target.value); }}
          onKeyDown={handleKeyDown}
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
          placeholder="Ask something or give a task..."
          rows={1}
          disabled={disabled}
        />
        <button
          className="send-btn"
          onClick={onSend}
          disabled={disabled}
        >
          <span className="send-btn-icon">↑</span>
        </button>
      </div>
      <div className={`input-hint ${focused || !input ? 'input-hint-show' : ''}`}>
        <kbd>Enter</kbd> send <span className="input-hint-sep">·</span>
        <kbd>⇧Enter</kbd> newline <span className="input-hint-sep">·</span>
        <kbd>⌘K</kbd> commands
      </div>
    </div>
  );
}
