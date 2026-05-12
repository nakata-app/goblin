import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import type { Message } from '../types';

export type RightTab = 'dashboard' | 'thinking' | 'tasks' | 'output' | 'help';

export interface TaskEntry {
  id: string;
  name: string;
  status: 'pending' | 'running' | 'done' | 'error';
  result?: string;
  parentId?: string;
  depth?: number;
  agentType?: string;
}

export interface TaskTree {
  task: TaskEntry;
  children: TaskTree[];
}

export interface DecisionEntry {
  round: number;
  reasoning: string;
  tools_chosen: string[];
}

interface ChatState {
  messages: Message[];
  input: string;
  rightPanelContent: string;
  isStreaming: boolean;
  activeTab: RightTab;
  thinkingContent: string;
  tasks: TaskEntry[];
  taskTree: TaskTree[];
  diffContent: string;
  decisions: DecisionEntry[];

  setInput: (input: string) => void;
  addMessage: (msg: Message) => void;
  appendContent: (msgId: string, chunk: string) => void;
  setMessageContent: (msgId: string, content: string) => void;
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
  fetchTasks: () => Promise<void>;
  fetchTaskTree: () => Promise<TaskTree[]>;
  setDiff: (content: string) => void;
  addDecision: (d: DecisionEntry) => void;
  clearDecisions: () => void;
}

function isTauri(): boolean {
  return typeof window !== 'undefined' && ('__TAURI__' in window || '__TAURI_INTERNALS__' in window);
}

function persistTask(task: TaskEntry) {
  if (!isTauri()) return;
  invoke('task_upsert', {
    id: task.id,
    name: task.name,
    status: task.status,
    result: task.result ?? null,
  }).catch(() => {});
}

function persistClearTasks() {
  if (!isTauri()) return;
  invoke('task_clear').catch(() => {});
}

export { persistTask, persistClearTasks };

export const useChatStore = create<ChatState>((set) => ({
  messages: [],
  input: '',
  rightPanelContent: '',
  isStreaming: false,
  activeTab: 'dashboard',
  thinkingContent: '',
  tasks: [],
  taskTree: [] as TaskTree[],
  diffContent: '',
  decisions: [],

  setInput: (input) => set({ input }),
  addMessage: (msg) => set((s) => ({ messages: [...s.messages, msg] })),
  appendContent: (msgId, chunk) =>
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId ? { ...m, content: m.content + chunk } : m
      ),
    })),
  setMessageContent: (msgId, content) =>
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId ? { ...m, content } : m
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
  fetchTasks: async () => {
    try {
      const tasks = await invoke<TaskEntry[]>('task_list');
      set({ tasks });
    } catch {
      // Backend not available
    }
  },
  fetchTaskTree: async () => {
    try {
      const tree = await invoke<TaskTree[]>('task_tree');
      set({ taskTree: tree });
      return tree;
    } catch {
      set({ taskTree: [] });
      return [];
    }
  },
  setDiff: (content) => set({ diffContent: content }),
  addDecision: (d) => set((s) => ({ decisions: [...s.decisions, d] })),
  clearDecisions: () => set({ decisions: [] }),
}));
