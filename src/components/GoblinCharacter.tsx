import type { GoblinState } from '../types';

interface GoblinCharacterProps {
  state: GoblinState;
  text: string;
  detail: string;
  isAnimating: boolean;
}

export const STATE_EMOJI: Record<GoblinState, string> = {
  idle: '👺',
  thinking: '🤔',
  reading: '📖',
  writing: '✍️',
  searching: '🔍',
  running: '⚡',
  error: '😱',
  success: '😎',
};

const PARTICLE_COUNT = 6;

export function GoblinCharacter({ state, text, detail, isAnimating }: GoblinCharacterProps) {
  const particles = isAnimating
    ? Array.from({ length: PARTICLE_COUNT }, (_, i) => {
        const angle = (i / PARTICLE_COUNT) * 360;
        const delay = i * 0.1;
        return (
          <div
            key={i}
            className="goblin-particle"
            style={{
              '--angle': `${angle}deg`,
              '--delay': `${delay}s`,
            } as React.CSSProperties}
          />
        );
      })
    : null;

  const idleBreathe = state === 'idle' && !isAnimating;

  return (
    <div className="goblin-strip">
      <div
        className={`goblin-avatar ${isAnimating ? 'animating' : ''} ${idleBreathe ? 'idle-breathe' : ''} goblin-${state}`}
      >
        <span className={`goblin-emoji ${isAnimating ? 'animating' : ''}`}>
          {STATE_EMOJI[state]}
        </span>
        {isAnimating && <div className="goblin-ring" />}
        {particles}
      </div>
      <div className="goblin-status">
        <div className="goblin-status-text">{text}</div>
        <div className="goblin-status-detail">{detail}</div>
      </div>
      {isAnimating && (
        <div className="goblin-sparkle">✦</div>
      )}
    </div>
  );
}
