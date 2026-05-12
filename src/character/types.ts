// ═══════════════════════════════════════════════════
// GOBLIN CHARACTER ENGINE — Type Definitions
// ═══════════════════════════════════════════════════

// --- Emotion Dimensions (0.0 — 1.0 continuous) ---
export interface EmotionVector {
  focus: number;
  frustration: number;
  curiosity: number;
  satisfaction: number;
  energy: number;
  engagement: number;
}

export type EmotionName = keyof EmotionVector;

export type Emotion =
  | 'neutral'
  | 'focused'
  | 'frustrated'
  | 'curious'
  | 'satisfied'
  | 'tired'
  | 'excited'
  | 'concerned'
  | 'playful'
  | 'proud'
  | 'surprised';

export type Mood =
  | 'calm'
  | 'productive'
  | 'tense'
  | 'playful'
  | 'tired'
  | 'supportive'
  | 'celebratory';

// --- Emotional Config per dimension ---
export interface EmotionConfig {
  /** Baseline value the emotion returns to when idle */
  baseline: number;
  /** How fast the current value moves toward target (0-1, higher = faster) */
  inertia: number;
  /** How fast the emotion decays back to baseline when no stimulus (per second) */
  decay: number;
  /** Minimum time between significant changes (ms) */
  cooldown: number;
  /** Current value (continuously interpolated) */
  current: number;
  /** Target value set by events */
  target: number;
  lastChangeAt: number;
}

// --- Emotional State (full snapshot) ---
export interface EmotionalState {
  vector: EmotionVector;
  primaryEmotion: Emotion;
  secondaryEmotion: Emotion | null;
  intensity: number;
  mood: Mood;
  configs: Record<EmotionName, EmotionConfig>;
  timestamp: number;
}

// --- Presence / Micro-behaviors ---
export interface PresenceState {
  blinkPhase: 'open' | 'closing' | 'closed' | 'opening';
  blinkProgress: number; // 0-1 within blink cycle
  nextBlinkAt: number;
  blinkCount: number;

  eyeGazeX: number; // -1 to 1 (horizontal gaze offset)
  eyeGazeY: number; // -1 to 1 (vertical gaze offset)
  nextSaccadeAt: number;

  breathePhase: number; // 0 to 2*PI
  breatheAmplitude: number;

  posture: 'upright' | 'lean_forward' | 'lean_back' | 'tilt_left' | 'tilt_right';
  postureTransition: number; // 0-1 lerp toward target posture
  nextPostureShiftAt: number;

  headTilt: number; // degrees
  earWiggle: number; // 0-1

  isAttentive: boolean;
  attentionFocus: 'user' | 'code' | 'terminal' | 'thinking' | 'none';
  attentionLocked: boolean;
}

// --- Animation Intent ---
export interface AnimationIntent {
  posture: 'upright' | 'lean_forward' | 'lean_back' | 'tilt_left' | 'tilt_right';
  eyeFocus: 'user' | 'code' | 'terminal' | 'thinking';
  energyLevel: number; // 0-1, drives animation speed and amplitude
  expressionIntensity: number; // 0-1, how strong facial expressions are
  reactionSpeed: number; // 0-1, how fast to react
  animationState: AnimationState;
}

export type AnimationState =
  | 'idle_breathe'
  | 'attentive_watch'
  | 'thinking_deep'
  | 'reading_scan'
  | 'writing_focused'
  | 'searching_explore'
  | 'running_active'
  | 'error_shock'
  | 'success_celebrate'
  | 'frustrated_tense'
  | 'curious_tilt'
  | 'playful_bounce';

// --- Events flowing through the Event Bus ---
export type CharacterEventType =
  | 'user.typing.started'
  | 'user.typing.fast'
  | 'user.typing.stopped'
  | 'user.idle.started'
  | 'user.idle.ended'
  | 'user.mouse.moved'
  | 'agent.thinking.started'
  | 'agent.thinking.progress'
  | 'agent.thinking.completed'
  | 'agent.tool.read_file'
  | 'agent.tool.write_file'
  | 'agent.tool.edit_file'
  | 'agent.tool.grep'
  | 'agent.tool.glob'
  | 'agent.tool.bash'
  | 'agent.tool.web_search'
  | 'agent.tool.web_fetch'
  | 'agent.tool.git'
  | 'agent.tool.other'
  | 'agent.response.received'
  | 'agent.error.occurred'
  | 'agent.error.repeated'
  | 'agent.success'
  | 'agent.decision'
  | 'build.failed'
  | 'build.succeeded'
  | 'session.started'
  | 'session.ended'
  | 'user.dislike';

export interface CharacterEvent {
  type: CharacterEventType;
  priority: number; // 0-100, higher = more important
  payload?: Record<string, unknown>;
  timestamp: number;
}

// --- Behavior configuration ---
export interface BehaviorConfig {
  /** Emotional response curve per event type */
  eventResponses: Record<CharacterEventType, Partial<EmotionVector>>;
  /** Priority threshold for interrupting current behavior (0-100) */
  interruptThreshold: number;
  /** Minimum time between state changes (ms) */
  minStateDuration: number;
  /** How strongly personality constrains emotional range */
  personalityWeight: number;
}

// --- Memory (character's internal) ---
export interface CharacterMemory {
  recentEvents: CharacterEvent[];
  errorStreak: number;
  successStreak: number;
  userEngagementScore: number;
  lastUserActivity: number;
  sessionStartTime: number;
}
