import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';

export interface MemoryRecord {
  id: string;
  ns: string;
  tier: number;
  text: string;
  meta: string | null;
  created: number;
  last_accessed: number;
  access_count: number;
}

export interface ObservationRecord {
  id: string;
  ts: number;
  session_id: string;
  tool_name: string;
  args_summary: string | null;
  result_summary: string | null;
  success: boolean;
}

export interface LearnedRecord {
  id: string;
  preference: string;
  reinforcement_count: number;
  last_seen: number;
}

export interface MemoryStats {
  total: number;
  by_ns: Array<[string, number]>;
}

export type MemoryTab = 'memories' | 'observations' | 'learned';

interface MemoryState {
  open: boolean;
  tab: MemoryTab;
  query: string;
  memories: MemoryRecord[];
  observations: ObservationRecord[];
  learned: LearnedRecord[];
  stats: MemoryStats | null;
  loading: boolean;
  error: string | null;

  setOpen: (open: boolean) => void;
  setTab: (tab: MemoryTab) => void;
  setQuery: (q: string) => void;

  refresh: () => Promise<void>;
  search: (q: string) => Promise<void>;
  removeMemory: (id: string) => Promise<void>;
}

export const useMemoryStore = create<MemoryState>((set, get) => ({
  open: false,
  tab: 'memories',
  query: '',
  memories: [],
  observations: [],
  learned: [],
  stats: null,
  loading: false,
  error: null,

  setOpen: (open) => {
    set({ open });
    if (open) {
      void get().refresh();
    }
  },
  setTab: (tab) => set({ tab }),
  setQuery: (query) => set({ query }),

  refresh: async () => {
    set({ loading: true, error: null });
    try {
      const [memories, observations, learned, stats] = await Promise.all([
        invoke<MemoryRecord[]>('memory_list', { limit: 100, offset: 0 }),
        invoke<ObservationRecord[]>('memory_observations', { limit: 100 }),
        invoke<LearnedRecord[]>('memory_learned', { limit: 100 }),
        invoke<MemoryStats>('memory_stats'),
      ]);
      set({ memories, observations, learned, stats, loading: false });
    } catch (e) {
      set({ loading: false, error: String(e) });
    }
  },

  search: async (q: string) => {
    set({ loading: true, error: null, query: q });
    if (!q.trim()) {
      await get().refresh();
      return;
    }
    try {
      const memories = await invoke<MemoryRecord[]>('memory_search', {
        query: q,
        ns: null,
      });
      set({ memories, loading: false });
    } catch (e) {
      set({ loading: false, error: String(e) });
    }
  },

  removeMemory: async (id: string) => {
    try {
      await invoke<boolean>('memory_remove', { id });
      await get().refresh();
    } catch (e) {
      set({ error: String(e) });
    }
  },
}));
