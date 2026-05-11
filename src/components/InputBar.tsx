import { useRef, useCallback, useEffect, useState } from 'react';

interface InputBarProps {
  input: string;
  onInputChange: (value: string) => void;
  onSend: () => void;
  disabled: boolean;
  shortcuts?: { key: string; action: string }[];
}

export function InputBar({ input, onInputChange, onSend, disabled }: InputBarProps) {
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const [focused, setFocused] = useState(false);

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
    },
    [onSend, disabled]
  );

  return (
    <div className={`input-area ${focused ? 'input-focused' : ''}`}>
      <div className="input-row">
        <div className="input-wrapper">
          <textarea
            ref={inputRef}
            className="chat-input"
            value={input}
            onChange={(e) => onInputChange(e.target.value)}
            onKeyDown={handleKeyDown}
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
            placeholder="Ask something or give a task... (Enter: send, Shift+Enter: new line)"
            rows={1}
            disabled={disabled}
          />
          <div className="input-hint">
            <kbd>Enter</kbd> send &middot; <kbd>Shift</kbd>+<kbd>Enter</kbd> line &middot; <kbd>Esc</kbd> cancel
          </div>
        </div>
        <button
          className="send-btn"
          onClick={onSend}
          disabled={disabled}
        >
          <span className="send-btn-icon">↑</span>
        </button>
      </div>
    </div>
  );
}
