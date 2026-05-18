import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAgentStore } from '../stores/agentStore';

const APPROVAL_TIMEOUT_MS = 30_000;

function formatArgs(args: unknown): string {
  if (args === null || args === undefined) return '';
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args, null, 2);
  } catch {
    return String(args);
  }
}

export function ApprovalModal() {
  const pending = useAgentStore((s) => s.pendingApproval);
  const clearPending = useAgentStore((s) => s.setPendingApproval);
  const [responding, setResponding] = useState(false);
  const [remainingMs, setRemainingMs] = useState(APPROVAL_TIMEOUT_MS);

  // Recompute remaining time once per second while a request is open.
  useEffect(() => {
    if (!pending) return;
    const tick = () => {
      const elapsed = Date.now() - pending.requestedAt;
      setRemainingMs(Math.max(0, APPROVAL_TIMEOUT_MS - elapsed));
    };
    tick();
    const interval = setInterval(tick, 500);
    return () => clearInterval(interval);
  }, [pending]);

  const argsPreview = useMemo(
    () => (pending ? formatArgs(pending.args) : ''),
    [pending]
  );

  if (!pending) return null;

  const respond = async (approved: boolean) => {
    if (responding) return;
    setResponding(true);
    try {
      await invoke('tool_approval_response', { id: pending.id, approved });
    } catch (e) {
      // The Rust side may have already timed out and discarded the
      // request. Either way, the modal should close — the agent loop
      // has moved on.
      console.warn('approval response failed:', e);
    }
    clearPending(null);
    setResponding(false);
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') respond(false);
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) respond(true);
  };

  const seconds = Math.ceil(remainingMs / 1000);

  return (
    <div className="approval-overlay" onKeyDown={onKey} role="dialog" aria-modal>
      <div className="approval-card">
        <div className="approval-header">
          <span className="approval-icon" aria-hidden>!</span>
          <div>
            <h3 className="approval-title">Tool wants to run</h3>
            <code className="approval-tool">{pending.tool}</code>
          </div>
          <span className="approval-timer" title="Auto-reject after 30s">
            {seconds}s
          </span>
        </div>
        <pre className="approval-args">{argsPreview || '(no arguments)'}</pre>
        <div className="approval-actions">
          <button
            type="button"
            className="approval-btn approval-deny"
            onClick={() => respond(false)}
            disabled={responding}
          >
            Cancel (Esc)
          </button>
          <button
            type="button"
            className="approval-btn approval-allow"
            onClick={() => respond(true)}
            disabled={responding}
            autoFocus
          >
            Allow (⌘↵)
          </button>
        </div>
      </div>
    </div>
  );
}
