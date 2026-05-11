import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import type { Session } from '../types';

interface SessionSummary {
  id: string;
  title: string | null;
  startedAt: number;
  endedAt: number | null;
  model: string | null;
  messageCount: number;
  cost: number;
}

interface SessionSwitchResponse {
  id: string;
  title: string | null;
  startedAt: number;
  endedAt: number | null;
  model: string | null;
  tokensIn: number;
  tokensOut: number;
  cost: number;
  messages: Array<{ role: string; content: string; toolCalls?: unknown }>;
}

interface SessionState {
  sessions: Session[];
  activeSessionId: string | null;
  loading: boolean;

  fetchSessions: () => Promise<void>;
  createSession: () => Promise<void>;
  switchSession: (id: string) => Promise<SessionSwitchResponse | null>;
  deleteSession: (id: string) => Promise<void>;
  setActiveSessionId: (id: string) => void;
}

function toSession(s: SessionSummary): Session {
  return {
    id: s.id,
    title: s.title || '',
    startedAt: s.startedAt * 1000,
    endedAt: s.endedAt ? s.endedAt * 1000 : undefined,
    messageCount: s.messageCount,
    cost: s.cost,
    model: s.model || undefined,
  };
}

export const useSessionStore = create<SessionState>((set, get) => ({
  sessions: [],
  activeSessionId: null,
  loading: false,

  fetchSessions: async () => {
    set({ loading: true });
    try {
      const list = await invoke<SessionSummary[]>('session_list');
      set({ sessions: list.map(toSession), loading: false });
    } catch {
      set({ loading: false });
    }
  },

  createSession: async () => {
    await invoke<SessionSummary>('session_create');
    await get().fetchSessions();
  },

  switchSession: async (id: string) => {
    const data = await invoke<SessionSwitchResponse>('session_switch', { id });
    set({ activeSessionId: data.id });
    await get().fetchSessions();
    return data;
  },

  deleteSession: async (id: string) => {
    await invoke('session_delete', { id });
    get().fetchSessions();
  },

  setActiveSessionId: (id: string) => set({ activeSessionId: id }),
}));
