import type { Session } from '../types';

interface SidebarProps {
  isOpen: boolean;
  onToggle: () => void;
  sessions: Session[];
  activeSessionId: string | null;
  onSelectSession: (id: string) => void;
}

export function Sidebar({ isOpen, onToggle, sessions, activeSessionId, onSelectSession }: SidebarProps) {
  return (
    <>
      <div className={`sidebar-overlay ${isOpen ? 'sidebar-open' : ''}`} onClick={onToggle} />
      <div className={`sidebar ${isOpen ? 'sidebar-open' : ''}`}>
        <div className="sidebar-header">
          <span className="sidebar-title">Sessions</span>
          <button className="sidebar-close" onClick={onToggle}>✕</button>
        </div>
        <div className="sidebar-list">
          {sessions.length === 0 && (
            <div className="sidebar-empty">No sessions yet</div>
          )}
          {sessions.map((s) => (
            <div
              key={s.id}
              className={`sidebar-item ${s.id === activeSessionId ? 'sidebar-item-active' : ''}`}
              onClick={() => onSelectSession(s.id)}
            >
              <div className="sidebar-item-title">{s.title || 'Untitled session'}</div>
              <div className="sidebar-item-meta">
                {s.messageCount} msgs &middot; {s.model || '?'}
              </div>
            </div>
          ))}
        </div>
      </div>
    </>
  );
}
