export interface Message {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: number;
  toolCalls?: ToolCall[];
  queued?: boolean;
}

export interface ToolCallFunction {
  name: string;
  arguments: string;
}

export interface ToolCall {
  id: string;
  name: string;
  function: ToolCallFunction;
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

// Re-export character engine types for convenience
export type {
  Emotion,
  EmotionName,
  Mood,
  EmotionVector,
  EmotionConfig,
  EmotionalState,
  PresenceState,
  AnimationIntent,
  AnimationState,
  CharacterEvent,
  CharacterEventType,
  CharacterMemory,
} from '../character/types';
