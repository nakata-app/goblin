import { create } from 'zustand';
import type { GoblinState } from '../types';

interface AgentState {
  goblinState: GoblinState;
  model: string;
  cost: number;
  turnCount: number;
  tokensIn: number;
  tokensOut: number;
  activeTool: string | null;
  error: string | null;

  setGoblinState: (s: GoblinState) => void;
  setModel: (m: string) => void;
  addCost: (c: number) => void;
  incrementTurn: () => void;
  addTokens: (input: number, output: number) => void;
  setActiveTool: (t: string | null) => void;
  setError: (e: string | null) => void;
  reset: () => void;
}

export const useAgentStore = create<AgentState>((set) => ({
  goblinState: 'idle',
  model: 'auto',
  cost: 0,
  turnCount: 0,
  tokensIn: 0,
  tokensOut: 0,
  activeTool: null,
  error: null,

  setGoblinState: (s) => set({ goblinState: s }),
  setModel: (m) => set({ model: m }),
  addCost: (c) => set((s) => ({ cost: s.cost + c })),
  incrementTurn: () => set((s) => ({ turnCount: s.turnCount + 1 })),
  addTokens: (input, output) =>
    set((s) => ({
      tokensIn: s.tokensIn + input,
      tokensOut: s.tokensOut + output,
    })),
  setActiveTool: (t) => set({ activeTool: t }),
  setError: (e) => set({ error: e }),
  reset: () =>
    set({
      goblinState: 'idle',
      cost: 0,
      turnCount: 0,
      tokensIn: 0,
      tokensOut: 0,
      activeTool: null,
      error: null,
    }),
}));
