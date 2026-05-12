import { describe, it, expect, beforeEach, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { mockInvoke } from '../test/mocks/tauri';
import { useAgentStore } from '../stores/agentStore';
import { useChatStore } from '../stores/chatStore';

describe('agent loop - sendMessage', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    useAgentStore.setState({
      goblinState: 'idle',
      model: 'deepseek-v4-flash',
      cost: 0,
      turnCount: 0,
      tokensIn: 0,
      tokensOut: 0,
      activeTool: null,
      error: null,
    });
    useChatStore.setState({
      messages: [],
      input: '',
      rightPanelContent: '',
      isStreaming: false,
    });
    vi.useRealTimers();
  });

  it('kullanici mesaji chat\'e eklenir', async () => {
    mockInvoke.mockResolvedValueOnce({
      content: 'merhaba dunya',
      tool_calls: null,
      tokens_in: 10,
      tokens_out: 5,
      model: 'deepseek-v4-flash',
    });

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('selam');
      // Wait for double rAF + invoke to complete
      await new Promise(r => setTimeout(r, 100));
    });

    const messages = useChatStore.getState().messages;
    expect(messages).toHaveLength(2);
    expect(messages[0].role).toBe('user');
    expect(messages[0].content).toBe('selam');
    expect(messages[1].role).toBe('assistant');
    expect(messages[1].content).toBe('merhaba dunya');
  });

  it('agent dogru model ile cagrilir', async () => {
    useAgentStore.setState({ model: 'deepseek-v4-pro' });

    mockInvoke.mockResolvedValueOnce({
      content: 'ok',
      tool_calls: null,
      tokens_in: 5,
      tokens_out: 2,
      model: 'deepseek-v4-pro',
    });

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('test');
      await new Promise(r => setTimeout(r, 100));
    });

    expect(mockInvoke).toHaveBeenCalledWith('send_message', {
      message: 'test',
      model: 'deepseek-v4-pro',
    });
  });

  it('token ve maliyet dogru hesaplanir', async () => {
    mockInvoke.mockResolvedValueOnce({
      content: 'response',
      tool_calls: null,
      tokens_in: 1000,
      tokens_out: 500,
      model: 'deepseek-v4-flash',
    });

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('test');
      await new Promise(r => setTimeout(r, 100));
    });

    const state = useAgentStore.getState();
    expect(state.tokensIn).toBe(1000);
    expect(state.tokensOut).toBe(500);
    expect(state.cost).toBeGreaterThan(0);
    expect(state.turnCount).toBe(1);
  });

  it('tool_calls output panelinde goruntulenir', async () => {
    mockInvoke.mockResolvedValueOnce({
      content: 'islem yapildi',
      tool_calls: [
        {
          id: 'call_1',
          name: 'read_file',
          function: { name: 'read_file', arguments: '{"path":"/tmp/test.txt"}' },
          args: { path: '/tmp/test.txt' },
        },
      ],
      tokens_in: 20,
      tokens_out: 10,
      model: 'deepseek-v4-flash',
    });

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('dosyayi oku');
      await new Promise(r => setTimeout(r, 100));
    });

    const rightPanel = useChatStore.getState().rightPanelContent;
    expect(rightPanel).toContain('[TOOL]');
    expect(rightPanel).toContain('read_file');
  });

  it('hata durumunda error state\'e gecilir', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('API baglanti hatasi'));

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('test');
      await new Promise(r => setTimeout(r, 100));
    });

    const agentState = useAgentStore.getState();
    expect(agentState.goblinState).toBe('error');
    expect(agentState.error).toContain('API baglanti hatasi');

    const messages = useChatStore.getState().messages;
    expect(messages[messages.length - 1].content).toContain('Hata:');
  });

  it('state transition: idle -> thinking -> success -> idle', async () => {
    const states: string[] = [];
    const unsub = useAgentStore.subscribe((s) => {
      states.push(s.goblinState);
    });

    mockInvoke.mockResolvedValueOnce({
      content: 'done',
      tool_calls: null,
      tokens_in: 1,
      tokens_out: 1,
      model: 'deepseek-v4-flash',
    });

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      result.current.sendMessage('test');
      await new Promise(r => setTimeout(r, 100));
    });

    // thinking sonrasi success, 1500ms sonra idle
    expect(states).toContain('thinking');
    expect(states).toContain('success');

    await act(async () => {
      await new Promise(r => setTimeout(r, 1600));
    });

    expect(useAgentStore.getState().goblinState).toBe('idle');

    unsub();
  });

  it('clearConversation tum state\'i sifirlar', async () => {
    useAgentStore.setState({ cost: 5.0, turnCount: 3, tokensIn: 100, tokensOut: 50 });
    useChatStore.setState({
      messages: [
        { id: '1', role: 'user', content: 'hey', timestamp: Date.now() },
      ],
      rightPanelContent: 'output',
    });

    mockInvoke.mockResolvedValueOnce(undefined);

    const { useAgent } = await import('../hooks/useAgent');
    const { result } = renderHook(() => useAgent());

    await act(async () => {
      await result.current.clearConversation();
    });

    expect(mockInvoke).toHaveBeenCalledWith('clear_conversation');

    const agentState = useAgentStore.getState();
    expect(agentState.cost).toBe(0);
    expect(agentState.turnCount).toBe(0);

    const chatState = useChatStore.getState();
    expect(chatState.messages).toHaveLength(0);
    expect(chatState.rightPanelContent).toBe('');
  });
});
