import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock @tauri-apps/api/core before importing the store
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { useSessionStore } from '../stores/sessionStore';

describe('sessionStore', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useSessionStore.setState({
      sessions: [],
      activeSessionId: null,
      loading: false,
    });
  });

  it('setActiveSessionId gunceller', () => {
    useSessionStore.getState().setActiveSessionId('sess-123');
    expect(useSessionStore.getState().activeSessionId).toBe('sess-123');
  });

  it('fetchSessions session listesini doldurur', async () => {
    const mockSessions = [
      {
        id: 's1',
        title: 'Test Session',
        startedAt: 1715000000,
        endedAt: null,
        model: 'deepseek',
        messageCount: 5,
        cost: 0.01,
      },
    ];

    vi.mocked(invoke).mockResolvedValueOnce(mockSessions);

    await useSessionStore.getState().fetchSessions();

    const state = useSessionStore.getState();
    expect(state.sessions).toHaveLength(1);
    expect(state.sessions[0].id).toBe('s1');
    expect(state.sessions[0].title).toBe('Test Session');
    // timestamp converted from seconds to ms
    expect(state.sessions[0].startedAt).toBe(1715000000 * 1000);
    expect(state.loading).toBe(false);
  });

  it('fetchSessions handles error gracefully', async () => {
    vi.mocked(invoke).mockRejectedValueOnce(new Error('Network error'));

    await useSessionStore.getState().fetchSessions();

    const state = useSessionStore.getState();
    expect(state.sessions).toHaveLength(0);
    expect(state.loading).toBe(false);
  });

  it('createSession invokes backend and refreshes', async () => {
    const mockCreate = { id: 'new-sess', title: null, startedAt: 1715000000, endedAt: null, model: null, messageCount: 0, cost: 0 };
    const mockList = [
      { id: 'new-sess', title: null, startedAt: 1715000000, endedAt: null, model: null, messageCount: 0, cost: 0 },
    ];

    vi.mocked(invoke)
      .mockResolvedValueOnce(mockCreate)  // session_create
      .mockResolvedValueOnce(mockList);   // session_list

    await useSessionStore.getState().createSession();

    expect(vi.mocked(invoke)).toHaveBeenCalledWith('session_create');
    const state = useSessionStore.getState();
    expect(state.sessions).toHaveLength(1);
  });

  it('switchSession sets activeSessionId', async () => {
    const mockSwitch = {
      id: 'switched-sess',
      title: 'Switched',
      startedAt: 1715000000,
      endedAt: null,
      model: 'deepseek',
      tokensIn: 100,
      tokensOut: 50,
      cost: 0.05,
      messages: [],
    };
    const mockList = [{
      id: 'switched-sess', title: 'Switched', startedAt: 1715000000,
      endedAt: null, model: 'deepseek', messageCount: 0, cost: 0.05,
    }];

    vi.mocked(invoke)
      .mockResolvedValueOnce(mockSwitch)  // session_switch
      .mockResolvedValueOnce(mockList);   // session_list

    const result = await useSessionStore.getState().switchSession('switched-sess');

    expect(vi.mocked(invoke)).toHaveBeenCalledWith('session_switch', { id: 'switched-sess' });
    expect(result?.id).toBe('switched-sess');
    expect(useSessionStore.getState().activeSessionId).toBe('switched-sess');
  });

  it('deleteSession calls backend and refreshes', async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce(undefined)   // session_delete
      .mockResolvedValueOnce([]);         // session_list

    await useSessionStore.getState().deleteSession('del-sess');

    expect(vi.mocked(invoke)).toHaveBeenCalledWith('session_delete', { id: 'del-sess' });
  });
});
