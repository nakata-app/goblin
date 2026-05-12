// ═══════════════════════════════════════════════════
// useGoblinState — React hook connecting agent activity
// to the character engine via the EventBus.
//
// This replaces the old simple goblinState → setGoblinState pattern
// with the full emotional/semantic character pipeline.
// ═══════════════════════════════════════════════════

import { useEffect, useCallback } from 'react';
import { useCharacterStore } from '../stores/characterStore';
import type { CharacterEventType } from '../character/types';

export type GoblinActivity =
  | 'idle'
  | 'thinking'
  | 'reading'
  | 'writing'
  | 'searching'
  | 'running'
  | 'error'
  | 'success';

// Map old simple states to character events
const ACTIVITY_EVENT_MAP: Record<GoblinActivity, CharacterEventType> = {
  idle: 'agent.thinking.completed',
  thinking: 'agent.thinking.started',
  reading: 'agent.tool.read_file',
  writing: 'agent.tool.write_file',
  searching: 'agent.tool.web_search',
  running: 'agent.tool.bash',
  error: 'agent.error.occurred',
  success: 'agent.success',
};

export function useGoblinState() {
  const emit = useCharacterStore((s) => s.emit);
  const setAttention = useCharacterStore((s) => s.setAttention);
  const releaseAttention = useCharacterStore((s) => s.releaseAttention);
  const emotionalState = useCharacterStore((s) => s.emotionalState);
  const presenceState = useCharacterStore((s) => s.presenceState);
  const animationIntent = useCharacterStore((s) => s.animationIntent);
  const start = useCharacterStore((s) => s.start);
  const stop = useCharacterStore((s) => s.stop);
  const reset = useCharacterStore((s) => s.reset);

  // Start/stop engine lifecycle
  useEffect(() => {
    start();
    return () => stop();
  }, [start, stop]);

  /** Set goblin activity — maps to character events automatically. */
  const setActivity = useCallback(
    (activity: GoblinActivity, toolName?: string) => {
      if (activity === 'idle') {
        releaseAttention();
        return;
      }

      // Map activity to attention focus
      const focusMap: Record<GoblinActivity, 'user' | 'code' | 'terminal' | 'thinking'> = {
        idle: 'user',
        thinking: 'thinking',
        reading: 'code',
        writing: 'code',
        searching: 'code',
        running: 'terminal',
        error: 'user',
        success: 'user',
      };

      setAttention(focusMap[activity], true);

      // Emit the mapped event
      const eventType = ACTIVITY_EVENT_MAP[activity];
      emit(eventType, toolName ? { tool: toolName } : undefined);
    },
    [emit, setAttention, releaseAttention]
  );

  /** Signal a user typing event (call on each keystroke or debounced). */
  const signalUserTyping = useCallback(
    (speed: 'normal' | 'fast' = 'normal') => {
      emit(speed === 'fast' ? 'user.typing.fast' : 'user.typing.started');
    },
    [emit]
  );

  /** Signal user stopped typing. */
  const signalUserStopped = useCallback(() => {
    emit('user.typing.stopped');
  }, [emit]);

  /** Signal user idle state. */
  const signalUserIdle = useCallback(
    (idle: boolean) => {
      emit(idle ? 'user.idle.started' : 'user.idle.ended');
    },
    [emit]
  );

  /** Signal mouse movement (call debounced). */
  const signalMouseMove = useCallback(() => {
    emit('user.mouse.moved');
  }, [emit]);

  /** Signal tool execution. */
  const signalTool = useCallback(
    (toolName: string) => {
      const toolMap: Record<string, CharacterEventType> = {
        read_file: 'agent.tool.read_file',
        write_file: 'agent.tool.write_file',
        edit_file: 'agent.tool.edit_file',
        grep: 'agent.tool.grep',
        glob: 'agent.tool.glob',
        bash: 'agent.tool.bash',
        bash_background: 'agent.tool.bash',
        web_search: 'agent.tool.web_search',
        web_fetch: 'agent.tool.web_fetch',
        git_status: 'agent.tool.git',
        git_diff: 'agent.tool.git',
        git_commit: 'agent.tool.git',
        git_log: 'agent.tool.git',
      };
      const event = toolMap[toolName] ?? 'agent.tool.other';
      emit(event, { tool: toolName });
    },
    [emit]
  );

  /** Signal agent success/error. */
  const signalOutcome = useCallback(
    (outcome: 'success' | 'error' | 'repeated_error') => {
      if (outcome === 'repeated_error') {
        emit('agent.error.repeated');
      } else {
        emit(outcome === 'success' ? 'agent.success' : 'agent.error.occurred');
      }
    },
    [emit]
  );

  /** Signal user feedback (dislike). */
  const signalDislike = useCallback(() => {
    emit('user.dislike');
  }, [emit]);

  return {
    emotionalState,
    presenceState,
    animationIntent,
    setActivity,
    signalUserTyping,
    signalUserStopped,
    signalUserIdle,
    signalMouseMove,
    signalTool,
    signalOutcome,
    signalDislike,
    reset,
  };
}
