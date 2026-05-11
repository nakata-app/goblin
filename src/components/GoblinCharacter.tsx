import type { GoblinState } from '../types';

interface GoblinCharacterProps {
  state: GoblinState;
  text: string;
  detail: string;
  isAnimating: boolean;
}

const STATE_EMOJI: Record<GoblinState, string> = {
  idle: '👺',
  thinking: '🤔',
  reading: '📖',
  writing: '✍️',
  searching: '🔍',
  running: '⚡',
  error: '😱',
  success: '😎',
};

export function GoblinCharacter({ state, text, detail, isAnimating }: GoblinCharacterProps) {
  return (
    <div className="goblin-strip">
      <div className={`goblin-avatar ${isAnimating ? 'animating' : ''} goblin-${state}`}>
        <span className="goblin-emoji">{STATE_EMOJI[state]}</span>
        {isAnimating && <div className="goblin-ring" />}
      </div>
      <div className="goblin-status">
        <div className="goblin-status-text">{text}</div>
        <div className="goblin-status-detail">{detail}</div>
      </div>
    </div>
  );
}
