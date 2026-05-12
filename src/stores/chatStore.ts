import { create } from 'zustand';
import type { Message } from '../types';

export type RightTab = 'dashboard' | 'thinking' | 'tasks' | 'output' | 'help';

export interface TaskEntry {
  id: string;
  name: string;
  status: 'pending' | 'running' | 'done' | 'error';
  result?: string;
}
interface ChatState {
  messages: Message[];
  input: string;
  rightPanelContent: string;
  isStreaming: boolean;
  activeTab: RightTab;
  thinkingContent: string;
  tasks: TaskEntry[];
  diffContent: string;

  setInput: (input: string) => void;
  addMessage: (msg: Message) => void;
  appendContent: (msgId: string, chunk: string) => void;
  markMessageSent: (msgId: string) => void;
  setRightPanel: (content: string) => void;
  appendRightPanel: (content: string) => void;
  clearMessages: () => void;
  setStreaming: (v: boolean) => void;
  setActiveTab: (tab: RightTab) => void;
  setThinking: (content: string) => void;
  appendThinking: (chunk: string) => void;
  clearThinking: () => void;
  setTasks: (tasks: TaskEntry[]) => void;
  upsertTask: (task: TaskEntry) => void;
  clearTasks: () => void;
  setDiff: (content: string) => void;
}

export const useChatStore = create<ChatState>((set) => ({
  messages: [],
  input: '',
  rightPanelContent: '',
  isStreaming: false,
  activeTab: 'dashboard',
  thinkingContent: '',
  tasks: [],
  diffContent: '',

  setInput: (input) => set({ input }),
  addMessage: (msg) => set((s) => ({ messages: [...s.messages, msg] })),
  appendContent: (msgId, chunk) =>
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId ? { ...m, content: m.content + chunk } : m
      ),
    })),
  markMessageSent: (msgId) =>
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId ? { ...m, queued: false } : m
      ),
    })),
  setRightPanel: (content) => set({ rightPanelContent: content }),
  appendRightPanel: (content) =>
    set((s) => ({ rightPanelContent: s.rightPanelContent + content })),
  clearMessages: () => set({ messages: [], rightPanelContent: '' }),
  setStreaming: (v) => set({ isStreaming: v }),
  setActiveTab: (tab) => set({ activeTab: tab }),
  setThinking: (content) => set({ thinkingContent: content }),
  appendThinking: (chunk) => set((s) => ({ thinkingContent: s.thinkingContent + chunk })),
  clearThinking: () => set({ thinkingContent: '' }),
  setTasks: (tasks) => set({ tasks }),
  upsertTask: (task) =>
    set((s) => {
      const idx = s.tasks.findIndex((t) => t.id === task.id);
      if (idx >= 0) {
        const next = [...s.tasks];
        next[idx] = task;
        return { tasks: next };
      }
      return { tasks: [...s.tasks, task] };
    }),
  clearTasks: () => set({ tasks: [] }),
  setDiff: (content) => set({ diffContent: content }),
}));
