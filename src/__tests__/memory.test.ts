import { describe, it, expect, beforeEach, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import {
  useMemoryStore,
  type MemoryRecord,
  type ObservationRecord,
  type LearnedRecord,
  type MemoryStats,
} from '../stores/memoryStore';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

function resetStore() {
  useMemoryStore.setState({
    open: false,
    tab: 'memories',
    query: '',
    memories: [],
    observations: [],
    learned: [],
    stats: null,
    loading: false,
    error: null,
  });
}

const sampleMemory: MemoryRecord = {
  id: 'm1',
  ns: 'proj:goblin',
  tier: 2,
  text: 'Atakan prefers TypeScript over JS',
  meta: null,
  created: 1700000000,
  last_accessed: 1700001000,
  access_count: 4,
};

const sampleObs: ObservationRecord = {
  id: 'o1',
  ts: 1700000500,
  session_id: 'sess-abc12345',
  tool_name: 'bash',
  args_summary: 'ls -la',
  result_summary: 'listed 14 files',
  success: true,
};

const sampleLearned: LearnedRecord = {
  id: 'learn_xyz',
  preference: 'use rtk wrapper for git',
  reinforcement_count: 7,
  last_seen: 1700002000,
};

const sampleStats: MemoryStats = {
  total: 12,
  by_ns: [['proj:goblin', 8], ['global', 4]],
};

describe('memoryStore — initial state', () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it('defaults: closed, memories tab, empty lists', () => {
    const s = useMemoryStore.getState();
    expect(s.open).toBe(false);
    expect(s.tab).toBe('memories');
    expect(s.memories).toEqual([]);
    expect(s.observations).toEqual([]);
    expect(s.learned).toEqual([]);
    expect(s.stats).toBeNull();
  });

  it('setTab switches between tabs', () => {
    useMemoryStore.getState().setTab('observations');
    expect(useMemoryStore.getState().tab).toBe('observations');
    useMemoryStore.getState().setTab('learned');
    expect(useMemoryStore.getState().tab).toBe('learned');
  });

  it('setQuery stores the search term', () => {
    useMemoryStore.getState().setQuery('atakan');
    expect(useMemoryStore.getState().query).toBe('atakan');
  });
});

describe('memoryStore — refresh()', () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it('fans out 4 invokes in parallel and populates state', async () => {
    mockInvoke
      .mockResolvedValueOnce([sampleMemory])
      .mockResolvedValueOnce([sampleObs])
      .mockResolvedValueOnce([sampleLearned])
      .mockResolvedValueOnce(sampleStats);

    await useMemoryStore.getState().refresh();

    const s = useMemoryStore.getState();
    expect(s.memories).toEqual([sampleMemory]);
    expect(s.observations).toEqual([sampleObs]);
    expect(s.learned).toEqual([sampleLearned]);
    expect(s.stats).toEqual(sampleStats);
    expect(s.loading).toBe(false);
    expect(s.error).toBeNull();
    expect(mockInvoke).toHaveBeenCalledTimes(4);
  });

  it('records error on failure and clears loading', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('db locked'));
    mockInvoke.mockResolvedValue([]);

    await useMemoryStore.getState().refresh();

    const s = useMemoryStore.getState();
    expect(s.loading).toBe(false);
    expect(s.error).toContain('db locked');
  });
});

describe('memoryStore — setOpen()', () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it('opens the panel and triggers refresh', async () => {
    mockInvoke
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce({ total: 0, by_ns: [] });

    useMemoryStore.getState().setOpen(true);
    // refresh is fired async; wait a microtask cycle
    await Promise.resolve();
    await Promise.resolve();

    expect(useMemoryStore.getState().open).toBe(true);
    expect(mockInvoke).toHaveBeenCalledWith('memory_list', { limit: 100, offset: 0 });
    expect(mockInvoke).toHaveBeenCalledWith('memory_observations', { limit: 100 });
    expect(mockInvoke).toHaveBeenCalledWith('memory_learned', { limit: 100 });
    expect(mockInvoke).toHaveBeenCalledWith('memory_stats');
  });

  it('closing the panel does NOT trigger refresh', () => {
    useMemoryStore.setState({ open: true });
    useMemoryStore.getState().setOpen(false);
    expect(useMemoryStore.getState().open).toBe(false);
    expect(mockInvoke).not.toHaveBeenCalled();
  });
});

describe('memoryStore — search()', () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it('calls memory_search with the query and ns:null', async () => {
    mockInvoke.mockResolvedValueOnce([sampleMemory]);
    await useMemoryStore.getState().search('atakan');
    expect(mockInvoke).toHaveBeenCalledWith('memory_search', {
      query: 'atakan',
      ns: null,
    });
    expect(useMemoryStore.getState().memories).toEqual([sampleMemory]);
    expect(useMemoryStore.getState().query).toBe('atakan');
  });

  it('empty query falls back to refresh()', async () => {
    mockInvoke
      .mockResolvedValueOnce([sampleMemory])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce(sampleStats);

    await useMemoryStore.getState().search('  ');

    // Should have hit memory_list (from refresh), not memory_search.
    const calls = mockInvoke.mock.calls.map((c: unknown[]) => c[0]);
    expect(calls).toContain('memory_list');
    expect(calls).not.toContain('memory_search');
  });

  it('records error on search failure', async () => {
    mockInvoke.mockRejectedValueOnce('fts5 broken');
    await useMemoryStore.getState().search('atakan');
    expect(useMemoryStore.getState().error).toContain('fts5 broken');
    expect(useMemoryStore.getState().loading).toBe(false);
  });
});

describe('memoryStore — removeMemory()', () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it('invokes memory_remove then refreshes', async () => {
    mockInvoke
      .mockResolvedValueOnce(true) // memory_remove
      // refresh fan-out
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce(sampleStats);

    await useMemoryStore.getState().removeMemory('m1');

    expect(mockInvoke).toHaveBeenCalledWith('memory_remove', { id: 'm1' });
    // Followed by the 4 refresh invokes.
    expect(mockInvoke).toHaveBeenCalledTimes(5);
  });
});
