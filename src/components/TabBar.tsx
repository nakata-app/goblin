// Horizontal session-tab strip rendered above the chat panel.
// Phase A: pure presentation + click handlers. State lives in
// tabsStore + sessionStore; this just reflects it.

import { useTabsStore } from '../stores/tabsStore';
import { useSessionStore } from '../stores/sessionStore';
import { useAgentStore } from '../stores/agentStore';

interface TabBarProps {
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onNew: () => void;
}

export function TabBar({ onSelect, onClose, onNew }: TabBarProps) {
  const openTabs = useTabsStore((s) => s.openTabs);
  const cache = useTabsStore((s) => s.cache);
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const goblinState = useAgentStore((s) => s.goblinState);

  if (openTabs.length === 0) {
    return (
      <div className="tabbar">
        <button className="tab-new" onClick={onNew} title="New session">+</button>
      </div>
    );
  }

  const isStreaming = (id: string) =>
    id === activeSessionId &&
    (goblinState === 'streaming' || goblinState === 'thinking' || goblinState === 'running');

  return (
    <div className="tabbar">
      {openTabs.map((id) => {
        const snap = cache[id];
        const isActive = id === activeSessionId;
        const label = (snap?.title && snap.title.trim()) || 'Untitled';
        const short = label.length > 24 ? label.slice(0, 22) + '…' : label;
        const msgCount = snap?.messages?.length ?? 0;
        const tooltip = msgCount > 0
          ? `${label}\n${msgCount} message${msgCount === 1 ? '' : 's'}`
          : label;
        return (
          <div
            key={id}
            className={`tab ${isActive ? 'tab-active' : ''}`}
            onClick={() => onSelect(id)}
            title={tooltip}
          >
            {isStreaming(id) && <span className="tab-streaming-dot" />}
            <span className="tab-label">{short}</span>
            {msgCount > 0 && (
              <span className="tab-count-pill">{msgCount > 99 ? '99+' : msgCount}</span>
            )}
            <button
              className="tab-close"
              onClick={(e) => {
                e.stopPropagation();
                onClose(id);
              }}
              title="Close tab"
            >×</button>
          </div>
        );
      })}
      <button className="tab-new" onClick={onNew} title="New session">+</button>
    </div>
  );
}
