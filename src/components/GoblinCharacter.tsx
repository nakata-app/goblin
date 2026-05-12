// ═══════════════════════════════════════════════════
// GOBLIN CHARACTER — Procedural SVG + CSS character
// Driven by Emotional State + Presence System.
// LLM controls meaning, this layer controls presence.
// ═══════════════════════════════════════════════════

import { memo } from 'react';
import type { EmotionalState, PresenceState, AnimationIntent } from '../character/types';
import type { GoblinState } from '../types';

// Legacy emoji map kept for tests
export const STATE_EMOJI: Record<GoblinState, string> = {
  idle: '👺',
  thinking: '🤔',
  reading: '📖',
  writing: '✍️',
  searching: '🔍',
  running: '⚡',
  error: '😱',
  success: '😎',
  streaming: '📡',
};

interface GoblinCharacterProps {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
}

export const GoblinCharacter = memo(function GoblinCharacter({
  emotionalState,
  presenceState,
  animationIntent,
}: GoblinCharacterProps) {
  const { vector, mood } = emotionalState;
  const {
    blinkProgress,
    eyeGazeX,
    eyeGazeY,
    breathePhase,
    posture,
    headTilt,
    earWiggle,
  } = presenceState;

  // Breathing scale effect
  const breatheScale = 1 + Math.sin(breathePhase) * presenceState.breatheAmplitude;

  // Posture transform
  const postureTransform = (() => {
    const base = `rotate(${headTilt}deg)`;
    switch (posture) {
      case 'lean_forward': return `${base} translateY(2px)`;
      case 'lean_back': return `${base} translateY(-2px)`;
      case 'tilt_left': return `${base} translateX(-1px)`;
      case 'tilt_right': return `${base} translateX(1px)`;
      default: return base;
    }
  })();

  // Eye state for SVG
  const eyeOpen = 1 - blinkProgress;
  const pupilX = eyeGazeX * 4;
  const pupilY = eyeGazeY * 3;

  // Mouth curve based on emotion
  const mouthCurve = (() => {
    const s = vector.satisfaction;
    const f = vector.frustration;
    if (f > 0.6) return 'M 32 38 Q 36 42 40 38'; // frown
    if (s > 0.6) return 'M 32 36 Q 36 32 40 36'; // smile
    if (s > 0.3) return 'M 32 37 Q 36 34 40 37'; // slight smile
    return 'M 32 37 Q 36 39 40 37'; // neutral
  })();

  // Ear wiggle scale
  const earScale = 1 + earWiggle * 0.15;

  // Skin tone based on mood
  const skinColors: Record<string, string> = {
    calm: '#5a8a3c',
    productive: '#4a9a3c',
    tense: '#8a5a3c',
    playful: '#6a9a3c',
    tired: '#5a7a3c',
    supportive: '#4a8a3c',
    celebratory: '#3aaa3c',
  };
  const skinColor = skinColors[mood] ?? '#5a8a3c';

  // Eye color shifts with emotion
  const eyeColor = (() => {
    if (vector.frustration > 0.5) return '#ff6644';
    if (vector.curiosity > 0.7) return '#44ddff';
    if (vector.focus > 0.7) return '#ffdd44';
    return '#ffff44';
  })();

  // Status text
  const statusText = (() => {
    if (vector.frustration > 0.6) return 'Hmm...';
    if (vector.satisfaction > 0.7) return 'Done!';
    if (vector.focus > 0.7) return 'Working...';
    if (vector.curiosity > 0.6) return 'Interesting...';
    if (vector.energy < 0.2) return 'Zzz...';
    if (vector.engagement > 0.5) return 'Ready!';
    return 'Ready';
  })();

  const detailText = (() => {
    const state = animationIntent.animationState;
    const map: Record<string, string> = {
      idle_breathe: 'waiting for command',
      attentive_watch: 'watching...',
      thinking_deep: 'processing...',
      reading_scan: 'reading files...',
      writing_focused: 'writing code...',
      searching_explore: 'searching...',
      running_active: 'executing...',
      error_shock: 'something went wrong',
      success_celebrate: 'all done!',
      frustrated_tense: 'troubleshooting...',
      curious_tilt: 'exploring...',
      playful_bounce: 'feeling good!',
    };
    return map[state] ?? animationIntent.animationState;
  })();

  const stateClass = `goblin-state-${animationIntent.animationState}`;

  return (
    <div className={`goblin-strip ${stateClass}`}>
      {/* SVG Character */}
      <div
        className="goblin-svg-container"
        style={{
          transform: `${postureTransform} scale(${breatheScale})`,
          transition: 'transform 0.3s ease',
        }}
      >
        <svg
          width="48"
          height="48"
          viewBox="0 0 48 48"
          className="goblin-svg"
        >
          {/* Ears */}
          <g style={{ transformOrigin: '12px 20px', transform: `scale(${earScale})` }}>
            <polygon
              points="8,12 2,4 14,24"
              fill={skinColor}
              stroke="#3a5a2a"
              strokeWidth="1"
            />
          </g>
          <g style={{ transformOrigin: '36px 20px', transform: `scale(${earScale})` }}>
            <polygon
              points="40,12 46,4 34,24"
              fill={skinColor}
              stroke="#3a5a2a"
              strokeWidth="1"
            />
          </g>

          {/* Head */}
          <ellipse cx="24" cy="24" rx="16" ry="16" fill={skinColor} stroke="#3a5a2a" strokeWidth="1.5" />

          {/* Eyes */}
          <ellipse cx="18" cy="22" rx="4" ry={4 * eyeOpen} fill="white" stroke="#333" strokeWidth="0.5" />
          <ellipse cx="30" cy="22" rx="4" ry={4 * eyeOpen} fill="white" stroke="#333" strokeWidth="0.5" />

          {/* Pupils */}
          {eyeOpen > 0.1 && (
            <>
              <circle cx={18 + pupilX} cy={22 + pupilY} r="2" fill={eyeColor} />
              <circle cx={30 + pupilX} cy={22 + pupilY} r="2" fill={eyeColor} />
            </>
          )}

          {/* Eyebrows (emotion indicators) */}
          <line
            x1="13" y1="17" x2="21" y2={vector.frustration > 0.4 ? 18 : 16}
            stroke="#2a3a1a"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
          <line
            x1="27" y1={vector.frustration > 0.4 ? 18 : 16} x2="35" y2="17"
            stroke="#2a3a1a"
            strokeWidth="1.5"
            strokeLinecap="round"
          />

          {/* Mouth */}
          <path
            d={mouthCurve}
            fill="none"
            stroke="#2a3a1a"
            strokeWidth="1.2"
            strokeLinecap="round"
          />

          {/* Tiny teeth when smiling */}
          {vector.satisfaction > 0.5 && (
            <>
              <rect x="33" y="36" width="2" height="2" rx="0.5" fill="white" />
              <rect x="37" y="36" width="2" height="2" rx="0.5" fill="white" />
            </>
          )}
        </svg>
      </div>

      {/* Status text */}
      <div className="goblin-status">
        <div className="goblin-status-text">{statusText}</div>
        <div className="goblin-status-detail">{detailText}</div>
      </div>

      {/* Emotional glow */}
      <div
        className="goblin-emotion-glow"
        style={{
          opacity: Math.max(vector.energy, vector.satisfaction, vector.frustration) * 0.3,
          background:
            vector.frustration > 0.5
              ? 'radial-gradient(circle, rgba(239,68,68,0.3), transparent)'
              : vector.satisfaction > 0.5
                ? 'radial-gradient(circle, rgba(16,185,129,0.3), transparent)'
                : vector.focus > 0.5
                  ? 'radial-gradient(circle, rgba(245,158,11,0.3), transparent)'
                  : 'radial-gradient(circle, rgba(16,185,129,0.15), transparent)',
        }}
      />

      {/* Active sparkle */}
      {animationIntent.animationState !== 'idle_breathe' && (
        <div className="goblin-sparkle">✦</div>
      )}
    </div>
  );
});
