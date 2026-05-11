import { create } from 'zustand';
import type { Message } from '../types';

interface ChatState {
  messages: Message[];
  input: string;
  rightPanelContent: string;
  isStreaming: boolean;

  setInput: (input: string) => void;
  addMessage: (msg: Message) => void;
  appendContent: (msgId: string, chunk: string) => void;
  setRightPanel: (content: string) => void;
  appendRightPanel: (content: string) => void;
  clearMessages: () => void;
  setStreaming: (v: boolean) => void;
}

export const useChatStore = create<ChatState>((set) => ({
  messages: [],
  input: '',
  rightPanelContent: '',
  isStreaming: false,

  setInput: (input) => set({ input }),
  addMessage: (msg) => set((s) => ({ messages: [...s.messages, msg] })),
  appendContent: (msgId, chunk) =>
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId ? { ...m, content: m.content + chunk } : m
      ),
    })),
  setRightPanel: (content) => set({ rightPanelContent: content }),
  appendRightPanel: (content) =>
    set((s) => ({ rightPanelContent: s.rightPanelContent + content })),
  clearMessages: () => set({ messages: [], rightPanelContent: '' }),
  setStreaming: (v) => set({ isStreaming: v }),
}));
