import { useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useChatStore, persistTask, persistClearTasks } from '../stores/chatStore';
import { useAgentStore } from '../stores/agentStore';
import { useSessionStore } from '../stores/sessionStore';
import { useTabsStore } from '../stores/tabsStore';
import { useCharacterStore } from '../stores/characterStore';
import { extractLLMEmotion, llmOutputToTargets } from '../character/LLMInterpreter';
import type { CharacterEventType } from '../character/types';
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
  reasoning?: string | null;
  decisions?: DecisionEntry[] | null;
}

interface DecisionEntry {
  round: number;
  reasoning: string;
  tools_chosen: string[];
}

interface ProgressPayload {
  type: string;
  round?: number;
  max?: number;
  tool?: string;
  args?: string;
  success?: boolean;
  summary?: string;
  model?: string;
  error?: string;
  reasoning?: string;
  tools?: string[];
  has_tool_calls?: boolean;
  chunk?: string;
  session_id?: string;
}

const TOOL_EVENT_MAP: Record<string, string> = {
  read_file: 'agent.tool.read_file',
  write_file: 'agent.tool.write_file',
  edit_file: 'agent.tool.edit_file',
  grep: 'agent.tool.grep',
  glob: 'agent.tool.glob',
  bash: 'agent.tool.bash',
  web_search: 'agent.tool.web_search',
  web_fetch: 'agent.tool.web_fetch',
  git_status: 'agent.tool.git',
  git_diff: 'agent.tool.git',
  git_commit: 'agent.tool.git',
};

function stripEmotionJSON(text: string): string {
  let out = text.replace(/```json\s*[\s\S]*?```\n?/g, '');
  const emotionIdx = out.search(/\{\s*"emotion"/);
  if (emotionIdx === -1) return out.trim();
  let depth = 0;
  let end = -1;
  for (let i = emotionIdx; i < out.length; i++) {
    if (out[i] === '{') depth++;
    if (out[i] === '}') {
      depth--;
      if (depth === 0) { end = i + 1; break; }
    }
  }
  if (end === -1) return out.trim();
  try {
    const candidate = out.substring(emotionIdx, end);
    JSON.parse(candidate);
    out = out.substring(0, emotionIdx) + out.substring(end);
  } catch { /* not valid JSON, leave as is */ }
  return out.trim();
}

export function useAgent() {
  const addMessage = useChatStore((s) => s.addMessage);
  const appendContent = useChatStore((s) => s.appendContent);
  const setMessageContent = useChatStore((s) => s.setMessageContent);
  const markMessageSent = useChatStore((s) => s.markMessageSent);
  const setRightPanel = useChatStore((s) => s.setRightPanel);
  const clearMessages = useChatStore((s) => s.clearMessages);
  const setThinking = useChatStore((s) => s.setThinking);
  const clearThinking = useChatStore((s) => s.clearThinking);
  const upsertTask = useChatStore((s) => s.upsertTask);
  const clearTasks = useChatStore((s) => s.clearTasks);
  const addDecision = useChatStore((s) => s.addDecision);
  const clearDecisions = useChatStore((s) => s.clearDecisions);
  const setModel = useAgentStore((s) => s.setModel);
  const setGoblinState = useAgentStore((s) => s.setGoblinState);
  const model = useAgentStore((s) => s.model);
  const addCost = useAgentStore((s) => s.addCost);
  const incrementTurn = useAgentStore((s) => s.incrementTurn);
  const addTokens = useAgentStore((s) => s.addTokens);
  const setError = useAgentStore((s) => s.setError);
  const emitEvent = useCharacterStore((s) => s.emit);
  const applyLLMOutput = useCharacterStore((s) => s.applyLLMOutput);

  const idleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const sendingRef = useRef(false);
  const queueRef = useRef<{ text: string; msgId: string } | null>(null);
  const streamingMsgIdRef = useRef<string | null>(null);
  const streamingContentRef = useRef<string>('');
  const runningToolIdRef = useRef<string | null>(null);

  const processSend = useCallback(
    async (text: string, existingMsgId?: string) => {
      if (existingMsgId) {
        markMessageSent(existingMsgId);
      } else {
        const userMsg: Message = {
          id: generateId(),
          role: 'user',
          content: text,
          timestamp: Date.now(),
        };
        addMessage(userMsg);
      }
      sendingRef.current = true;
      setGoblinState('thinking');
      emitEvent('agent.thinking.started' as CharacterEventType);
      incrementTurn();
      clearThinking();
      clearTasks();
      persistClearTasks();
      clearDecisions();

      // Force React to flush state updates synchronously before the async invoke.
      // Double rAF ensures: 1) React schedules render, 2) React commits render to DOM.
      await new Promise<void>(r => {
        requestAnimationFrame(() => requestAnimationFrame(() => r()));
      });

      streamingMsgIdRef.current = null;
      streamingContentRef.current = '';

      // Snapshot the session this send belongs to so we can ignore
      // progress events that arrive after the user switches tabs.
      const sendSessionId = useSessionStore.getState().activeSessionId;

      // True only while the send's originating tab is the active one.
      // Lets us silence streaming UI mutations after the user navigates
      // away mid-flight; the final reply still lands in tabsStore so
      // it reappears on next visit.
      const isStillActive = () =>
        !sendSessionId || useSessionStore.getState().activeSessionId === sendSessionId;

      // Listen for real-time progress events from the Rust backend
      const progressUnlisten = await listen<ProgressPayload>('agent-progress', (event) => {
        const p = event.payload;
        // Drop events from sessions other than the one this send started
        // in. send_message_in_session stamps every event with session_id;
        // legacy send_message emits events without it, in which case we
        // assume they belong to the active (only) session.
        if (p.session_id && sendSessionId && p.session_id !== sendSessionId) {
          return;
        }
        // The originating tab was switched away from; the user is now
        // looking at a different chatStore, so don't pollute that view
        // with this send's stream. Final reply still gets cached below.
        if (!isStillActive()) {
          return;
        }
        const current = useChatStore.getState().rightPanelContent;
        switch (p.type) {
          case 'round':
            setRightPanel(`[Round ${p.round}/${p.max}]${current ? '\n' + current : ''}`);
            break;
          case 'content_chunk': {
            const rawChunk = p.chunk ?? '';
            streamingContentRef.current += rawChunk;
            // Strip emotion JSON from accumulated content and replace display
            const clean = stripEmotionJSON(streamingContentRef.current);
            if (!streamingMsgIdRef.current) {
              const streamMsg: Message = {
                id: generateId(),
                role: 'assistant',
                content: clean,
                timestamp: Date.now(),
              };
              streamingMsgIdRef.current = streamMsg.id;
              addMessage(streamMsg);
              setGoblinState('streaming');
            } else {
              setMessageContent(streamingMsgIdRef.current, clean);
            }
            break;
          }
          case 'reasoning_chunk': {
            const chunk = p.chunk ?? '';
            useChatStore.getState().appendThinking(chunk);
            break;
          }
          case 'tool_start': {
            const tid = `pt-${Date.now()}-${Math.random().toString(36).slice(2,6)}`;
            runningToolIdRef.current = tid;
            const t = { id: tid, name: p.tool as string, status: 'running' as const };
            upsertTask(t);
            persistTask(t);
            setRightPanel(`[TOOL] ${p.tool}(${p.args ?? ''})${current ? '\n' + current : ''}`);
            break;
          }
          case 'tool_end': {
            const tid = runningToolIdRef.current ?? `pt-${Date.now()}-${Math.random().toString(36).slice(2,6)}`;
            runningToolIdRef.current = null;
            const t = { id: tid, name: p.tool as string, status: (p.success ? 'done' : 'error') as 'done' | 'error', result: p.summary };
            upsertTask(t);
            persistTask(t);
            break;
          }
          case 'thinking':
            setGoblinState('thinking');
            break;
          case 'error':
            setGoblinState('error');
            setError(p.error as string);
            break;
          case 'decision':
            addDecision({
              round: (p.round ?? 0),
              reasoning: (p.reasoning ?? ''),
              tools_chosen: (p.tools ?? []),
            });
            emitEvent('agent.decision' as CharacterEventType, {
              tools: p.tools ?? [],
              has_reasoning: !!(p.reasoning),
            });
            break;
        }
      });

      try {
        // Route through the session-scoped command when we have a
        // session id (multi-tab path); fall back to the legacy global
        // command for cold boots where activeSessionId is still null.
        const response = sendSessionId
          ? await invoke<AgentResponse>('send_message_in_session', {
              sessionId: sendSessionId,
              message: text,
              model: model === 'auto' ? null : model,
            })
          : await invoke<AgentResponse>('send_message', {
              message: text,
              model: model === 'auto' ? null : model,
            });

        progressUnlisten();

        const displayContent = stripEmotionJSON(response.content);
        const ti = response.tokens_in ?? 0;
        const to = response.tokens_out ?? 0;
        const costEstimate = ((ti / 1_000_000) * 0.28) +
          ((to / 1_000_000) * 1.10);

        if (!isStillActive() && sendSessionId) {
          // The user navigated away mid-flight. Park the assistant
          // reply + accounting directly into this session's cached
          // snapshot so the next switch back to it shows the reply.
          const assistantMsg: Message = {
            id: generateId(),
            role: 'assistant',
            content: displayContent,
            timestamp: Date.now(),
            toolCalls: response.tool_calls ?? [],
          };
          const tabs = useTabsStore.getState();
          const existing = tabs.getSnapshot(sendSessionId);
          if (existing) {
            tabs.patchSnapshot(sendSessionId, {
              messages: [...existing.messages, assistantMsg],
              tokensIn: existing.tokensIn + ti,
              tokensOut: existing.tokensOut + to,
              cost: existing.cost + costEstimate,
              model: response.model || existing.model,
            });
          }
          sendingRef.current = false;
          return;
        }

        if (streamingMsgIdRef.current) {
          setMessageContent(streamingMsgIdRef.current, displayContent);
          streamingMsgIdRef.current = null;
          streamingContentRef.current = '';
        } else {
          const assistantMsg: Message = {
            id: generateId(),
            role: 'assistant',
            content: displayContent,
            timestamp: Date.now(),
            toolCalls: response.tool_calls ?? [],
          };
          addMessage(assistantMsg);
        }

        addTokens(ti, to);

        if (response.model) {
          setModel(response.model);
        }

        addCost(costEstimate);

        setGoblinState('success');
        emitEvent('agent.success' as CharacterEventType);

        const llmEmotion = extractLLMEmotion(response.content);
        if (llmEmotion) {
          const targets = llmOutputToTargets(llmEmotion);
          applyLLMOutput(targets);
        } else {
          emitEvent('agent.response.received' as CharacterEventType);
        }

        if (response.reasoning) {
          setThinking(response.reasoning);
        }

        if (response.decisions && response.decisions.length > 0) {
          useChatStore.getState().clearDecisions();
          for (const d of response.decisions) {
            useChatStore.getState().addDecision(d);
          }
        }

        if (response.tool_calls && response.tool_calls.length > 0) {
          setRightPanel(
            response.tool_calls
              .map((tc) => `[TOOL] ${tc.function?.name ?? 'unknown'}(${tc.function?.arguments ?? ''})`)
              .join('\n')
          );

          for (const tc of response.tool_calls) {
            const toolName = tc.function?.name ?? '';
            const t = { id: tc.id ?? generateId(), name: toolName, status: 'running' as const };
            upsertTask(t);
            persistTask(t);
            const eventType = (TOOL_EVENT_MAP[toolName] ?? 'agent.tool.other') as CharacterEventType;
            emitEvent(eventType, { tool: toolName });
          }
        }

        if (idleTimerRef.current) clearTimeout(idleTimerRef.current);
        idleTimerRef.current = setTimeout(() => setGoblinState('idle'), 1500);
      } catch (err) {
        progressUnlisten();
        const errorMsg = err instanceof Error ? err.message : String(err);
        const errorMessage: Message = {
          id: generateId(),
          role: 'assistant',
          content: `Hata: ${errorMsg}`,
          timestamp: Date.now(),
        };
        if (!isStillActive() && sendSessionId) {
          // Stash the error into the originating tab's snapshot — same
          // reasoning as the success path: don't taint the visible tab.
          const tabs = useTabsStore.getState();
          const existing = tabs.getSnapshot(sendSessionId);
          if (existing) {
            tabs.patchSnapshot(sendSessionId, {
              messages: [...existing.messages, errorMessage],
            });
          }
        } else {
          addMessage(errorMessage);
          setGoblinState('error');
          emitEvent('agent.error.occurred' as CharacterEventType);
          setError(errorMsg);
          if (idleTimerRef.current) clearTimeout(idleTimerRef.current);
          idleTimerRef.current = setTimeout(() => setGoblinState('idle'), 3000);
        }
      } finally {
        sendingRef.current = false;

        // Process queued message if any
        if (queueRef.current) {
          const { text: queuedText, msgId } = queueRef.current;
          queueRef.current = null;
          processSend(queuedText, msgId);
        }
      }
    },
    [
      addMessage,
      appendContent,
      setMessageContent,
      markMessageSent,
      setRightPanel,
      addCost,
      incrementTurn,
      addTokens,
      setModel,
      setGoblinState,
      setError,
      emitEvent,
      applyLLMOutput,
      clearDecisions,
      addDecision,
      model,
    ]
  );

  const sendMessage = useCallback(
    (text: string) => {
      if (sendingRef.current) {
        const msgId = generateId();
        const queuedMsg: Message = {
          id: msgId,
          role: 'user',
          content: text,
          timestamp: Date.now(),
          queued: true,
        };
        addMessage(queuedMsg);
        queueRef.current = { text, msgId };
        return;
      }
      processSend(text);
    },
    [processSend, addMessage]
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
