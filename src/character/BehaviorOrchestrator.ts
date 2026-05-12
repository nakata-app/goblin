// ═══════════════════════════════════════════════════
// BEHAVIOR ORCHESTRATOR
// Maps events → emotional targets via configurable responses.
// Handles priority arbitration, interruption, mood continuity.
// This is the layer that prevents chaotic emotional flipping.
// ═══════════════════════════════════════════════════

import type {
  BehaviorConfig,
  CharacterEvent,
  CharacterEventType,
  EmotionName,
  EmotionVector,
} from './types';
import { EmotionalEngine } from './EmotionalEngine';
import type { LLMTargets } from './LLMInterpreter';

// Map each event type to emotion dimension deltas.
// Positive = increase, negative = decrease.
export const DEFAULT_EVENT_RESPONSES: Record<CharacterEventType, Partial<EmotionVector>> = {
  // --- User signals ---
  'user.typing.started':       { focus: 0.3,  engagement: 0.3, energy: 0.1 },
  'user.typing.fast':          { focus: 0.6,  engagement: 0.5, energy: 0.25, curiosity: 0.2 },
  'user.typing.stopped':       { focus: -0.2, engagement: -0.1 },
  'user.idle.started':         { engagement: -0.4, energy: -0.2, focus: -0.3 },
  'user.idle.ended':           { engagement: 0.3, energy: 0.2, focus: 0.2 },
  'user.mouse.moved':          { engagement: 0.05 },

  // --- Agent states ---
  'agent.thinking.started':    { focus: 0.7,  engagement: 0.3, energy: 0.2, curiosity: 0.2 },
  'agent.thinking.progress':   { focus: 0.1,  energy: -0.05 },
  'agent.thinking.completed':  { focus: -0.3, satisfaction: 0.15 },

  // --- Tool activity ---
  'agent.tool.read_file':      { focus: 0.5,  curiosity: 0.3 },
  'agent.tool.write_file':     { focus: 0.6,  satisfaction: 0.1, energy: 0.1 },
  'agent.tool.edit_file':      { focus: 0.55, satisfaction: 0.05 },
  'agent.tool.grep':           { focus: 0.4,  curiosity: 0.35 },
  'agent.tool.glob':           { focus: 0.3,  curiosity: 0.25 },
  'agent.tool.bash':           { focus: 0.5,  energy: 0.15 },
  'agent.tool.web_search':     { curiosity: 0.5, energy: 0.1, focus: 0.3 },
  'agent.tool.web_fetch':      { curiosity: 0.4, focus: 0.3 },
  'agent.tool.git':            { focus: 0.4,  satisfaction: 0.1 },
  'agent.tool.other':          { focus: 0.3,  curiosity: 0.15 },

  // --- Outcomes ---
  'agent.response.received':   { satisfaction: 0.4, engagement: 0.2, focus: -0.3 },
  'agent.error.occurred':      { frustration: 0.5, focus: -0.2, satisfaction: -0.3, energy: -0.2 },
  'agent.error.repeated':      { frustration: 0.8, energy: -0.3, satisfaction: -0.5 },
  'agent.success':             { satisfaction: 0.7, energy: 0.2, frustration: -0.5 },
  'agent.decision':            { focus: 0.0, energy: 0.0, curiosity: 0.0 },  // payload-driven, see processEvent

  // --- Build ---
  'build.failed':              { frustration: 0.6, focus: -0.2, satisfaction: -0.3 },
  'build.succeeded':           { satisfaction: 0.5, energy: 0.15, frustration: -0.4 },

  // --- Session ---
  'session.started':           { engagement: 0.5, curiosity: 0.3, energy: 0.3 },
  'session.ended':             { engagement: -0.5, energy: -0.2, focus: -0.3 },

  // --- Feedback ---
  'user.dislike':              { frustration: 0.3, satisfaction: -0.3, engagement: -0.1 },
};

// Priority values for each event type (0-100)
const EVENT_PRIORITIES: Record<CharacterEventType, number> = {
  'user.typing.started': 15,
  'user.typing.fast': 25,
  'user.typing.stopped': 5,
  'user.idle.started': 30,
  'user.idle.ended': 20,
  'user.mouse.moved': 2,
  'agent.thinking.started': 40,
  'agent.thinking.progress': 10,
  'agent.thinking.completed': 35,
  'agent.tool.read_file': 20,
  'agent.tool.write_file': 25,
  'agent.tool.edit_file': 25,
  'agent.tool.grep': 20,
  'agent.tool.glob': 15,
  'agent.tool.bash': 30,
  'agent.tool.web_search': 20,
  'agent.tool.web_fetch': 15,
  'agent.tool.git': 20,
  'agent.tool.other': 15,
  'agent.response.received': 45,
  'agent.error.occurred': 60,
  'agent.error.repeated': 80,
  'agent.success': 50,
  'agent.decision': 35,
  'build.failed': 55,
  'build.succeeded': 40,
  'session.started': 35,
  'session.ended': 30,
  'user.dislike': 50,
};

export class BehaviorOrchestrator {
  engine: EmotionalEngine;
  config: BehaviorConfig;
  private lastResponseApplied = 0;
  private llmWeight = 0.7; // LLM primacy over event-driven
  private lastLLMOutput: LLMTargets | null = null;

  constructor(engine: EmotionalEngine, config?: Partial<BehaviorConfig>) {
    this.engine = engine;
    this.config = {
      eventResponses: { ...DEFAULT_EVENT_RESPONSES },
      interruptThreshold: 50,
      minStateDuration: 800,
      personalityWeight: 0.6,
      ...config,
    };
  }

  /** Process an incoming event and apply emotional targets. */
  processEvent(event: CharacterEvent): void {
    const response = this.config.eventResponses[event.type];
    if (!response) return;

    // Apply personality weight: scale down extreme reactions
    const weight = this.config.personalityWeight;
    const now = Date.now();

    // Decision events: extract behavior patterns from payload
    let deltas: Partial<EmotionVector> = { ...response };
    if (event.type === 'agent.decision' && event.payload) {
      const tools = (event.payload.tools as string[]) ?? [];
      const toolCount = tools.length;
      const hasReasoning = !!(event.payload.has_reasoning);

      // Tool density → energy and focus
      if (toolCount >= 4) {
        deltas.energy = (deltas.energy ?? 0) + 0.5;
        deltas.focus = (deltas.focus ?? 0) + 0.6;
      } else if (toolCount >= 2) {
        deltas.energy = (deltas.energy ?? 0) + 0.3;
        deltas.focus = (deltas.focus ?? 0) + 0.5;
      } else if (toolCount === 0) {
        // Direct response — satisfied, calm
        deltas.satisfaction = (deltas.satisfaction ?? 0) + 0.3;
        deltas.focus = (deltas.focus ?? 0) - 0.2;
      } else {
        deltas.focus = (deltas.focus ?? 0) + 0.4;
      }

      // Tool category → specific emotions
      for (const t of tools) {
        if (t.startsWith('web_')) { deltas.curiosity = (deltas.curiosity ?? 0) + 0.15; }
        if (t === 'read_file' || t === 'grep' || t === 'glob') { deltas.curiosity = (deltas.curiosity ?? 0) + 0.1; }
        if (t === 'write_file' || t === 'edit_file' || t === 'multi_edit') { deltas.satisfaction = (deltas.satisfaction ?? 0) + 0.05; }
        if (t === 'bash') { deltas.focus = (deltas.focus ?? 0) + 0.1; deltas.energy = (deltas.energy ?? 0) + 0.05; }
        if (t.includes('git')) { deltas.satisfaction = (deltas.satisfaction ?? 0) + 0.05; }
      }

      // Tool diversity → curiosity
      const uniqueTools = new Set(tools).size;
      if (uniqueTools >= 3) { deltas.curiosity = (deltas.curiosity ?? 0) + 0.2; }

      // Deep reasoning → contemplative
      if (hasReasoning) { deltas.curiosity = (deltas.curiosity ?? 0) + 0.15; }

      // Clamp deltas
      for (const k of Object.keys(deltas) as EmotionName[]) {
        deltas[k] = Math.max(-1, Math.min(1, deltas[k] ?? 0));
      }
    }

    for (const [dim, value] of Object.entries(deltas) as [EmotionName, number][]) {
      const scaled = value * weight;

      // If LLM output is active, event-driven values are blended in at lower weight
      const blendWeight = this.lastLLMOutput ? (1 - this.llmWeight) : 1.0;
      const blendedScaled = scaled * blendWeight;

      if (Math.abs(blendedScaled) > 0.1) {
        this.engine.setTarget(dim, this.engine.configs[dim].target + blendedScaled, now);
      } else {
        this.engine.nudge(dim, blendedScaled);
      }
    }

    this.lastResponseApplied = now;
  }

  /**
   * Apply LLM-interpreted emotional targets.
   * LLM has primacy (understands semantics) but is blended with
   * event-driven values for realtime reactivity.
   *
   * Called after each LLM response with the parsed emotion JSON.
   */
  applyLLMOutput(targets: LLMTargets): void {
    this.lastLLMOutput = targets;

    // Apply LLM targets directly (bypass cooldown — LLM is authoritative)
    for (const [dim, value] of Object.entries(targets.targets) as [EmotionName, number][]) {
      if (value === undefined) continue;

      // Lerp: current target * (1 - llmWeight) + llmValue * llmWeight
      // This smooth-blends LLM authority with existing event-driven state
      const currentTarget = this.engine.configs[dim].target;
      const blended = currentTarget * (1 - this.llmWeight) + value * this.llmWeight;

      this.engine.configs[dim].target = blended;
      this.engine.configs[dim].lastChangeAt = Date.now();
    }

    // Force animation state from LLM if confidence is high
    if (targets.confidence > 0.7) {
      this.engine.nudge('focus', 0);
    }
  }

  /** Set the LLM blend weight (0-1). Higher = LLM has more authority. */
  setLLMWeight(w: number): void {
    this.llmWeight = Math.max(0, Math.min(1, w));
  }

  /** Get last applied LLM output. */
  getLastLLMOutput(): LLMTargets | null {
    return this.lastLLMOutput;
  }

  /** Get priority for an event type (0-100). */
  getPriority(type: CharacterEventType): number {
    return EVENT_PRIORITIES[type] ?? 10;
  }

  /** Check if a new event should interrupt current behavior. */
  shouldInterrupt(eventPriority: number): boolean {
    return eventPriority >= this.config.interruptThreshold;
  }

  /** Reset orchestrator state. */
  reset(): void {
    this.engine.reset();
    this.lastResponseApplied = 0;
    this.lastLLMOutput = null;
  }

  /** Get time since last event response was applied. */
  timeSinceLastResponse(now = Date.now()): number {
    return now - this.lastResponseApplied;
  }
}
