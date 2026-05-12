// ═══════════════════════════════════════════════════
// COGNITIVE CONTEXT ENGINE
// Understands what is happening semantically.
// Task inference, focus estimation, frustration detection,
// coding flow analysis, intent tracking, error patterns.
// ═══════════════════════════════════════════════════

import type { CharacterEvent } from './types';

export interface CognitiveSnapshot {
  currentTask: string | null;
  taskConfidence: number; // 0-1
  estimatedFocus: number; // 0-1
  estimatedFrustration: number; // 0-1
  inCodingFlow: boolean;
  flowDuration: number; // ms in current flow
  intent: 'coding' | 'debugging' | 'exploring' | 'refactoring' | 'searching' | 'responding' | 'idle';
  errorStreak: number;
  successStreak: number;
  recentTools: string[];
  toolFrequency: Record<string, number>;
  typingSpeed: number; // chars per second
  lastActivity: number;
  sessionProductivity: number; // 0-1
}

export class CognitiveEngine {
  private events: CharacterEvent[] = [];
  private maxEvents = 200;
  private taskPatterns: Array<{ tools: string[]; intent: CognitiveSnapshot['intent'] }> = [
    { tools: ['read_file', 'grep', 'glob'], intent: 'exploring' },
    { tools: ['write_file', 'edit_file', 'read_file'], intent: 'coding' },
    { tools: ['bash', 'read_file', 'grep'], intent: 'debugging' },
    { tools: ['web_search', 'web_fetch'], intent: 'searching' },
    { tools: ['read_file', 'edit_file', 'grep'], intent: 'refactoring' },
    { tools: [], intent: 'responding' },
  ];

  // Typing tracking
  private typingTimes: number[] = [];
  private lastTypingEvent = 0;
  private charsTyped = 0;
  private flowStartTime = 0;

  feed(event: CharacterEvent): void {
    this.events.push(event);
    if (this.events.length > this.maxEvents) {
      this.events = this.events.slice(-this.maxEvents);
    }

    // Track typing speed
    if (event.type.startsWith('user.typing')) {
      if (this.lastTypingEvent > 0) {
        const interval = event.timestamp - this.lastTypingEvent;
        if (interval < 5000) {
          this.typingTimes.push(interval);
          if (this.typingTimes.length > 50) {
            this.typingTimes.shift();
          }
        }
      }
      this.lastTypingEvent = event.timestamp;
      this.charsTyped++;
    }
  }

  /** Get full cognitive snapshot. */
  snapshot(): CognitiveSnapshot {
    const recent = this.events.slice(-50);
    const tools = recent
      .filter((e) => e.type.startsWith('agent.tool.'))
      .map((e) => (e.payload?.tool as string) || '');

    const toolFrequency: Record<string, number> = {};
    for (const t of tools) {
      toolFrequency[t] = (toolFrequency[t] || 0) + 1;
    }

    // Detect coding flow
    const now = Date.now();
    const timeSinceLastEvent = now - (recent[recent.length - 1]?.timestamp ?? now);
    const inFlow = timeSinceLastEvent < 30000 && tools.length >= 3;

    if (inFlow && this.flowStartTime === 0) {
      this.flowStartTime = now;
    } else if (!inFlow) {
      this.flowStartTime = 0;
    }

    // Infer intent from tool patterns
    const intent = this.inferIntent(tools);

    // Typing speed (chars per second, averaged over recent window)
    const avgTypingInterval =
      this.typingTimes.length > 0
        ? this.typingTimes.reduce((a, b) => a + b, 0) / this.typingTimes.length
        : 999;
    const typingSpeed = avgTypingInterval > 0 ? 1000 / avgTypingInterval : 0;

    // Error/success streaks
    const errorStreak = this.countStreak(recent, 'error');
    const successStreak = this.countStreak(recent, 'success');

    // Focus estimation (0-1)
    const estimatedFocus =
      (tools.length > 0 ? 0.5 : 0) +
      (inFlow ? 0.3 : 0) +
      (typingSpeed > 3 ? 0.2 : typingSpeed > 1 ? 0.1 : 0);

    // Frustration estimation
    const estimatedFrustration =
      errorStreak > 3 ? 0.8 :
      errorStreak > 1 ? 0.5 :
      errorStreak > 0 ? 0.3 : 
      successStreak > 3 ? 0 : 0.1;

    return {
      currentTask: this.inferTask(tools),
      taskConfidence: tools.length > 2 ? 0.7 : 0.3,
      estimatedFocus: Math.min(1, estimatedFocus),
      estimatedFrustration,
      inCodingFlow: inFlow,
      flowDuration: inFlow ? now - this.flowStartTime : 0,
      intent,
      errorStreak,
      successStreak,
      recentTools: tools.slice(-5),
      toolFrequency,
      typingSpeed,
      lastActivity: recent[recent.length - 1]?.timestamp ?? 0,
      sessionProductivity: this.computeProductivity(),
    };
  }

  /** Feed cognitive insights into EmotionalEngine targets. */
  applyToEmotions(cs: CognitiveSnapshot, setTarget: (dim: string, value: number) => void): void {
    setTarget('focus', cs.estimatedFocus);
    setTarget('frustration', cs.estimatedFrustration);
    setTarget('energy', cs.inCodingFlow ? 0.7 : 0.4);
    setTarget('engagement', cs.typingSpeed > 0 ? 0.7 : 0.3);
  }

  reset(): void {
    this.events = [];
    this.typingTimes = [];
    this.lastTypingEvent = 0;
    this.charsTyped = 0;
    this.flowStartTime = 0;
  }

  // --- Private ---

  private inferIntent(tools: string[]): CognitiveSnapshot['intent'] {
    if (tools.length === 0) return 'idle';
    let bestMatch: CognitiveSnapshot['intent'] = 'responding';
    let bestScore = 0;
    for (const pattern of this.taskPatterns) {
      const score = pattern.tools.filter((t) => tools.includes(t)).length;
      if (score > bestScore) {
        bestScore = score;
        bestMatch = pattern.intent;
      }
    }
    return bestMatch;
  }

  private inferTask(tools: string[]): string | null {
    const freq = tools.reduce<Record<string, number>>((acc, t) => {
      acc[t] = (acc[t] || 0) + 1;
      return acc;
    }, {});
    const top = Object.entries(freq).sort(([, a], [, b]) => b - a)[0];
    if (!top) return null;
    const [tool] = top;
    const map: Record<string, string> = {
      read_file: 'Reading files',
      write_file: 'Writing code',
      edit_file: 'Editing code',
      grep: 'Searching codebase',
      glob: 'Finding files',
      bash: 'Running commands',
      web_search: 'Searching web',
      web_fetch: 'Fetching content',
      git_status: 'Checking git',
      git_diff: 'Reviewing changes',
      git_commit: 'Committing',
    };
    return map[tool] ?? `Using ${tool}`;
  }

  private countStreak(events: CharacterEvent[], type: 'error' | 'success'): number {
    let streak = 0;
    const keyword = type === 'error' ? 'error' : 'success';
    for (let i = events.length - 1; i >= 0; i--) {
      if (events[i].type.includes(keyword)) streak++;
      else if (!events[i].type.startsWith('agent.tool.')) break;
    }
    return streak;
  }

  private computeProductivity(): number {
    const all = this.events;
    const successes = all.filter((e) => e.type.includes('success')).length;
    const errors = all.filter((e) => e.type.includes('error')).length;
    const total = successes + errors;
    if (total === 0) return 0.5;
    return successes / total;
  }
}
