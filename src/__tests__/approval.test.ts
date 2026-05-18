import { describe, it, expect, beforeEach, vi } from 'vitest';
import { useAgentStore } from '../stores/agentStore';
import { useChatStore } from '../stores/chatStore';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

describe('approval flow', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    useAgentStore.getState().setPendingApproval(null);
  });

  it('store starts with no pending approval', () => {
    expect(useAgentStore.getState().pendingApproval).toBeNull();
  });

  it('setPendingApproval parks a request', () => {
    useAgentStore.getState().setPendingApproval({
      id: 'req-1',
      tool: 'bash',
      args: { command: 'ls' },
      requestedAt: Date.now(),
    });
    const p = useAgentStore.getState().pendingApproval;
    expect(p).not.toBeNull();
    expect(p?.id).toBe('req-1');
    expect(p?.tool).toBe('bash');
  });

  it('setPendingApproval(null) clears it', () => {
    useAgentStore.getState().setPendingApproval({
      id: 'req-2',
      tool: 'write_file',
      args: {},
      requestedAt: Date.now(),
    });
    useAgentStore.getState().setPendingApproval(null);
    expect(useAgentStore.getState().pendingApproval).toBeNull();
  });

  it('reset clears pendingApproval', () => {
    useAgentStore.getState().setPendingApproval({
      id: 'req-3',
      tool: 'bash',
      args: {},
      requestedAt: Date.now(),
    });
    useAgentStore.getState().reset();
    expect(useAgentStore.getState().pendingApproval).toBeNull();
  });

  it('invoke tool_approval_response payload shape', async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await invoke('tool_approval_response', { id: 'req-4', approved: true });
    expect(mockInvoke).toHaveBeenCalledWith('tool_approval_response', {
      id: 'req-4',
      approved: true,
    });
  });

  it('invoke tool_approval_response with rejection', async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await invoke('tool_approval_response', { id: 'req-5', approved: false });
    expect(mockInvoke).toHaveBeenLastCalledWith('tool_approval_response', {
      id: 'req-5',
      approved: false,
    });
  });
});

describe('pending attachments flow', () => {
  beforeEach(() => {
    useChatStore.getState().clearPendingAttachments();
  });

  it('starts empty', () => {
    expect(useChatStore.getState().pendingAttachments).toEqual([]);
  });

  it('addPendingAttachment appends to queue', () => {
    useChatStore.getState().addPendingAttachment({
      id: 'att-1',
      name: 'photo.png',
      mime_type: 'image/png',
      data: 'ZmFrZQ==',
      bytes: 4,
    });
    const q = useChatStore.getState().pendingAttachments;
    expect(q).toHaveLength(1);
    expect(q[0].name).toBe('photo.png');
    expect(q[0].data).toBe('ZmFrZQ==');
  });

  it('multiple attachments queue up in order', () => {
    useChatStore.getState().addPendingAttachment({
      id: 'a', name: 'one.png', mime_type: 'image/png', data: 'AA==', bytes: 1,
    });
    useChatStore.getState().addPendingAttachment({
      id: 'b', name: 'two.jpg', mime_type: 'image/jpeg', data: 'BB==', bytes: 1,
    });
    expect(useChatStore.getState().pendingAttachments.map((a) => a.id)).toEqual(['a', 'b']);
  });

  it('removePendingAttachment drops by id', () => {
    useChatStore.getState().addPendingAttachment({
      id: 'a', name: 'one.png', mime_type: 'image/png', data: 'AA==', bytes: 1,
    });
    useChatStore.getState().addPendingAttachment({
      id: 'b', name: 'two.jpg', mime_type: 'image/jpeg', data: 'BB==', bytes: 1,
    });
    useChatStore.getState().removePendingAttachment('a');
    const remaining = useChatStore.getState().pendingAttachments;
    expect(remaining).toHaveLength(1);
    expect(remaining[0].id).toBe('b');
  });

  it('clearPendingAttachments empties the queue', () => {
    useChatStore.getState().addPendingAttachment({
      id: 'a', name: 'one.png', mime_type: 'image/png', data: 'AA==', bytes: 1,
    });
    useChatStore.getState().clearPendingAttachments();
    expect(useChatStore.getState().pendingAttachments).toEqual([]);
  });
});
