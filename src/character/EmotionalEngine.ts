// ═══════════════════════════════════════════════════
// EMOTIONAL STATE ENGINE
// Continuous emotional simulation with inertia, decay, cooldowns.
// LLM NEVER controls animation directly — this engine translates
// semantic events into continuous emotional values.
// ═══════════════════════════════════════════════════

import type {
  Emotion,
  EmotionConfig,
  EmotionName,
  EmotionalState,
  EmotionVector,
  Mood,
} from './types';

export const DEFAULT_EMOTION_CONFIGS: Record<EmotionName, EmotionConfig> = {
  focus:      { baseline: 0.15, inertia: 0.3, decay: 0.025, cooldown: 800,  current: 0.15, target: 0.15, lastChangeAt: 0 },
  frustration:{ baseline: 0.05, inertia: 0.5, decay: 0.015, cooldown: 2000, current: 0.05, target: 0.05, lastChangeAt: 0 },
  curiosity:  { baseline: 0.3,  inertia: 0.4, decay: 0.02,  cooldown: 1500, current: 0.3,  target: 0.3,  lastChangeAt: 0 },
  satisfaction:{ baseline: 0.1,  inertia: 0.6, decay: 0.03,  cooldown: 1000, current: 0.1,  target: 0.1,  lastChangeAt: 0 },
  energy:     { baseline: 0.5,  inertia: 0.5, decay: 0.01,  cooldown: 3000, current: 0.5,  target: 0.5,  lastChangeAt: 0 },
  engagement: { baseline: 0.4,  inertia: 0.35, decay: 0.02, cooldown: 1200, current: 0.4,  target: 0.4,  lastChangeAt: 0 },
};

function clamp(v: number): number {
  return Math.max(0, Math.min(1, v));
}

/**
 * Determine primary emotion from vector values.
 * Priority: frustration > satisfaction > focus > curiosity > energy
 */
function computePrimaryEmotion(v: EmotionVector): Emotion {
  if (v.frustration > 0.65) return 'frustrated';
  if (v.satisfaction > 0.8) return 'proud';
  if (v.satisfaction > 0.55) return 'satisfied';
  if (v.curiosity > 0.8) return 'curious';
  if (v.focus > 0.7) return 'focused';
  if (v.energy > 0.8) return 'excited';
  if (v.energy < 0.2) return 'tired';
  if (v.curiosity > 0.5 && v.energy > 0.5) return 'playful';
  if (v.frustration > 0.35) return 'concerned';
  if (v.focus > 0.4 && v.energy > 0.4) return 'focused';
  if (v.engagement < 0.2) return 'tired';
  return 'neutral';
}

function computeSecondaryEmotion(v: EmotionVector, primary: Emotion): Emotion | null {
  const entries: [Emotion, number][] = [
    ['frustrated', v.frustration],
    ['proud', v.satisfaction],
    ['satisfied', v.satisfaction],
    ['curious', v.curiosity],
    ['focused', v.focus],
    ['excited', v.energy],
    ['tired', 1 - v.energy],
    ['concerned', v.frustration * 0.8],
    ['playful', v.curiosity * 0.5 + v.energy * 0.5],
    ['surprised', 0],
  ];
  const sorted = entries
    .filter(([e]) => e !== primary)
    .sort(([, a], [, b]) => b - a);
  return sorted[0][1] > 0.45 ? sorted[0][0] : null;
}

function computeMood(v: EmotionVector, _primary: Emotion): Mood {
  if (v.frustration > 0.5) return 'tense';
  if (v.satisfaction > 0.7) return 'celebratory';
  if (v.energy < 0.2) return 'tired';
  if (v.focus > 0.6 && v.energy > 0.4) return 'productive';
  if (v.curiosity > 0.6 && v.energy > 0.5) return 'playful';
  if (v.engagement > 0.6 && v.frustration < 0.3) return 'supportive';
  if (v.focus > 0.5) return 'productive';
  return 'calm';
}

function computeIntensity(v: EmotionVector): number {
  const values = Object.values(v) as number[];
  const max = Math.max(...values);
  const avg = values.reduce((a, b) => a + b, 0) / values.length;
  return clamp(max * 0.6 + avg * 0.4);
}

export class EmotionalEngine {
  configs: Record<EmotionName, EmotionConfig>;
  private tickInterval: ReturnType<typeof setInterval> | null = null;
  private tickRate = 50; // ms between ticks
  private listeners: Array<(state: EmotionalState) => void> = [];

  constructor(configs?: Partial<Record<EmotionName, Partial<EmotionConfig>>>) {
    this.configs = { ...DEFAULT_EMOTION_CONFIGS };
    if (configs) {
      for (const [name, overrides] of Object.entries(configs)) {
        const dim = name as EmotionName;
        this.configs[dim] = { ...this.configs[dim], ...overrides };
      }
    }
  }

  /** Set target for a specific emotion dimension. Cooldown respected. */
  setTarget(dim: EmotionName, value: number, now = Date.now()): void {
    const cfg = this.configs[dim];
    if (now - cfg.lastChangeAt < cfg.cooldown) return;
    cfg.target = clamp(value);
    cfg.lastChangeAt = now;
  }

  /** Apply a delta on top of current target (for additive events). */
  bumpTarget(dim: EmotionName, delta: number, now = Date.now()): void {
    const cfg = this.configs[dim];
    cfg.target = clamp(cfg.target + delta);
    cfg.lastChangeAt = now;
  }

  /** Incrementally nudge target without resetting cooldown (for continuous signals). */
  nudge(dim: EmotionName, delta: number): void {
    const cfg = this.configs[dim];
    cfg.target = clamp(cfg.target + delta);
  }

  /** Start the continuous simulation loop. */
  start(): void {
    if (this.tickInterval) return;
    let lastTick = Date.now();
    this.tickInterval = setInterval(() => {
      const now = Date.now();
      const dt = (now - lastTick) / 1000;
      lastTick = now;
      this.tick(dt);
      this.notify();
    }, this.tickRate);
  }

  /** Stop simulation. */
  stop(): void {
    if (this.tickInterval) {
      clearInterval(this.tickInterval);
      this.tickInterval = null;
    }
  }

  /** Subscribe to state changes. */
  onChange(fn: (state: EmotionalState) => void): () => void {
    this.listeners.push(fn);
    return () => {
      this.listeners = this.listeners.filter((l) => l !== fn);
    };
  }

  /** Get current snapshot. */
  snapshot(): EmotionalState {
    const v = this.vector();
    const primary = computePrimaryEmotion(v);
    return {
      vector: v,
      primaryEmotion: primary,
      secondaryEmotion: computeSecondaryEmotion(v, primary),
      intensity: computeIntensity(v),
      mood: computeMood(v, primary),
      configs: { ...this.configs },
      timestamp: Date.now(),
    };
  }

  /** Reset all to baseline. */
  reset(): void {
    for (const dim of Object.keys(this.configs) as EmotionName[]) {
      const cfg = this.configs[dim];
      cfg.current = cfg.baseline;
      cfg.target = cfg.baseline;
      cfg.lastChangeAt = 0;
    }
  }

  // --- Internal ---
  private vector(): EmotionVector {
    return {
      focus: this.configs.focus.current,
      frustration: this.configs.frustration.current,
      curiosity: this.configs.curiosity.current,
      satisfaction: this.configs.satisfaction.current,
      energy: this.configs.energy.current,
      engagement: this.configs.engagement.current,
    };
  }

  private tick(dt: number): void {
    for (const dim of Object.keys(this.configs) as EmotionName[]) {
      const cfg = this.configs[dim];
      const diff = cfg.target - cfg.current;

      if (Math.abs(diff) < 0.001) {
        // Decay toward baseline when no active target
        const decayDiff = cfg.baseline - cfg.current;
        cfg.current = clamp(cfg.current + decayDiff * cfg.decay * dt * 10);
      } else {
        // Move toward target with inertia
        cfg.current = clamp(cfg.current + diff * cfg.inertia * dt * 10);
      }

      // Snap very close values
      if (Math.abs(cfg.current - cfg.target) < 0.005) {
        cfg.current = cfg.target;
      }
    }
  }

  private notify(): void {
    const state = this.snapshot();
    for (const fn of this.listeners) fn(state);
  }
}
