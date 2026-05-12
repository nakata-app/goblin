import type { Session } from '../types';

interface SessionPickerProps {
  sessions: Session[];
  onSelect: (id: string) => void;
  onNew: () => void;
}

export function formatTime(ts: number): string {
  return new Date(ts).toLocaleString('tr-TR', {
    month: 'short', day: 'numeric',
    hour: '2-digit', minute: '2-digit',
  });
}

export function SessionPicker({ sessions, onSelect, onNew }: SessionPickerProps) {
  const recent = sessions
    .filter(s => s.messageCount > 0)
    .slice(0, 5);

  if (recent.length === 0) {
    return null;
  }

  return (
    <div className="session-picker-overlay">
      <div className="session-picker">
        <div className="session-picker-header">
          <h3>Continue where you left off?</h3>
          <p>Pick a session or start fresh.</p>
        </div>

        <div className="session-picker-list">
          {recent.map(s => (
            <button
              key={s.id}
              className="session-picker-item"
              onClick={() => onSelect(s.id)}
            >
              <span className="spi-title">{s.title || 'Untitled'}</span>
              <span className="spi-meta">
                {s.messageCount} msg &middot; {formatTime(s.startedAt)}
                {(s.cost ?? 0) > 0 ? ` · $${(s.cost ?? 0).toFixed(3)}` : ''}
              </span>
            </button>
          ))}
        </div>

        <button className="session-picker-new" onClick={onNew}>
          + New Session
        </button>
      </div>
    </div>
  );
}
