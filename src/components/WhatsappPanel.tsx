import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface WaUser { jid: string; name: string; }
interface WaContact {
  jid: string;
  last_message: string;
  last_ts: number;
  unread: number;
  /** Display name from address book / pushName. null when unknown. */
  name?: string | null;
}
interface WaHistoryMessage { id: string; jid: string; direction: 'in' | 'out'; text: string; timestamp_ms: number; }

interface BridgeStatus {
  status: string;
  error: string | null;
  user: WaUser | null;
  qr: string | null;
}

interface SendResult { success: boolean; id: string | null; error: string | null; }

interface Props { isOpen: boolean; onToggle: () => void; }

const POLL_INTERVAL = 3000;

export function WhatsappPanel({ isOpen, onToggle }: Props) {
  const [status, setStatus] = useState<BridgeStatus | null>(null);
  const [contacts, setContacts] = useState<WaContact[]>([]);
  const [selectedJid, setSelectedJid] = useState<string | null>(null);
  const [history, setHistory] = useState<WaHistoryMessage[]>([]);
  const [sendText, setSendText] = useState('');
  const [sending, setSending] = useState(false);
  const [loading, setLoading] = useState(false);
  const [newJid, setNewJid] = useState('');
  const [showNewJid, setShowNewJid] = useState(false);
  const [autoReply, setAutoReply] = useState(false);
  const [search, setSearch] = useState('');
  // jid -> data URL string when loaded, null when unavailable.
  // undefined means "not fetched yet" so we know to request it.
  const [photoCache, setPhotoCache] = useState<Record<string, string | null>>({});
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const msgEndRef = useRef<HTMLDivElement>(null);
  const selectedJidRef = useRef<string | null>(null);
  const photoCacheRef = useRef(photoCache);

  selectedJidRef.current = selectedJid;
  photoCacheRef.current = photoCache;

  const loadContacts = useCallback(async () => {
    try {
      const c = await invoke<WaContact[]>('whatsapp_list_contacts');
      setContacts(c);

      // Fetch profile pictures for any contact we have not seen yet.
      // Bridge caches photos for 24h so this is cheap to call repeatedly.
      // Errors are swallowed; the avatar falls back to initials.
      const known = photoCacheRef.current;
      const missing = c.filter((x) => !(x.jid in known));
      for (const item of missing) {
        invoke<string | null>('whatsapp_profile_picture', { jid: item.jid })
          .then((photo) => {
            setPhotoCache((prev) => ({ ...prev, [item.jid]: photo ?? null }));
          })
          .catch(() => {
            setPhotoCache((prev) => ({ ...prev, [item.jid]: null }));
          });
      }
    } catch { /* bridge not running */ }
  }, []);

  const loadHistory = useCallback(async (jid: string) => {
    try {
      const h = await invoke<WaHistoryMessage[]>('whatsapp_get_history', { jid, limit: 100 });
      setHistory(h);
    } catch { /* ignore */ }
  }, []);

  const poll = useCallback(async () => {
    try {
      const s = await invoke<BridgeStatus>('whatsapp_status');
      setStatus(s);
      if (s.status === 'connected') {
        // wa_agent_loop owns bridge polling and db writes.
        // Frontend just refreshes from db to stay in sync.
        loadContacts();
        const cur = selectedJidRef.current;
        if (cur) loadHistory(cur);
      }
    } catch {
      setStatus({ status: 'stopped', error: null, user: null, qr: null });
    }
  }, [loadContacts, loadHistory]);

  useEffect(() => {
    if (!isOpen) {
      if (pollRef.current) clearInterval(pollRef.current);
      return;
    }
    poll();
    pollRef.current = setInterval(poll, POLL_INTERVAL);
    invoke<boolean>('whatsapp_get_auto_reply').then(setAutoReply).catch(() => {});
    loadContacts();
    return () => { if (pollRef.current) clearInterval(pollRef.current); };
  }, [isOpen, poll, loadContacts]);

  useEffect(() => {
    msgEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [history]);

  const selectContact = (jid: string) => {
    setSelectedJid(jid);
    loadHistory(jid);
    setSendText('');
  };

  const handleStart = async () => {
    setLoading(true);
    try { await invoke('whatsapp_start'); await poll(); }
    catch (e) { alert(`Failed to start: ${e}`); }
    finally { setLoading(false); }
  };

  const handleStop = async () => {
    setLoading(true);
    try {
      await invoke('whatsapp_stop');
      setStatus({ status: 'stopped', error: null, user: null, qr: null });
      setContacts([]);
      setSelectedJid(null);
      setHistory([]);
    } catch (e) { alert(`Failed to stop: ${e}`); }
    finally { setLoading(false); }
  };

  const handleSend = async () => {
    if (!selectedJid || !sendText.trim()) return;
    setSending(true);
    try {
      const result = await invoke<SendResult>('whatsapp_send', {
        jid: selectedJid,
        text: sendText.trim(),
      });
      if (!result.success) {
        alert(`Send failed: ${result.error}`);
      } else {
        const outMsg: WaHistoryMessage = {
          id: result.id ?? Date.now().toString(),
          jid: selectedJid,
          direction: 'out',
          text: sendText.trim(),
          timestamp_ms: Date.now(),
        };
        setHistory((prev) => [...prev, outMsg]);
        setSendText('');
        loadContacts();
      }
    } catch (e) { alert(`Send error: ${e}`); }
    finally { setSending(false); }
  };

  const statusLabel = (s: string) => {
    switch (s) {
      case 'connected': return { text: 'Connected', cls: 'wa-status-ok' };
      case 'qr': return { text: 'Scan QR', cls: 'wa-status-warn' };
      case 'reconnecting': return { text: 'Reconnecting...', cls: 'wa-status-warn' };
      case 'logged_out': return { text: 'Logged Out', cls: 'wa-status-err' };
      case 'error': return { text: 'Error', cls: 'wa-status-err' };
      default: return { text: 'Stopped', cls: '' };
    }
  };

  if (!isOpen) return null;

  const sl = statusLabel(status?.status ?? 'stopped');
  const connected = status?.status === 'connected';

  return (
    <>
      <div className="wa-overlay" onClick={onToggle} />
      <div className="wa-panel">
        {/* Header */}
        <div className="wa-header">
          <span className="wa-title">WhatsApp</span>
          <div className="wa-header-actions">
            <span className={`wa-status-badge ${sl.cls}`}>{sl.text}</span>
            {(!status || status.status === 'stopped' || status.status === 'logged_out' || status.status === 'error') ? (
              <button className="wa-btn wa-btn-primary" disabled={loading} onClick={handleStart}>
                {loading ? '...' : 'Start'}
              </button>
            ) : (
              <button className="wa-btn wa-btn-danger" disabled={loading} onClick={handleStop}>
                {loading ? '...' : 'Stop'}
              </button>
            )}
            <button className="wa-close" onClick={onToggle}>✕</button>
          </div>
        </div>

        {/* QR */}
        {status?.qr && status.status === 'qr' && (
          <div className="wa-qr-section">
            <div className="wa-qr-label">Scan with WhatsApp to connect</div>
            <img src={status.qr} alt="WhatsApp QR Code" className="wa-qr-image" />
            <div className="wa-qr-hint">WhatsApp → Settings → Linked Devices → Link a Device</div>
          </div>
        )}

        {/* User bar */}
        {status?.user && (
          <div className="wa-user-bar">
            <span className="wa-user-icon">💬</span>
            <span className="wa-user-name">{status.user.name || status.user.jid}</span>
            <label className="wa-toggle" title="Goblin benim adıma cevap versin">
              <input
                type="checkbox"
                checked={autoReply}
                onChange={async (e) => {
                  const val = e.target.checked;
                  await invoke('whatsapp_set_auto_reply', { enabled: val });
                  setAutoReply(val);
                }}
              />
              <span className="wa-toggle-label">{autoReply ? 'Otomatik: Açık' : 'Otomatik: Kapalı'}</span>
            </label>
          </div>
        )}

        {status?.error && <div className="wa-error">{status.error}</div>}

        {/* Main body: contacts + conversation */}
        {connected && (
          <div className="wa-main">
            {/* Contacts sidebar */}
            <div className="wa-contacts">
              <div className="wa-contacts-toolbar">
                <span className="wa-contacts-label">Chats</span>
                <span className="wa-contacts-count">{contacts.length}</span>
                <button className="wa-new-btn" title="New conversation" onClick={() => setShowNewJid((v) => !v)}>+</button>
              </div>
              {contacts.length > 0 && (
                <div className="wa-search-row">
                  <input
                    className="wa-search-input"
                    value={search}
                    onChange={(e) => setSearch(e.target.value)}
                    placeholder="Search chats..."
                  />
                  {search && (
                    <button className="wa-search-clear" onClick={() => setSearch('')} title="Clear">×</button>
                  )}
                </div>
              )}
              {showNewJid && (
                <div className="wa-new-jid">
                  <input
                    className="wa-new-jid-input"
                    value={newJid}
                    onChange={(e) => setNewJid(e.target.value)}
                    placeholder="905XXXXXXXXX"
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' && newJid.trim()) {
                        const jid = newJid.trim().includes('@') ? newJid.trim() : `${newJid.trim()}@s.whatsapp.net`;
                        selectContact(jid);
                        setNewJid('');
                        setShowNewJid(false);
                      }
                    }}
                    autoFocus
                  />
                </div>
              )}
              {contacts.length === 0 && !showNewJid && (
                <div className="wa-contacts-empty">Mesaj bekleniyor...</div>
              )}
              {(() => {
                const q = search.trim().toLowerCase();
                const filtered = q
                  ? contacts.filter((c) =>
                      displayLabel(c.jid, c.name).toLowerCase().includes(q) ||
                      formatJid(c.jid).toLowerCase().includes(q) ||
                      c.last_message.toLowerCase().includes(q))
                  : contacts;
                if (q && filtered.length === 0) {
                  return <div className="wa-contacts-empty">No matches for "{search}"</div>;
                }
                return filtered.map((c) => {
                  const label = displayLabel(c.jid, c.name);
                  return (
                    <div
                      key={c.jid}
                      className={`wa-contact-item ${selectedJid === c.jid ? 'wa-contact-active' : ''}`}
                      onClick={() => selectContact(c.jid)}
                    >
                      <Avatar jid={c.jid} name={c.name} photo={photoCache[c.jid]} size="md" />
                      <div className="wa-contact-body">
                        <div className="wa-contact-row">
                          <span className="wa-contact-jid">{label}</span>
                          <span className="wa-contact-time">{formatTimestamp(c.last_ts)}</span>
                        </div>
                        <div className="wa-contact-row">
                          <span className="wa-contact-last">{c.last_message.slice(0, 48)}</span>
                          {c.unread > 0 && (
                            <span className="wa-unread-badge">{c.unread > 99 ? '99+' : c.unread}</span>
                          )}
                        </div>
                      </div>
                    </div>
                  );
                });
              })()}
            </div>

            {/* Conversation */}
            <div className="wa-conversation">
              {!selectedJid ? (
                <div className="wa-conv-placeholder">
                  <div className="wa-conv-placeholder-icon">💬</div>
                  <div className="wa-conv-placeholder-title">Select a conversation</div>
                  <div className="wa-conv-placeholder-sub">Pick a chat on the left, or hit + to start a new one.</div>
                </div>
              ) : (
                <>
                  {(() => {
                    const selectedContact = contacts.find((c) => c.jid === selectedJid);
                    const selName = selectedContact?.name ?? null;
                    return (
                      <div className="wa-conv-header">
                        <div className="wa-avatar-wrap">
                          <Avatar
                            jid={selectedJid}
                            name={selName}
                            photo={photoCache[selectedJid]}
                            size="sm"
                          />
                          {autoReply && <span className="wa-online-dot" title="Auto-reply on" />}
                        </div>
                        <div className="wa-conv-meta">
                          <span className="wa-conv-name">{displayLabel(selectedJid, selName)}</span>
                          <span className="wa-conv-sub">
                            {autoReply ? 'Goblin replies automatically' : `${history.length} messages`}
                          </span>
                        </div>
                      </div>
                    );
                  })()}
                  <div className="wa-messages">
                    {history.length === 0 && (
                      <div className="wa-conv-fresh">
                        <div className="wa-conv-fresh-bubble">No messages yet — say hi 👋</div>
                      </div>
                    )}
                    {history.map((m, i) => {
                      const showFrom =
                        i === 0 || history[i - 1].direction !== m.direction;
                      return (
                        <div key={m.id} className={`wa-msg ${m.direction === 'out' ? 'wa-msg-me' : ''}`}>
                          {showFrom && (
                            <div className="wa-msg-from">
                              {m.direction === 'out'
                                ? 'You'
                                : displayLabel(m.jid, contacts.find((c) => c.jid === m.jid)?.name ?? null)}
                            </div>
                          )}
                          <div className="wa-msg-text">{m.text}</div>
                          <div className="wa-msg-foot">
                            <span className="wa-msg-time">{formatTimestamp(m.timestamp_ms)}</span>
                            {m.direction === 'out' && (
                              <span className="wa-msg-status" title="sent">
                                <svg width="14" height="10" viewBox="0 0 16 10" fill="none">
                                  <path d="M1 5 L4 8 L9 3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
                                  <path d="M6 5 L9 8 L14 3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
                                </svg>
                              </span>
                            )}
                          </div>
                        </div>
                      );
                    })}
                    <div ref={msgEndRef} />
                  </div>
                  <div className="wa-send-bar">
                    <input
                      className="wa-send-text"
                      value={sendText}
                      onChange={(e) => setSendText(e.target.value)}
                      onKeyDown={(e) => { if (e.key === 'Enter') handleSend(); }}
                      placeholder="Message..."
                    />
                    <button
                      className="wa-send-btn"
                      disabled={sending || !sendText.trim()}
                      onClick={handleSend}
                      title="Send (Enter)"
                    >
                      {sending ? (
                        <span className="wa-spinner" />
                      ) : (
                        <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
                          <path d="M1.5 8 L14.5 1.5 L11 14.5 L7.5 9 L1.5 8z" fill="currentColor"/>
                        </svg>
                      )}
                    </button>
                  </div>
                </>
              )}
            </div>
          </div>
        )}

        {/* Stopped state */}
        {(!status || status.status === 'stopped') && (
          <div className="wa-empty">
            <div className="wa-empty-icon">📱</div>
            <div>WhatsApp bridge is not running</div>
            <div className="wa-empty-hint">Press Start to connect</div>
          </div>
        )}
      </div>
    </>
  );
}

function formatJid(jid: string): string {
  return jid.replace(/@.*$/, '');
}

/// Display label for a contact: real name when known, otherwise the
/// phone number / @lid prefix. Centralised so contact list, conversation
/// header, and message bubbles all stay in sync.
function displayLabel(jid: string, name?: string | null): string {
  return name && name.trim().length > 0 ? name : formatJid(jid);
}

/// Small inline avatar component. Renders the contact's profile picture
/// when available, else the initials-colored circle fallback. The img
/// onError swap handles cases where the data URL turns out unrenderable.
function Avatar({
  jid,
  name,
  photo,
  size,
}: {
  jid: string;
  name?: string | null;
  photo?: string | null;
  size: 'sm' | 'md';
}) {
  const label = displayLabel(jid, name);
  const cls = size === 'sm' ? 'wa-avatar wa-avatar-sm' : 'wa-avatar';
  if (photo) {
    return (
      <div className={cls}>
        <img
          className="wa-avatar-img"
          src={photo}
          alt={label}
          loading="lazy"
          draggable={false}
        />
      </div>
    );
  }
  return (
    <div className={cls} style={{ background: avatarColor(jid) }}>
      {initialsFor(label)}
    </div>
  );
}

function formatTimestamp(ts: number): string {
  return new Date(ts).toLocaleTimeString('tr-TR', { hour: '2-digit', minute: '2-digit' });
}

function initialsFor(name: string): string {
  const clean = name.replace(/[^a-zA-Z0-9]/g, '');
  if (!clean) return '?';
  if (clean.length <= 2) return clean.toUpperCase();
  return (clean.slice(0, 1) + clean.slice(-1)).toUpperCase();
}

const AVATAR_PALETTE = [
  'linear-gradient(135deg, #10b981, #059669)',
  'linear-gradient(135deg, #6366f1, #4f46e5)',
  'linear-gradient(135deg, #ec4899, #db2777)',
  'linear-gradient(135deg, #f59e0b, #d97706)',
  'linear-gradient(135deg, #06b6d4, #0891b2)',
  'linear-gradient(135deg, #a855f7, #9333ea)',
  'linear-gradient(135deg, #ef4444, #dc2626)',
  'linear-gradient(135deg, #84cc16, #65a30d)',
];

function avatarColor(jid: string): string {
  let h = 0;
  for (let i = 0; i < jid.length; i++) h = (h * 31 + jid.charCodeAt(i)) >>> 0;
  return AVATAR_PALETTE[h % AVATAR_PALETTE.length];
}
