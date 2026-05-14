import type { GoblinState } from '../types';

interface StatusBarProps {
  state: GoblinState;
  stateText: string;
  model: string;
  turnCount: number;
  cost: number;
  tokensIn: number;
  tokensOut: number;
  activeTool: string | null;
  error: string | null;
  onRetry?: () => void;
}

export function StatusBar({
  state,
  stateText,
  model,
  turnCount,
  cost,
  tokensIn,
  tokensOut,
  activeTool,
  error,
  onRetry,
}: StatusBarProps) {
  return (
    <div className="status-bar">
      <div className="status-left">
        <div className="status-indicator">
          <div
            className={`status-dot ${
              state === 'thinking' ? 'status-thinking' :
              state === 'error' ? 'status-error' :
              state === 'success' ? 'status-success' : ''
            }`}
          />
          <span className="status-state">{stateText}</span>
        </div>
        {activeTool && (
          <span className="status-tool">
            <span className="status-tool-dot" />
            {activeTool}
          </span>
        )}
        {error && (
          <span className="status-error-text" title={error}>
            {error.length > 40 ? error.slice(0, 40) + '...' : error}
            {onRetry && (
              <button className="status-retry-btn" onClick={onRetry} title="Retry last message">⟳</button>
            )}
          </span>
        )}
      </div>
      <div className="status-right">
        <span className="status-item">model: {model}</span>
        <span className="status-divider" />
        <span className="status-item">turn: {turnCount}</span>
        <span className="status-divider" />
        <span className="status-item">cost: ${cost.toFixed(4)}</span>
        <span className="status-divider" />
        <span className="status-item">
          tk: {(tokensIn + tokensOut).toLocaleString()}
        </span>
      </div>
    </div>
  );
}
