import { memo, useMemo } from 'react';
import type { EmotionalState, PresenceState, AnimationIntent } from '../character/types';

interface GoblinLiveProps {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
}

const PARTICLE_COUNT = 14;

export const GoblinLive = memo(function GoblinLive({
  emotionalState,
  presenceState,
  animationIntent,
}: GoblinLiveProps) {
  const { vector, mood, primaryEmotion, intensity } = emotionalState;
  const { blinkProgress, eyeGazeX, eyeGazeY, breathePhase, headTilt, earWiggle } = presenceState;
  const { animationState } = animationIntent;

  const breatheScale = 1 + Math.sin(breathePhase) * (presenceState.breatheAmplitude || 0.04) * 3;

  const skinColor = useMemo(() => {
    const m: Record<string, string> = {
      calm: '#5a8a3c',
      productive: '#4a9a3c',
      tense: '#8a5a3c',
      playful: '#6a9a3c',
      tired: '#5a7a3c',
      supportive: '#4a8a3c',
      celebratory: '#3aaa3c',
    };
    return m[mood] ?? '#5a8a3c';
  }, [mood]);

  const eyeColor = (vector?.frustration ?? 0) > 0.5 ? '#ff6644'
    : (vector?.curiosity ?? 0) > 0.7 ? '#44ddff'
    : (vector?.focus ?? 0) > 0.7 ? '#ffdd44'
    : '#ffff44';

  const eyeOpen = 1 - (blinkProgress ?? 0);
  const pupilX = (eyeGazeX ?? 0) * 10;
  const pupilY = (eyeGazeY ?? 0) * 8;

  const mouthCurve = useMemo(() => {
    const s = vector?.satisfaction ?? 0;
    const f = vector?.frustration ?? 0;
    if (f > 0.6) return 'M 45 86 Q 60 95 75 86';
    if (s > 0.6) return 'M 45 82 Q 60 70 75 82';
    if (s > 0.3) return 'M 47 84 Q 60 76 73 84';
    return 'M 47 84 Q 60 90 73 84';
  }, [vector?.satisfaction, vector?.frustration]);

  const frust = vector?.frustration ?? 0;
  const eyebrows = frust > 0.4
    ? { lx1: 28, ly1: 46, lx2: 42, ly2: 48, rx1: 78, ry1: 48, rx2: 92, ry2: 46 }
    : { lx1: 28, ly1: 44, lx2: 42, ly2: 42, rx1: 78, ry1: 42, rx2: 92, ry2: 44 };

  const ringColor = frust > 0.5 ? 'rgba(239,68,68,' : 'rgba(16,185,129,';
  const ringOpacity = 0.08 + (intensity ?? 0) * 0.12;

  const isActive = animationState !== 'idle_breathe';
  const isError = animationState === 'error_shock';
  const isSuccess = animationState === 'success_celebrate';

  const faceBorder = isError ? 'rgba(239,68,68,0.5)' : isSuccess ? 'rgba(16,185,129,0.5)' : 'rgba(16,185,129,0.2)';
  const faceShadow = isError ? '0 0 60px rgba(239,68,68,0.2)' : isSuccess ? '0 0 60px rgba(16,185,129,0.3)' : '0 0 40px rgba(16,185,129,0.08)';

  const particles = useMemo(() =>
    Array.from({ length: PARTICLE_COUNT }, (_, i) => ({
      angle: (i / PARTICLE_COUNT) * 360,
      size: 3 + Math.random() * 4,
      delay: +(i * 0.21).toFixed(2),
      orbit: 140 + Math.random() * 40,
    }))
  , []);

  const stateLabel = animationState === 'idle_breathe' ? 'Ready'
    : animationState === 'thinking_deep' ? 'Thinking'
    : animationState === 'reading_scan' ? 'Reading'
    : animationState === 'writing_focused' ? 'Writing'
    : animationState === 'searching_explore' ? 'Searching'
    : animationState === 'running_active' ? 'Running'
    : animationState === 'error_shock' ? 'Error'
    : animationState === 'success_celebrate' ? 'Done'
    : (primaryEmotion ?? 'Ready');

  return (
    <div className={`goblin-live${isActive ? ' goblin-live-active' : ''}`}>
      <div className="goblin-live-container">
        <svg
          width="320"
          height="340"
          viewBox="0 0 320 340"
          className="goblin-live-svg"
        >
          {/* Orbit rings */}
          <circle cx="160" cy="145" r="145" fill="none" stroke={ringColor + ringOpacity + ')'} strokeWidth="1.5" />
          <circle cx="160" cy="145" r="130" fill="none" stroke={ringColor + (ringOpacity * 1.4) + ')'} strokeWidth="1" />
          <circle cx="160" cy="145" r="110" fill="none" stroke={ringColor + (ringOpacity * 0.5) + ')'} strokeWidth="0.5" />

          {/* Particles */}
          {particles.map((p, i) => (
            <circle
              key={i}
              r={p.size / 2}
              fill={isError ? 'rgba(239,68,68,0.5)' : 'rgba(16,185,129,0.35)'}
              style={{
                ['--angle' as string]: `${p.angle}deg`,
                ['--delay' as string]: `${p.delay}s`,
                animation: `goblinParticleOrbit 3s linear ${p.delay}s infinite`,
                transformOrigin: '160px 145px',
              } as React.CSSProperties}
            />
          ))}

          {/* Head group */}
          <g style={{
            transform: `translate(160px,145px) rotate(${headTilt ?? 0}deg) translate(-160px,-145px)`,
            transition: 'transform 0.3s ease',
          }}>
            <g style={{
              transform: `translate(160px,145px) scale(${breatheScale}) translate(-160px,-145px)`,
              transformOrigin: '160px 145px',
              transition: 'transform 0.2s ease',
            }}>
              {/* Ears */}
              <g style={{ transformOrigin: '80px 75px', transform: `scale(${1 + (earWiggle ?? 0) * 0.2})` }}>
                <polygon points="90,40 65,20 110,70" fill={skinColor} stroke="#3a5a2a" strokeWidth="1.5" />
              </g>
              <g style={{ transformOrigin: '240px 75px', transform: `scale(${1 + (earWiggle ?? 0) * 0.2})` }}>
                <polygon points="230,40 255,20 210,70" fill={skinColor} stroke="#3a5a2a" strokeWidth="1.5" />
              </g>

              {/* Head */}
              <ellipse cx="160" cy="90" rx="62" ry="60" fill={skinColor} stroke="#3a5a2a" strokeWidth="2" />

              {/* Face glow */}
              <ellipse cx="160" cy="90" rx="62" ry="60" fill="none" stroke={faceBorder} strokeWidth="2.5"
                style={{ filter: `drop-shadow(${faceShadow})`, transition: 'all 0.4s ease' }}
              />

              {/* Eyes */}
              <ellipse cx="125" cy="82" rx="14" ry={14 * eyeOpen} fill="#111" stroke="#444" strokeWidth="0.8" />
              <ellipse cx="195" cy="82" rx="14" ry={14 * eyeOpen} fill="#111" stroke="#444" strokeWidth="0.8" />

              {/* Eye whites */}
              {eyeOpen > 0.05 && (
                <>
                  <ellipse cx="125" cy="82" rx="12" ry={12 * eyeOpen} fill="white" />
                  <ellipse cx="195" cy="82" rx="12" ry={12 * eyeOpen} fill="white" />
                </>
              )}

              {/* Pupils */}
              {eyeOpen > 0.1 && (
                <>
                  <circle cx={125 + pupilX} cy={82 + pupilY} r="6" fill={eyeColor} />
                  <circle cx={195 + pupilX} cy={82 + pupilY} r="6" fill={eyeColor} />
                  <circle cx={125 + pupilX + 2} cy={82 + pupilY - 2} r="2" fill="rgba(255,255,255,0.6)" />
                  <circle cx={195 + pupilX + 2} cy={82 + pupilY - 2} r="2" fill="rgba(255,255,255,0.6)" />
                </>
              )}

              {/* Eyebrows */}
              <line x1={eyebrows.lx1} y1={eyebrows.ly1} x2={eyebrows.lx2} y2={eyebrows.ly2}
                stroke="#2a3a1a" strokeWidth="2.5" strokeLinecap="round" />
              <line x1={eyebrows.rx1} y1={eyebrows.ry1} x2={eyebrows.rx2} y2={eyebrows.ry2}
                stroke="#2a3a1a" strokeWidth="2.5" strokeLinecap="round" />

              {/* Nose */}
              <ellipse cx="160" cy="95" rx="5" ry="3.5" fill="#4a7a2c" />

              {/* Mouth */}
              <path d={mouthCurve} fill="none" stroke="#2a3a1a" strokeWidth="2" strokeLinecap="round" />

              {/* Teeth */}
              {(vector?.satisfaction ?? 0) > 0.5 && (
                <>
                  <rect x="50" y="80" width="5" height="4" rx="1" fill="white" />
                  <rect x="59" y="80" width="5" height="4" rx="1" fill="white" />
                </>
              )}
            </g>
          </g>
        </svg>

        <div className="goblin-live-label">{stateLabel}</div>
        <div className="goblin-live-hint">{animationState}</div>
      </div>
    </div>
  );
});
