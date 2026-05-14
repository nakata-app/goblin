import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Session } from '../types';

interface ProviderStatus {
  name: string;
  label: string;
  configured: boolean;
  keyCount: number;
}

interface SidebarProps {
  isOpen: boolean;
  onToggle: () => void;
  sessions: Session[];
  activeSessionId: string | null;
  onSelectSession: (id: string) => void;
}

const PROVIDER_ICONS: Record<string, string> = {
  openai: '⬡',
  anthropic: '⬢',
  nvidia: '⬟',
  gemini: '◆',
  glm: '◇',
};

const PROVIDER_LABELS: Record<string, string> = {
  openai: 'DeepSeek / OpenAI',
  anthropic: 'Anthropic',
  nvidia: 'NVIDIA',
  gemini: 'Gemini',
  glm: 'GLM',
};

export function Sidebar({ isOpen, onToggle, sessions, activeSessionId, onSelectSession }: SidebarProps) {
  const [providers, setProviders] = useState<ProviderStatus[]>([]);
  const [query, setQuery] = useState('');

  useEffect(() => {
    invoke<Record<string, unknown>>('get_config')
      .then((cfg) => {
        const provs = cfg?.providers as Record<string, Record<string, unknown>> | undefined;
        if (!provs) return;
        const list: ProviderStatus[] = [
          {
            name: 'openai',
            label: PROVIDER_LABELS['openai'] ?? 'OpenAI',
            configured: provs.openai != null && (provs.openai.api_key as string)?.length > 0,
            keyCount: Array.isArray(provs.openai?.key_pool) ? (provs.openai.key_pool as unknown[]).length : 0,
          },
          {
            name: 'anthropic',
            label: PROVIDER_LABELS['anthropic'] ?? 'Anthropic',
            configured: provs.anthropic != null,
            keyCount: Array.isArray(provs.anthropic?.key_pool) ? (provs.anthropic.key_pool as unknown[]).length : 0,
          },
          {
            name: 'nvidia',
            label: PROVIDER_LABELS['nvidia'] ?? 'NVIDIA',
            configured: provs.nvidia != null,
            keyCount: Array.isArray(provs.nvidia?.key_pool) ? (provs.nvidia.key_pool as unknown[]).length : 0,
          },
          {
            name: 'gemini',
            label: PROVIDER_LABELS['gemini'] ?? 'Gemini',
            configured: provs.gemini != null,
            keyCount: Array.isArray(provs.gemini?.key_pool) ? (provs.gemini.key_pool as unknown[]).length : 0,
          },
          {
            name: 'glm',
            label: PROVIDER_LABELS['glm'] ?? 'GLM',
            configured: provs.glm != null,
            keyCount: Array.isArray(provs.glm?.key_pool) ? (provs.glm.key_pool as unknown[]).length : 0,
          },
        ];
        setProviders(list);
      })
      .catch(() => {});
  }, [isOpen]);

  return (
    <>
      <div className={`sidebar-overlay ${isOpen ? 'sidebar-open' : ''}`} onClick={onToggle} />
      <div className={`sidebar ${isOpen ? 'sidebar-open' : ''}`}>
        {/* Brand */}
        <div className="sb-brand">
          <span className="sb-brand-icon">👺</span>
          <span className="sb-brand-text">Goblin</span>
        </div>

        {/* Navigation */}
        <div className="sb-nav">
          <div className="sb-nav-label">Navigation</div>
          <button className="sb-nav-item sb-nav-active" onClick={onToggle}>
            <span className="sb-nav-icon">💬</span>
            <span>Sessions</span>
          </button>
        </div>

        {/* Providers */}
        <div className="sb-nav">
          <div className="sb-nav-label">Providers</div>
          {providers.map((p) => (
            <div key={p.name} className={`sb-provider ${p.configured ? 'sb-provider-on' : 'sb-provider-off'}`}>
              <span className="sb-provider-icon">{PROVIDER_ICONS[p.name] ?? '●'}</span>
              <span className="sb-provider-name">{p.label}</span>
              <span className={`sb-provider-dot ${p.configured ? 'sb-dot-on' : 'sb-dot-off'}`} />
            </div>
          ))}
          {providers.length === 0 && (
            <div className="sb-provider sb-provider-off">
              <span className="sb-provider-name" style={{ opacity: 0.4 }}>Loading...</span>
            </div>
          )}
        </div>

        {/* Sessions */}
        <div className="sb-sessions">
          <div className="sb-nav-label">
            Recent Sessions
            <span className="sb-count">{sessions.length}</span>
          </div>
          <div className="sb-search-row">
            <input
              className="sb-search-input"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search sessions..."
            />
            {query && (
              <button className="sb-search-clear" onClick={() => setQuery('')}>×</button>
            )}
          </div>
          <div className="sb-session-list">
            {(() => {
              const q = query.trim().toLowerCase();
              const filtered = q
                ? sessions.filter((s) =>
                    (s.title ?? '').toLowerCase().includes(q) ||
                    (s.model ?? '').toLowerCase().includes(q))
                : sessions;
              if (filtered.length === 0) {
                return <div className="sb-empty">{q ? `No matches for "${query}"` : 'No sessions yet'}</div>;
              }
              return filtered.slice(0, 30).map((s) => (
                <div
                  key={s.id}
                  className={`sb-session-item ${s.id === activeSessionId ? 'sb-session-active' : ''}`}
                  onClick={() => onSelectSession(s.id)}
                >
                  <span className="sb-session-title">{s.title || 'Untitled'}</span>
                  <span className="sb-session-meta">
                    {s.messageCount} msgs · {s.model || '?'}
                  </span>
                </div>
              ));
            })()}
          </div>
        </div>

        {/* Footer */}
        <div className="sb-footer">
          <span className="sb-footer-text">Goblin v0.1</span>
        </div>
      </div>
    </>
  );
}
