// ═══════════════════════════════════════════════════
// LLM INTERPRETATION LAYER
// Extracts structured emotion/behavior JSON from LLM text responses.
// Parses and validates against the contract schema.
//
// LLM outputs STRICT STRUCTURED JSON — this layer parses it,
// validates it, and converts it to engine-compatible targets.
// ═══════════════════════════════════════════════════

import type { Emotion, EmotionName } from './types';

export interface LLMEmotionOutput {
  emotion: {
    primary: Emotion;
    secondary?: Emotion;
    intensity: number; // 0-1
  };
  behavior: {
    engagement: 'supportive' | 'analytical' | 'empathetic' | 'celebratory' | 'concerned' | 'playful' | 'neutral';
    energy: number; // 0-1
  };
  animation_intent: {
    posture: 'upright' | 'lean_forward' | 'lean_back' | 'tilt_left' | 'tilt_right';
    eye_focus: 'user' | 'code' | 'terminal' | 'thinking';
  };
}

export interface LLMTargets {
  targets: Partial<Record<EmotionName, number>>;
  primaryEmotion: Emotion;
  secondaryEmotion: Emotion | null;
  intensity: number;
  posture: string;
  eyeFocus: string;
  confidence: number; // 0-1, how confident the LLM is
}

const VALID_EMOTIONS: Emotion[] = [
  'neutral', 'focused', 'frustrated', 'curious', 'satisfied',
  'tired', 'excited', 'concerned', 'playful', 'proud', 'surprised',
];

const VALID_POSTURES = ['upright', 'lean_forward', 'lean_back', 'tilt_left', 'tilt_right'];
const VALID_EYE_FOCUS = ['user', 'code', 'terminal', 'thinking'];
const VALID_ENGAGEMENT = ['supportive', 'analytical', 'empathetic', 'celebratory', 'concerned', 'playful', 'neutral'];

/**
 * Map LLM emotion + behavior → engine emotion vector targets.
 * Different engagement styles produce different internal emotion distributions.
 */
export function llmOutputToTargets(output: LLMEmotionOutput): LLMTargets {
  const { emotion, behavior, animation_intent } = output;
  const targets: Partial<Record<EmotionName, number>> = {};

  const intensity = clamp(emotion.intensity, 0, 1);
  const energy = clamp(behavior.energy, 0, 1);

  // Base emotion mapping
  switch (emotion.primary) {
    case 'focused':
      targets.focus = 0.6 + intensity * 0.4;
      targets.energy = energy;
      targets.engagement = 0.5 + intensity * 0.3;
      break;
    case 'frustrated':
      targets.frustration = 0.5 + intensity * 0.5;
      targets.focus = 0.2 + (1 - intensity) * 0.3;
      targets.energy = Math.min(energy, 0.5);
      targets.satisfaction = Math.max(0, 0.2 - intensity * 0.2);
      break;
    case 'curious':
      targets.curiosity = 0.6 + intensity * 0.4;
      targets.energy = energy;
      targets.focus = 0.3 + intensity * 0.3;
      break;
    case 'satisfied':
      targets.satisfaction = 0.5 + intensity * 0.5;
      targets.energy = energy;
      targets.focus = 0.2;
      targets.frustration = 0.02;
      break;
    case 'proud':
      targets.satisfaction = 0.7 + intensity * 0.3;
      targets.energy = Math.max(energy, 0.6);
      targets.engagement = 0.6;
      break;
    case 'tired':
      targets.energy = 0.1 + (1 - intensity) * 0.1;
      targets.focus = 0.05;
      targets.engagement = 0.1;
      break;
    case 'excited':
      targets.energy = 0.7 + intensity * 0.3;
      targets.satisfaction = 0.3 + intensity * 0.3;
      targets.curiosity = 0.4;
      break;
    case 'concerned':
      targets.frustration = 0.25 + intensity * 0.25;
      targets.engagement = 0.5 + intensity * 0.3;
      targets.focus = 0.3;
      break;
    case 'playful':
      targets.curiosity = 0.4 + intensity * 0.3;
      targets.energy = 0.5 + intensity * 0.3;
      targets.engagement = 0.5;
      break;
    case 'surprised':
      targets.curiosity = 0.6;
      targets.energy = 0.5 + intensity * 0.3;
      break;
    default: // neutral
      targets.focus = 0.15;
      targets.energy = 0.4;
      targets.engagement = 0.3;
      break;
  }

  // Behavior engagement style modulates the base mapping
  switch (behavior.engagement) {
    case 'supportive':
      targets.engagement = Math.max(targets.engagement ?? 0.5, 0.6);
      targets.frustration = Math.min(targets.frustration ?? 0.1, 0.2);
      break;
    case 'analytical':
      targets.focus = Math.max(targets.focus ?? 0.5, 0.7);
      targets.curiosity = Math.max(targets.curiosity ?? 0.3, 0.4);
      break;
    case 'empathetic':
      targets.engagement = Math.max(targets.engagement ?? 0.5, 0.7);
      targets.energy = Math.min(targets.energy ?? 0.5, 0.4);
      break;
    case 'celebratory':
      targets.satisfaction = Math.max(targets.satisfaction ?? 0.5, 0.8);
      targets.energy = Math.max(targets.energy ?? 0.5, 0.8);
      break;
    case 'concerned':
      targets.frustration = Math.max(targets.frustration ?? 0.1, 0.3);
      targets.engagement = Math.max(targets.engagement ?? 0.4, 0.5);
      break;
    case 'playful':
      targets.curiosity = Math.max(targets.curiosity ?? 0.3, 0.5);
      targets.energy = Math.max(targets.energy ?? 0.4, 0.6);
      break;
  }

  // Normalize all values
  for (const k of Object.keys(targets) as EmotionName[]) {
    targets[k] = clamp(targets[k]!, 0, 1);
  }

  return {
    targets,
    primaryEmotion: emotion.primary,
    secondaryEmotion: emotion.secondary ?? null,
    intensity: clamp(intensity, 0, 1),
    posture: VALID_POSTURES.includes(animation_intent.posture) ? animation_intent.posture : 'upright',
    eyeFocus: VALID_EYE_FOCUS.includes(animation_intent.eye_focus) ? animation_intent.eye_focus : 'thinking',
    confidence: 0.8, // LLM is the source of truth for interpretation
  };
}

/**
 * Extract emotion JSON block from LLM text response.
 * Looks for ` ```json ... ``` ` or raw JSON object with emotion/behavior/animation_intent keys.
 */
export function extractLLMEmotion(text: string): LLMEmotionOutput | null {
  // Try fenced JSON block first
  const fenced = text.match(/```json\s*([\s\S]*?)```/g);
  if (fenced) {
    for (const block of fenced) {
      const inner = block.replace(/```json\s*|\s*```/g, '').trim();
      const parsed = tryParse(inner);
      if (parsed) return parsed;
    }
  }

  // Try raw JSON objects with emotion key
  const jsonBlock = text.match(/\{[\s\S]*?"emotion"[\s\S]*?\}/);
  if (jsonBlock) {
    const parsed = tryParse(jsonBlock[0]);
    if (parsed) return parsed;
  }

  return null;
}

export function tryParse(jsonStr: string): LLMEmotionOutput | null {
  try {
    const obj = JSON.parse(jsonStr);
    if (validateLLMOutput(obj)) return obj as LLMEmotionOutput;
  } catch {
    // Invalid JSON
  }
  return null;
}

function validateLLMOutput(obj: unknown): boolean {
  if (!obj || typeof obj !== 'object') return false;
  const o = obj as Record<string, unknown>;

  if (!o.emotion || typeof o.emotion !== 'object') return false;
  const em = o.emotion as Record<string, unknown>;
  if (!em.primary || !VALID_EMOTIONS.includes(em.primary as Emotion)) return false;
  if (typeof em.intensity !== 'number') return false;

  if (!o.behavior || typeof o.behavior !== 'object') return false;
  const bh = o.behavior as Record<string, unknown>;
  if (!bh.engagement || !VALID_ENGAGEMENT.includes(bh.engagement as string)) return false;
  if (typeof bh.energy !== 'number') return false;

  if (!o.animation_intent || typeof o.animation_intent !== 'object') return false;
  const ai = o.animation_intent as Record<string, unknown>;
  if (!ai.posture || !VALID_POSTURES.includes(ai.posture as string)) return false;
  if (!ai.eye_focus || !VALID_EYE_FOCUS.includes(ai.eye_focus as string)) return false;

  return true;
}

function clamp(v: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, v));
}
