import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface WaUser { jid: string; name: string; }
interface WaContact { jid: string; last_message: string; last_ts: number; unread: number; }
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
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const msgEndRef = useRef<HTMLDivElement>(null);
  const selectedJidRef = useRef<string | null>(null);

  selectedJidRef.current = selectedJid;

  const loadContacts = useCallback(async () => {
    try {
      const c = await invoke<WaContact[]>('whatsapp_list_contacts');
      setContacts(c);
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
  const selectedContact = contacts.find((c) => c.jid === selectedJid);

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
                <button className="wa-new-btn" title="New conversation" onClick={() => setShowNewJid((v) => !v)}>+</button>
              </div>
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
              {contacts.map((c) => (
                <div
                  key={c.jid}
                  className={`wa-contact-item ${selectedJid === c.jid ? 'wa-contact-active' : ''}`}
                  onClick={() => selectContact(c.jid)}
                >
                  <div className="wa-contact-jid">{formatJid(c.jid)}</div>
                  <div className="wa-contact-last">{c.last_message.slice(0, 40)}</div>
                  <div className="wa-contact-time">{formatTimestamp(c.last_ts)}</div>
                </div>
              ))}
            </div>

            {/* Conversation */}
            <div className="wa-conversation">
              {!selectedJid ? (
                <div className="wa-conv-empty">Select a conversation</div>
              ) : (
                <>
                  <div className="wa-conv-header">
                    <span className="wa-conv-name">{selectedContact ? formatJid(selectedContact.jid) : formatJid(selectedJid)}</span>
                  </div>
                  <div className="wa-messages">
                    {history.length === 0 && <div className="wa-empty">No messages</div>}
                    {history.map((m) => (
                      <div key={m.id} className={`wa-msg ${m.direction === 'out' ? 'wa-msg-me' : ''}`}>
                        <div className="wa-msg-from">{m.direction === 'out' ? 'You' : formatJid(m.jid)}</div>
                        <div className="wa-msg-text">{m.text}</div>
                        <div className="wa-msg-time">{formatTimestamp(m.timestamp_ms)}</div>
                      </div>
                    ))}
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
                      className="wa-btn wa-btn-primary"
                      disabled={sending || !sendText.trim()}
                      onClick={handleSend}
                    >
                      {sending ? '...' : 'Gönder'}
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

function formatTimestamp(ts: number): string {
  return new Date(ts).toLocaleTimeString('tr-TR', { hour: '2-digit', minute: '2-digit' });
}
