// Per-tab snapshots for the multi-session UI. Each open tab caches the
// chat + token accounting state for a session id so the user can flick
// between tabs without losing context.
//
// Phase A scope: front-end cache only. The backend still treats one
// session as "active" at a time (driven by session_switch). True
// parallel-send across tabs would need useAgent to route through
// send_message_in_session — left for a later turn.

import { create } from 'zustand';
import type { Message } from '../types';

export interface TabSnapshot {
  messages: Message[];
  tokensIn: number;
  tokensOut: number;
  cost: number;
  turnCount: number;
  model: string;
  title: string;
}

interface TabsState {
  openTabs: string[];                       // session ids, left-to-right
  cache: Record<string, TabSnapshot>;       // sessionId -> snapshot

  hasTab: (id: string) => boolean;
  addTab: (id: string, snap: TabSnapshot) => void;
  updateSnapshot: (id: string, snap: TabSnapshot) => void;
  patchSnapshot: (id: string, patch: Partial<TabSnapshot>) => void;
  removeTab: (id: string) => string | null; // returns next active id (or null if no tabs left)
  getSnapshot: (id: string) => TabSnapshot | undefined;
  setTitle: (id: string, title: string) => void;
}

export const useTabsStore = create<TabsState>((set, get) => ({
  openTabs: [],
  cache: {},

  hasTab: (id) => get().openTabs.includes(id),

  addTab: (id, snap) => set((s) => {
    if (s.openTabs.includes(id)) {
      return { cache: { ...s.cache, [id]: snap } };
    }
    return {
      openTabs: [...s.openTabs, id],
      cache: { ...s.cache, [id]: snap },
    };
  }),

  updateSnapshot: (id, snap) => set((s) => ({
    cache: { ...s.cache, [id]: snap },
  })),

  patchSnapshot: (id, patch) => set((s) => {
    const existing = s.cache[id];
    if (!existing) return {};
    return { cache: { ...s.cache, [id]: { ...existing, ...patch } } };
  }),

  removeTab: (id) => {
    const { openTabs, cache } = get();
    const idx = openTabs.indexOf(id);
    if (idx === -1) return null;
    const next = [...openTabs.slice(0, idx), ...openTabs.slice(idx + 1)];
    const newCache = { ...cache };
    delete newCache[id];
    set({ openTabs: next, cache: newCache });
    // Prefer the right neighbor, fall back to the left, else null.
    if (next.length === 0) return null;
    if (idx < next.length) return next[idx];
    return next[next.length - 1];
  },

  getSnapshot: (id) => get().cache[id],

  setTitle: (id, title) => set((s) => {
    const existing = s.cache[id];
    if (!existing) return {};
    return { cache: { ...s.cache, [id]: { ...existing, title } } };
  }),
}));
