import { describe, it, expect, beforeEach } from 'vitest';
import { useChatStore } from '../stores/chatStore';
import { useAgentStore } from '../stores/agentStore';

describe('chatStore', () => {
  beforeEach(() => {
    useChatStore.setState({
      messages: [],
      input: '',
      rightPanelContent: '',
      isStreaming: false,
    });
  });

  it('setInput input degerini gunceller', () => {
    useChatStore.getState().setInput('selam');
    expect(useChatStore.getState().input).toBe('selam');
  });

  it('addMessage mesaj listesine ekler', () => {
    const msg = { id: '1', role: 'user' as const, content: 'test', timestamp: Date.now() };
    useChatStore.getState().addMessage(msg);
    expect(useChatStore.getState().messages).toHaveLength(1);
    expect(useChatStore.getState().messages[0].content).toBe('test');
  });

  it('appendContent mesaj icerigine chunk ekler', () => {
    const msg = { id: '1', role: 'assistant' as const, content: 'mer', timestamp: Date.now() };
    useChatStore.getState().addMessage(msg);
    useChatStore.getState().appendContent('1', 'haba');
    expect(useChatStore.getState().messages[0].content).toBe('merhaba');
  });

  it('setRightPanel sag paneli gunceller', () => {
    useChatStore.getState().setRightPanel('tool output');
    expect(useChatStore.getState().rightPanelContent).toBe('tool output');
  });

  it('appendRightPanel saga ekleme yapar', () => {
    useChatStore.getState().setRightPanel('ilk');
    useChatStore.getState().appendRightPanel(' devam');
    expect(useChatStore.getState().rightPanelContent).toBe('ilk devam');
  });

  it('clearMessages tum mesaj ve paneli temizler', () => {
    useChatStore.getState().addMessage({ id: '1', role: 'user', content: 'test', timestamp: Date.now() });
    useChatStore.getState().setRightPanel('data');
    useChatStore.getState().clearMessages();
    expect(useChatStore.getState().messages).toHaveLength(0);
    expect(useChatStore.getState().rightPanelContent).toBe('');
  });

  it('setStreaming state gunceller', () => {
    expect(useChatStore.getState().isStreaming).toBe(false);
    useChatStore.getState().setStreaming(true);
    expect(useChatStore.getState().isStreaming).toBe(true);
  });
});

describe('agentStore', () => {
  beforeEach(() => {
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
  });

  it('setGoblinState gunceller', () => {
    useAgentStore.getState().setGoblinState('thinking');
    expect(useAgentStore.getState().goblinState).toBe('thinking');
  });

  it('addCost toplam maliyeti artirir', () => {
    useAgentStore.getState().addCost(0.5);
    useAgentStore.getState().addCost(0.3);
    expect(useAgentStore.getState().cost).toBeCloseTo(0.8);
  });

  it('incrementTurn tur sayisini artirir', () => {
    useAgentStore.getState().incrementTurn();
    useAgentStore.getState().incrementTurn();
    expect(useAgentStore.getState().turnCount).toBe(2);
  });

  it('addTokens girdi ve cikti tokenlarini biriktirir', () => {
    useAgentStore.getState().addTokens(100, 50);
    useAgentStore.getState().addTokens(200, 75);
    expect(useAgentStore.getState().tokensIn).toBe(300);
    expect(useAgentStore.getState().tokensOut).toBe(125);
  });

  it('setActiveTool aktif aracı gunceller', () => {
    useAgentStore.getState().setActiveTool('read_file');
    expect(useAgentStore.getState().activeTool).toBe('read_file');
    useAgentStore.getState().setActiveTool(null);
    expect(useAgentStore.getState().activeTool).toBeNull();
  });

  it('setError hata mesajini kaydeder', () => {
    useAgentStore.getState().setError('baglanti hatasi');
    expect(useAgentStore.getState().error).toBe('baglanti hatasi');
    useAgentStore.getState().setError(null);
    expect(useAgentStore.getState().error).toBeNull();
  });

  it('reset tum state\'i baslangica dondurur', () => {
    useAgentStore.setState({
      goblinState: 'running',
      cost: 10,
      turnCount: 5,
      tokensIn: 1000,
      tokensOut: 500,
      activeTool: 'bash',
      error: 'hata',
    });
    useAgentStore.getState().reset();
    const s = useAgentStore.getState();
    expect(s.goblinState).toBe('idle');
    expect(s.cost).toBe(0);
    expect(s.turnCount).toBe(0);
    expect(s.tokensIn).toBe(0);
    expect(s.tokensOut).toBe(0);
    expect(s.activeTool).toBeNull();
    expect(s.error).toBeNull();
  });

  it('setModel model ismini gunceller', () => {
    useAgentStore.getState().setModel('deepseek-v4-pro');
    expect(useAgentStore.getState().model).toBe('deepseek-v4-pro');
  });
});
