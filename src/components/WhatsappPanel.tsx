import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface WaUser {
  jid: string;
  name: string;
}

interface WaMessage {
  id: string;
  from: string;
  text: string;
  timestamp: number;
}

interface BridgeStatus {
  status: string;
  error: string | null;
  user: WaUser | null;
  qr: string | null;
}

interface SendResult {
  success: boolean;
  id: string | null;
  error: string | null;
}

interface Props {
  isOpen: boolean;
  onToggle: () => void;
}

const POLL_INTERVAL = 3000;

export function WhatsappPanel({ isOpen, onToggle }: Props) {
  const [status, setStatus] = useState<BridgeStatus | null>(null);
  const [messages, setMessages] = useState<WaMessage[]>([]);
  const [sendTo, setSendTo] = useState('');
  const [sendText, setSendText] = useState('');
  const [sending, setSending] = useState(false);
  const [loading, setLoading] = useState(false);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const msgEndRef = useRef<HTMLDivElement>(null);

  const poll = useCallback(async () => {
    try {
      const s = await invoke<BridgeStatus>('whatsapp_status');
      setStatus(s);

      if (s.status === 'connected') {
        const msgs = await invoke<WaMessage[]>('whatsapp_poll');
        if (msgs.length > 0) {
          setMessages((prev) => [...prev, ...msgs].slice(-200));
        }
      }
    } catch {
      // Bridge not running or not reachable
      setStatus({ status: 'stopped', error: null, user: null, qr: null });
    }
  }, []);

  useEffect(() => {
    if (!isOpen) {
      if (pollRef.current) clearInterval(pollRef.current);
      return;
    }
    poll();
    pollRef.current = setInterval(poll, POLL_INTERVAL);
    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
    };
  }, [isOpen, poll]);

  useEffect(() => {
    msgEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  const handleStart = async () => {
    setLoading(true);
    try {
      await invoke('whatsapp_start');
      await poll();
    } catch (e) {
      alert(`Failed to start: ${e}`);
    } finally {
      setLoading(false);
    }
  };

  const handleStop = async () => {
    setLoading(true);
    try {
      await invoke('whatsapp_stop');
      setStatus({ status: 'stopped', error: null, user: null, qr: null });
      setMessages([]);
    } catch (e) {
      alert(`Failed to stop: ${e}`);
    } finally {
      setLoading(false);
    }
  };

  const handleSend = async () => {
    if (!sendTo.trim() || !sendText.trim()) return;
    setSending(true);
    try {
      const result = await invoke<SendResult>('whatsapp_send', {
        jid: sendTo.trim(),
        text: sendText.trim(),
      });
      if (!result.success) {
        alert(`Send failed: ${result.error}`);
      } else {
        setMessages((prev) => [
          ...prev,
          { id: result.id ?? Date.now().toString(), from: 'me', text: sendText, timestamp: Date.now() },
        ]);
        setSendText('');
      }
    } catch (e) {
      alert(`Send error: ${e}`);
    } finally {
      setSending(false);
    }
  };

  const statusLabel = (s: string) => {
    switch (s) {
      case 'connected': return { text: 'Connected', cls: 'wa-status-ok' };
      case 'qr': return { text: 'Scan QR Code', cls: 'wa-status-warn' };
      case 'reconnecting': return { text: 'Reconnecting...', cls: 'wa-status-warn' };
      case 'logged_out': return { text: 'Logged Out', cls: 'wa-status-err' };
      case 'error': return { text: 'Error', cls: 'wa-status-err' };
      case 'stopped': return { text: 'Stopped', cls: '' };
      default: return { text: s, cls: '' };
    }
  };

  if (!isOpen) return null;

  const sl = statusLabel(status?.status ?? 'stopped');

  return (
    <>
      <div className="wa-overlay" onClick={onToggle} />
      <div className="wa-panel">
        <div className="wa-header">
          <span className="wa-title">WhatsApp</span>
          <div className="wa-header-actions">
            <span className={`wa-status-badge ${sl.cls}`}>{sl.text}</span>
            {status?.status === 'stopped' || status?.status === 'logged_out' || status?.status === 'error' ? (
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

        <div className="wa-body">
          {/* QR Code */}
          {status?.qr && status.status === 'qr' && (
            <div className="wa-qr-section">
              <div className="wa-qr-label">Scan with WhatsApp to connect</div>
              <img src={status.qr} alt="WhatsApp QR Code" className="wa-qr-image" />
              <div className="wa-qr-hint">Open WhatsApp → Settings → Linked Devices → Link a Device</div>
            </div>
          )}

          {/* Connected user info */}
          {status?.user && (
            <div className="wa-user-bar">
              <span className="wa-user-icon">💬</span>
              <span className="wa-user-name">{status.user.name || status.user.jid}</span>
            </div>
          )}

          {/* Error */}
          {status?.error && (
            <div className="wa-error">{status.error}</div>
          )}

          {/* Messages */}
          {status?.status === 'connected' && (
            <div className="wa-messages">
              {messages.length === 0 && (
                <div className="wa-empty">No messages yet</div>
              )}
              {messages.map((m) => (
                <div key={m.id} className={`wa-msg ${m.from === 'me' ? 'wa-msg-me' : ''}`}>
                  <div className="wa-msg-from">{m.from === 'me' ? 'You' : formatJid(m.from)}</div>
                  <div className="wa-msg-text">{m.text}</div>
                  <div className="wa-msg-time">{formatTimestamp(m.timestamp)}</div>
                </div>
              ))}
              <div ref={msgEndRef} />
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

        {/* Send bar */}
        {status?.status === 'connected' && (
          <div className="wa-send-bar">
            <input
              className="wa-send-to"
              value={sendTo}
              onChange={(e) => setSendTo(e.target.value)}
              placeholder="905551234567"
            />
            <input
              className="wa-send-text"
              value={sendText}
              onChange={(e) => setSendText(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') handleSend(); }}
              placeholder="Message..."
            />
            <button className="wa-btn wa-btn-primary" disabled={sending} onClick={handleSend}>
              {sending ? '...' : 'Send'}
            </button>
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
