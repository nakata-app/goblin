// ═══════════════════════════════════════════════════
// CHARACTER PRESENCE LAYER
// Creates the illusion of life through micro-behaviors:
// blinking, eye saccades, breathing, posture shifts,
// attention focus, head tilts, ear wiggles.
//
// THIS is what creates emotional realism — not the LLM.
// ═══════════════════════════════════════════════════

import type { PresenceState, EmotionalState } from './types';

// Blink timing (humans blink every 2-10s, 100-400ms duration)
const BLINK_INTERVAL_MIN = 2000;
const BLINK_INTERVAL_MAX = 8000;
const BLINK_DURATION = 150; // ms for full close-open cycle

// Saccade timing (eyes micro-jump 2-5x per second)
const SACCADE_INTERVAL_MIN = 500;
const SACCADE_INTERVAL_MAX = 4000;

// Posture shift
const POSTURE_INTERVAL_MIN = 8000;
const POSTURE_INTERVAL_MAX = 30000;
const POSTURE_TRANSITION_SPEED = 0.02; // per tick

// Breathing
const BREATHE_BASE_SPEED = 0.8; // radians per second
const BREATHE_IDLE_AMPLITUDE = 0.04;
const BREATHE_ACTIVE_AMPLITUDE = 0.07;

function rand(min: number, max: number): number {
  return min + Math.random() * (max - min);
}

function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

export class PresenceSystem {
  state: PresenceState;
  private tickInterval: ReturnType<typeof setInterval> | null = null;
  private tickRate = 33;

  private listeners: Array<(s: PresenceState) => void> = [];

  // Blink state machine timing
  private blinkTimer = 0;
  private blinkTotalDuration = 0;

  constructor() {
    const now = Date.now();
    this.state = this.defaultState(now);
  }

  private defaultState(now: number): PresenceState {
    return {
      blinkPhase: 'open',
      blinkProgress: 0,
      nextBlinkAt: now + rand(BLINK_INTERVAL_MIN, BLINK_INTERVAL_MAX),
      blinkCount: 0,

      eyeGazeX: 0,
      eyeGazeY: 0,
      nextSaccadeAt: now + rand(SACCADE_INTERVAL_MIN, SACCADE_INTERVAL_MAX),

      breathePhase: 0,
      breatheAmplitude: BREATHE_IDLE_AMPLITUDE,

      posture: 'upright',
      postureTransition: 1,
      nextPostureShiftAt: now + rand(POSTURE_INTERVAL_MIN, POSTURE_INTERVAL_MAX),

      headTilt: 0,
      earWiggle: 0,

      isAttentive: false,
      attentionFocus: 'none',
      attentionLocked: false,
    };
  }

  /** Feed emotional state to modulate presence parameters. */
  updateEmotionalState(es: EmotionalState): void {
    // Breathing amplitude varies with energy
    const energy = es.vector.energy;
    this.state.breatheAmplitude = lerp(
      BREATHE_IDLE_AMPLITUDE,
      BREATHE_ACTIVE_AMPLITUDE,
      energy
    );

    // Frustration increases blink rate (nervous blinking)
    const frustration = es.vector.frustration;
    if (frustration > 0.5) {
      const reduction = lerp(1, 0.4, frustration);
      this.state.nextBlinkAt = Math.min(
        this.state.nextBlinkAt,
        Date.now() + rand(
          BLINK_INTERVAL_MIN * reduction,
          BLINK_INTERVAL_MAX * reduction
        )
      );
    }
  }

  /** Set attention focus explicitly (called when agent is doing something). */
  setAttention(focus: 'user' | 'code' | 'terminal' | 'thinking', locked = false): void {
    this.state.attentionFocus = focus;
    this.state.attentionLocked = locked;
    this.state.isAttentive = true;

    // Adjust gaze toward focus area
    switch (focus) {
      case 'user':     this.state.eyeGazeX = rand(-0.1, 0.1); this.state.eyeGazeY = rand(-0.1, 0.3); break;
      case 'code':     this.state.eyeGazeX = rand(-0.3, 0.3); this.state.eyeGazeY = rand(-0.3, 0.1); break;
      case 'terminal': this.state.eyeGazeX = rand(-0.2, 0.2); this.state.eyeGazeY = rand(0.1, 0.4); break;
      case 'thinking': this.state.eyeGazeX = rand(-0.5, 0.5); this.state.eyeGazeY = rand(-0.5, 0); break;
    }
  }

  /** Release attention lock (returns to natural wandering). */
  releaseAttention(): void {
    this.state.attentionLocked = false;
  }

  start(): void {
    if (this.tickInterval) return;
    let lastTick = Date.now();
    this.tickInterval = setInterval(() => {
      const now = Date.now();
      const dt = (now - lastTick) / 1000;
      lastTick = now;
      this.tick(dt, now);
      this.notify();
    }, this.tickRate);
  }

  stop(): void {
    if (this.tickInterval) {
      clearInterval(this.tickInterval);
      this.tickInterval = null;
    }
  }

  onChange(fn: (s: PresenceState) => void): () => void {
    this.listeners.push(fn);
    return () => {
      this.listeners = this.listeners.filter((l) => l !== fn);
    };
  }

  reset(): void {
    const now = Date.now();
    this.state = this.defaultState(now);
  }

  // --- Internal tick ---
  private tick(dt: number, now: number): void {
    this.tickBlink(dt, now);
    this.tickSaccade(dt, now);
    this.tickBreathe(dt);
    this.tickPosture(dt, now);
    this.tickHeadTilt(dt);
  }

  private tickBlink(dt: number, now: number): void {
    this.blinkTimer += dt * 1000;

    switch (this.state.blinkPhase) {
      case 'open': {
        if (now >= this.state.nextBlinkAt) {
          this.state.blinkPhase = 'closing';
          this.state.blinkProgress = 0;
          this.blinkTimer = 0;
          this.blinkTotalDuration = BLINK_DURATION;
          this.state.blinkCount++;
          this.state.nextBlinkAt = now + rand(BLINK_INTERVAL_MIN, BLINK_INTERVAL_MAX);
        }
        break;
      }
      case 'closing': {
        this.state.blinkProgress = this.blinkTimer / (this.blinkTotalDuration * 0.3);
        if (this.blinkTimer >= this.blinkTotalDuration * 0.3) {
          this.state.blinkPhase = 'closed';
          this.state.blinkProgress = 1;
          this.blinkTimer = 0;
        }
        break;
      }
      case 'closed': {
        if (this.blinkTimer >= this.blinkTotalDuration * 0.4) {
          this.state.blinkPhase = 'opening';
          this.state.blinkProgress = 1;
          this.blinkTimer = 0;
        }
        break;
      }
      case 'opening': {
        const openProgress = this.blinkTimer / (this.blinkTotalDuration * 0.3);
        this.state.blinkProgress = 1 - Math.min(openProgress, 1);
        if (this.blinkTimer >= this.blinkTotalDuration * 0.3) {
          this.state.blinkPhase = 'open';
          this.state.blinkProgress = 0;
          this.blinkTimer = 0;
        }
        break;
      }
    }
  }

  private tickSaccade(_dt: number, now: number): void {
    if (this.state.attentionLocked) return;
    if (now >= this.state.nextSaccadeAt) {
      // Random micro-movement of eyes
      this.state.eyeGazeX = lerp(this.state.eyeGazeX, rand(-0.5, 0.5), 0.6);
      this.state.eyeGazeY = lerp(this.state.eyeGazeY, rand(-0.5, 0.5), 0.6);
      this.state.nextSaccadeAt = now + rand(SACCADE_INTERVAL_MIN, SACCADE_INTERVAL_MAX);
    }
  }

  private tickBreathe(dt: number): void {
    const speed = BREATHE_BASE_SPEED * (0.6 + this.state.breatheAmplitude * 6);
    this.state.breathePhase += speed * dt;
    if (this.state.breathePhase > Math.PI * 2) {
      this.state.breathePhase -= Math.PI * 2;
    }
  }

  private tickPosture(dt: number, now: number): void {
    // Interpolate toward target posture
    if (this.state.postureTransition < 1) {
      this.state.postureTransition = Math.min(
        1,
        this.state.postureTransition + POSTURE_TRANSITION_SPEED * dt * 60
      );
    }

    if (now >= this.state.nextPostureShiftAt) {
      const postures: PresenceState['posture'][] = [
        'upright', 'lean_forward', 'lean_back', 'tilt_left', 'tilt_right',
      ];
      const current = this.state.posture;
      const others = postures.filter((p) => p !== current);
      this.state.posture = others[Math.floor(Math.random() * others.length)];
      this.state.postureTransition = 0;
      this.state.nextPostureShiftAt = now + rand(POSTURE_INTERVAL_MIN, POSTURE_INTERVAL_MAX);
    }
  }

  private tickHeadTilt(dt: number): void {
    // Subtle random head tilt (driven by posture + randomness)
    const targetTilt =
      this.state.posture === 'tilt_left' ? -5 :
      this.state.posture === 'tilt_right' ? 5 :
      Math.sin(Date.now() * 0.0003) * 2;
    this.state.headTilt = lerp(this.state.headTilt, targetTilt, 0.03 * dt * 60);

    // Occasional ear wiggle
    this.state.earWiggle = lerp(
      this.state.earWiggle,
      Math.sin(Date.now() * 0.005) > 0.9 ? 1 : 0,
      0.1 * dt * 60
    );
  }

  private notify(): void {
    for (const fn of this.listeners) fn(this.state);
  }
}
