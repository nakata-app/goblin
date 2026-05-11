export interface Message {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: number;
  toolCalls?: ToolCall[];
}

export interface ToolCall {
  id: string;
  name: string;
  args: Record<string, unknown>;
  result?: string;
  status: 'pending' | 'running' | 'done' | 'error';
}

export type GoblinState = 
  | 'idle'
  | 'thinking'
  | 'reading'
  | 'writing'
  | 'searching'
  | 'running'
  | 'error'
  | 'success';

export interface Session {
  id: string;
  title: string;
  startedAt: number;
  endedAt?: number;
  messageCount: number;
  cost?: number;
  model?: string;
}
