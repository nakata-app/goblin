import { useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChatStore } from '../stores/chatStore';
import { useAgentStore } from '../stores/agentStore';
import type { Message, ToolCall } from '../types';

function generateId(): string {
  return Math.random().toString(36).substring(2, 10);
}

interface AgentResponse {
  content: string;
  tool_calls: ToolCall[] | null;
  tokens_in: number;
  tokens_out: number;
  model: string;
}

export function useAgent() {
  const addMessage = useChatStore((s) => s.addMessage);
  const setRightPanel = useChatStore((s) => s.setRightPanel);
  const clearMessages = useChatStore((s) => s.clearMessages);
  const setGoblinState = useAgentStore((s) => s.setGoblinState);
  const model = useAgentStore((s) => s.model);
  const addCost = useAgentStore((s) => s.addCost);
  const incrementTurn = useAgentStore((s) => s.incrementTurn);
  const addTokens = useAgentStore((s) => s.addTokens);
  const setError = useAgentStore((s) => s.setError);

  const sendMessage = useCallback(
    async (text: string) => {
      const userMsg: Message = {
        id: generateId(),
        role: 'user',
        content: text,
        timestamp: Date.now(),
      };

      addMessage(userMsg);
      setGoblinState('thinking');
      incrementTurn();

      try {
        const response = await invoke<AgentResponse>('send_message', {
          message: text,
          model: model,
        });

        const assistantMsg: Message = {
          id: generateId(),
          role: 'assistant',
          content: response.content,
          timestamp: Date.now(),
          toolCalls: response.tool_calls ?? [],
        };

        addMessage(assistantMsg);
        addTokens(response.tokens_in, response.tokens_out);

        const costEstimate = ((response.tokens_in / 1_000_000) * 0.28) +
          ((response.tokens_out / 1_000_000) * 1.10);
        addCost(costEstimate);

        setGoblinState('success');

        if (response.tool_calls && response.tool_calls.length > 0) {
          setRightPanel(
            response.tool_calls
              .map((tc) => `[TOOL] ${tc.function.name}(${tc.function.arguments})`)
              .join('\n')
          );
        }

        setTimeout(() => setGoblinState('idle'), 1500);
      } catch (err) {
        const errorMsg = err instanceof Error ? err.message : String(err);
        const errorMessage: Message = {
          id: generateId(),
          role: 'assistant',
          content: `Hata: ${errorMsg}`,
          timestamp: Date.now(),
        };
        addMessage(errorMessage);
        setGoblinState('error');
        setError(errorMsg);
        setTimeout(() => setGoblinState('idle'), 3000);
      }
    },
    [
      addMessage,
      setRightPanel,
      addCost,
      incrementTurn,
      addTokens,
      setGoblinState,
      setError,
      model,
    ]
  );

  const clearConversation = useCallback(async () => {
    try {
      await invoke('clear_conversation');
      clearMessages();
      useAgentStore.getState().reset();
    } catch (err) {
      console.error('Clear failed:', err);
    }
  }, [clearMessages]);

  return { sendMessage, clearConversation };
}
