import { useEffect } from 'react';
import { useMemoryStore, type MemoryTab } from '../stores/memoryStore';

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  const now = Date.now();
  const diff = (now - d.getTime()) / 1000;
  if (diff < 60) return `${Math.floor(diff)}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return d.toLocaleString();
}

const TABS: Array<{ key: MemoryTab; label: string }> = [
  { key: 'memories', label: 'Memories' },
  { key: 'observations', label: 'Observations' },
  { key: 'learned', label: 'Learned' },
];

export function MemoryPanel() {
  const open = useMemoryStore((s) => s.open);
  const setOpen = useMemoryStore((s) => s.setOpen);
  const tab = useMemoryStore((s) => s.tab);
  const setTab = useMemoryStore((s) => s.setTab);
  const query = useMemoryStore((s) => s.query);
  const setQuery = useMemoryStore((s) => s.setQuery);
  const search = useMemoryStore((s) => s.search);
  const memories = useMemoryStore((s) => s.memories);
  const observations = useMemoryStore((s) => s.observations);
  const learned = useMemoryStore((s) => s.learned);
  const stats = useMemoryStore((s) => s.stats);
  const loading = useMemoryStore((s) => s.loading);
  const error = useMemoryStore((s) => s.error);
  const removeMemory = useMemoryStore((s) => s.removeMemory);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, setOpen]);

  if (!open) return null;

  return (
    <div className="memory-overlay" role="dialog" aria-modal aria-label="Memory">
      <div className="memory-backdrop" onClick={() => setOpen(false)} />
      <div className="memory-drawer">
        <div className="memory-header">
          <h3 className="memory-title">Memory</h3>
          {stats && (
            <span className="memory-stats" title="Total memories">
              {stats.total} items · {stats.by_ns.length} namespaces
            </span>
          )}
          <button
            type="button"
            className="memory-close"
            onClick={() => setOpen(false)}
            aria-label="Close memory panel"
          >
            ×
          </button>
        </div>

        <div className="memory-tabs">
          {TABS.map((t) => (
            <button
              key={t.key}
              type="button"
              className={`memory-tab ${tab === t.key ? 'active' : ''}`}
              onClick={() => setTab(t.key)}
            >
              {t.label}
              {t.key === 'memories' && stats && (
                <span className="memory-tab-count">{stats.total}</span>
              )}
              {t.key === 'observations' && (
                <span className="memory-tab-count">{observations.length}</span>
              )}
              {t.key === 'learned' && (
                <span className="memory-tab-count">{learned.length}</span>
              )}
            </button>
          ))}
        </div>

        {tab === 'memories' && (
          <div className="memory-search-row">
            <input
              type="text"
              className="memory-search"
              placeholder="Search memories (FTS5 + semantic)…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') void search(query);
              }}
            />
            <button
              type="button"
              className="memory-search-btn"
              onClick={() => void search(query)}
              disabled={loading}
            >
              Search
            </button>
          </div>
        )}

        {error && <div className="memory-error">{error}</div>}

        <div className="memory-body">
          {loading && <div className="memory-loading">Loading…</div>}

          {tab === 'memories' && !loading && (
            <>
              {memories.length === 0 ? (
                <div className="memory-empty">No memories.</div>
              ) : (
                memories.map((m) => (
                  <div key={m.id} className="memory-item">
                    <div className="memory-item-head">
                      <span className="memory-ns">{m.ns}</span>
                      <span className="memory-tier" title="Tier">T{m.tier}</span>
                      <span className="memory-time">{formatTime(m.last_accessed)}</span>
                      <span className="memory-access" title="Access count">
                        ×{m.access_count}
                      </span>
                      <button
                        type="button"
                        className="memory-del"
                        title="Forget"
                        onClick={() => void removeMemory(m.id)}
                      >
                        ×
                      </button>
                    </div>
                    <div className="memory-text">{m.text}</div>
                  </div>
                ))
              )}
            </>
          )}

          {tab === 'observations' && !loading && (
            <>
              {observations.length === 0 ? (
                <div className="memory-empty">No observations yet.</div>
              ) : (
                observations.map((o) => (
                  <div key={o.id} className="memory-item">
                    <div className="memory-item-head">
                      <span className={`memory-tool ${o.success ? 'ok' : 'fail'}`}>
                        {o.tool_name}
                      </span>
                      <span className="memory-time">{formatTime(o.ts)}</span>
                      <span className="memory-session" title={o.session_id}>
                        {o.session_id.slice(0, 8)}
                      </span>
                    </div>
                    {o.args_summary && (
                      <div className="memory-sub">args: {o.args_summary}</div>
                    )}
                    {o.result_summary && (
                      <div className="memory-sub">→ {o.result_summary}</div>
                    )}
                  </div>
                ))
              )}
            </>
          )}

          {tab === 'learned' && !loading && (
            <>
              {learned.length === 0 ? (
                <div className="memory-empty">No reinforced preferences.</div>
              ) : (
                learned.map((l) => (
                  <div key={l.id} className="memory-item">
                    <div className="memory-item-head">
                      <span className="memory-reinforce" title="Reinforcement count">
                        ×{l.reinforcement_count}
                      </span>
                      <span className="memory-time">{formatTime(l.last_seen)}</span>
                    </div>
                    <div className="memory-text">{l.preference}</div>
                  </div>
                ))
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
