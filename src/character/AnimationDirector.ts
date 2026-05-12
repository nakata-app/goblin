// ═══════════════════════════════════════════════════
// ANIMATION INTELLIGENCE LAYER
// Converts emotional state + presence state → animation parameters.
// Uses a simplified FSM for state transitions + procedural blending.
//
// This layer answers: "what should the character LOOK like right now?"
// ═══════════════════════════════════════════════════

import type {
  AnimationIntent,
  AnimationState,
  EmotionalState,
  PresenceState,
} from './types';

// State transition rules: [from] → allowed [to]
const TRANSITIONS: Record<AnimationState, AnimationState[]> = {
  idle_breathe:        ['attentive_watch', 'thinking_deep', 'curious_tilt', 'playful_bounce'],
  attentive_watch:     ['idle_breathe', 'thinking_deep', 'reading_scan', 'writing_focused', 'frustrated_tense'],
  thinking_deep:       ['idle_breathe', 'reading_scan', 'writing_focused', 'searching_explore', 'running_active', 'error_shock'],
  reading_scan:        ['idle_breathe', 'thinking_deep', 'writing_focused', 'attentive_watch'],
  writing_focused:     ['idle_breathe', 'thinking_deep', 'success_celebrate', 'error_shock', 'running_active'],
  searching_explore:   ['idle_breathe', 'thinking_deep', 'reading_scan', 'curious_tilt'],
  running_active:      ['idle_breathe', 'thinking_deep', 'success_celebrate', 'error_shock', 'frustrated_tense'],
  error_shock:         ['idle_breathe', 'frustrated_tense', 'thinking_deep', 'attentive_watch'],
  success_celebrate:   ['idle_breathe', 'attentive_watch', 'playful_bounce'],
  frustrated_tense:    ['idle_breathe', 'thinking_deep', 'error_shock', 'attentive_watch'],
  curious_tilt:        ['idle_breathe', 'thinking_deep', 'searching_explore', 'attentive_watch'],
  playful_bounce:      ['idle_breathe', 'attentive_watch', 'curious_tilt'],
};

// Map emotions to animation states
function emotionToAnimationState(es: EmotionalState): AnimationState {
  const v = es.vector;

  if (v.frustration > 0.7) return 'error_shock';
  if (v.frustration > 0.45) return 'frustrated_tense';
  if (v.satisfaction > 0.75) return 'success_celebrate';
  if (v.curiosity > 0.7 && v.energy > 0.5) return 'curious_tilt';
  if (v.curiosity > 0.6) return 'searching_explore';
  if (v.focus > 0.7 && v.energy > 0.4) return 'thinking_deep';
  if (v.focus > 0.5) return 'writing_focused';
  if (v.energy < 0.2) return 'idle_breathe';
  if (v.energy > 0.7 && v.engagement > 0.5) return 'playful_bounce';
  if (v.engagement > 0.5) return 'attentive_watch';

  return 'idle_breathe';
}

function emotionToEyeFocus(es: EmotionalState): AnimationIntent['eyeFocus'] {
  if (es.vector.focus > 0.6) return 'code';
  if (es.vector.curiosity > 0.6) return 'thinking';
  if (es.vector.frustration > 0.5) return 'terminal';
  if (es.vector.engagement > 0.5) return 'user';
  return 'thinking';
}

function emotionToPosture(es: EmotionalState, ps: PresenceState): AnimationIntent['posture'] {
  if (es.vector.focus > 0.7) return 'lean_forward';
  if (es.vector.frustration > 0.5) return 'lean_back';
  if (es.vector.energy < 0.2) return 'lean_back';
  // Blend with presence system's natural posture
  if (ps.postureTransition < 0.5) return ps.posture;
  if (Math.random() > 0.7) return ps.posture;
  return 'upright';
}

const stateNames: AnimationState[] = [
  'idle_breathe', 'attentive_watch', 'thinking_deep', 'reading_scan',
  'writing_focused', 'searching_explore', 'running_active', 'error_shock',
  'success_celebrate', 'frustrated_tense', 'curious_tilt', 'playful_bounce',
];

function closestStateName(s: string): AnimationState {
  return (
    stateNames.find((n) => n === s) ??
    stateNames.find((n) => n.includes(s.split('_')[0])) ??
    'idle_breathe'
  );
}

export class AnimationDirector {
  currentState: AnimationState = 'idle_breathe';
  private stateEnteredAt = Date.now();
  private minStateDuration = 400; // ms minimum in a state

  getIntent(es: EmotionalState, ps: PresenceState): AnimationIntent {
    const targetState = emotionToAnimationState(es);
    this.transition(targetState);

    return {
      posture: emotionToPosture(es, ps),
      eyeFocus: emotionToEyeFocus(es),
      energyLevel: es.vector.energy,
      expressionIntensity: es.intensity,
      reactionSpeed: 0.3 + es.vector.energy * 0.7,
      animationState: this.currentState,
    };
  }

  /** Force transition to a state (bypasses FSM for priority events). */
  forceState(state: string): void {
    const target = closestStateName(state);
    this.currentState = target;
    this.stateEnteredAt = Date.now();
  }

  /** Try to transition, respecting FSM rules and min duration. */
  private transition(target: AnimationState): void {
    if (target === this.currentState) return;

    const now = Date.now();
    if (now - this.stateEnteredAt < this.minStateDuration) return;

    const allowed = TRANSITIONS[this.currentState];
    if (!allowed || !allowed.includes(target)) {
      // Find a valid intermediate state to get there
      // Simple: if not directly reachable, go through idle_breathe
      if (target !== 'idle_breathe' && allowed?.includes('idle_breathe')) {
        this.currentState = 'idle_breathe';
        this.stateEnteredAt = now;
      }
      return;
    }

    this.currentState = target;
    this.stateEnteredAt = now;
  }

  reset(): void {
    this.currentState = 'idle_breathe';
    this.stateEnteredAt = Date.now();
  }
}
