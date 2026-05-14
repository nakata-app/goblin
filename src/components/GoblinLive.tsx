import React, { memo, useMemo, useEffect, useState, useRef } from 'react';
import type { EmotionalState, PresenceState, AnimationIntent } from '../character/types';
import { useAgentStore } from '../stores/agentStore';
import { useChatStore } from '../stores/chatStore';

interface GoblinLiveProps {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
}

// =================================================================
// Coordinate system (viewBox 0 0 320 380)
// Head center: (160, 110), head w=130 h=130 (pear shape)
// Shoulders at y=200, body torso 165-280
// Arms hang to y=300, hands at y=315
// =================================================================

type HandPose = {
  shoulder: { x: number; y: number };
  elbow: { x: number; y: number };
  wrist: { x: number; y: number };
  // Hand rotation in degrees; controls finger orientation.
  rot: number;
};

type Pose = { left: HandPose; right: HandPose };

const SHOULDER_L = { x: 122, y: 205 };
const SHOULDER_R = { x: 198, y: 205 };

const POSES: Record<string, Pose> = {
  idle: {
    left:  { shoulder: SHOULDER_L, elbow: { x: 105, y: 255 }, wrist: { x: 100, y: 305 }, rot: -10 },
    right: { shoulder: SHOULDER_R, elbow: { x: 215, y: 255 }, wrist: { x: 220, y: 305 }, rot: 10 },
  },
  thinking: {
    // right hand to chin
    left:  { shoulder: SHOULDER_L, elbow: { x: 108, y: 255 }, wrist: { x: 105, y: 305 }, rot: -10 },
    right: { shoulder: SHOULDER_R, elbow: { x: 200, y: 175 }, wrist: { x: 180, y: 150 }, rot: -45 },
  },
  speaking: {
    // open palms forward, slightly raised
    left:  { shoulder: SHOULDER_L, elbow: { x: 110, y: 240 }, wrist: { x: 95, y: 275 },  rot: -25 },
    right: { shoulder: SHOULDER_R, elbow: { x: 210, y: 240 }, wrist: { x: 225, y: 275 }, rot: 25 },
  },
  writing: {
    // both hands forward at desk level
    left:  { shoulder: SHOULDER_L, elbow: { x: 125, y: 255 }, wrist: { x: 140, y: 285 }, rot: 20 },
    right: { shoulder: SHOULDER_R, elbow: { x: 195, y: 255 }, wrist: { x: 180, y: 285 }, rot: -20 },
  },
  searching: {
    // right hand to brow (shading eyes)
    left:  { shoulder: SHOULDER_L, elbow: { x: 108, y: 255 }, wrist: { x: 105, y: 305 }, rot: -10 },
    right: { shoulder: SHOULDER_R, elbow: { x: 205, y: 160 }, wrist: { x: 175, y: 95 },  rot: -60 },
  },
  running: {
    left:  { shoulder: SHOULDER_L, elbow: { x: 95,  y: 250 }, wrist: { x: 80,  y: 290 }, rot: -25 },
    right: { shoulder: SHOULDER_R, elbow: { x: 225, y: 250 }, wrist: { x: 240, y: 290 }, rot: 25 },
  },
  error: {
    // arms wide open, shock
    left:  { shoulder: SHOULDER_L, elbow: { x: 80,  y: 235 }, wrist: { x: 55,  y: 280 }, rot: -45 },
    right: { shoulder: SHOULDER_R, elbow: { x: 240, y: 235 }, wrist: { x: 265, y: 280 }, rot: 45 },
  },
  success: {
    // both arms up in cheer
    left:  { shoulder: SHOULDER_L, elbow: { x: 110, y: 165 }, wrist: { x: 90,  y: 115 }, rot: -160 },
    right: { shoulder: SHOULDER_R, elbow: { x: 210, y: 165 }, wrist: { x: 230, y: 115 }, rot: 160 },
  },
};

function basePose(state: string): Pose {
  switch (state) {
    case 'thinking_deep':       return POSES.thinking;
    case 'reading_scan':        return POSES.thinking;
    case 'writing_focused':     return POSES.writing;
    case 'searching_explore':   return POSES.searching;
    case 'running_active':      return POSES.running;
    case 'error_shock':         return POSES.error;
    case 'success_celebrate':   return POSES.success;
    default:                    return POSES.idle;
  }
}

// =================================================================
// Hand component: 5 fingers with thumb. ~24px wide.
// =================================================================
function Hand({ x, y, rot, skin, stroke }: { x: number; y: number; rot: number; skin: string; stroke: string }) {
  return (
    <g
      transform={`translate(${x} ${y}) rotate(${rot})`}
      style={{ transition: 'transform 0.45s cubic-bezier(0.4, 0, 0.2, 1)' }}
    >
      {/* palm */}
      <ellipse cx="0" cy="2" rx="11" ry="9" fill={skin} stroke={stroke} strokeWidth="1.3" />
      {/* thumb */}
      <ellipse cx="-9" cy="-3" rx="4" ry="6" fill={skin} stroke={stroke} strokeWidth="1.1" transform="rotate(-25 -9 -3)" />
      {/* fingers */}
      <ellipse cx="-5" cy="-9" rx="2.5" ry="6" fill={skin} stroke={stroke} strokeWidth="1" />
      <ellipse cx="-1" cy="-10" rx="2.5" ry="7" fill={skin} stroke={stroke} strokeWidth="1" />
      <ellipse cx="3" cy="-10" rx="2.5" ry="6.5" fill={skin} stroke={stroke} strokeWidth="1" />
      <ellipse cx="7" cy="-8" rx="2.3" ry="5.5" fill={skin} stroke={stroke} strokeWidth="1" />
    </g>
  );
}

// =================================================================
// Mouth morph — 5 phonemes for lipsync + emotion paths
// =================================================================
function mouthPath(open: number, sat: number, frust: number, speaking: boolean): React.ReactElement {
  const cx = 160;
  const cy = 138;

  if (speaking) {
    // Phoneme-ish shapes based on open amount
    if (open < 0.15) {
      // closed line with tiny separation
      return <path d={`M ${cx - 12} ${cy} Q ${cx} ${cy + 1.5} ${cx + 12} ${cy}`}
        stroke="#2a1a10" strokeWidth="2.5" fill="none" strokeLinecap="round" />;
    }
    if (open < 0.4) {
      // small slit ("I" or soft consonant)
      return (
        <g>
          <ellipse cx={cx} cy={cy} rx="11" ry={2 + open * 5} fill="#2a1410" stroke="#1a0a08" strokeWidth="1.2" />
          <ellipse cx={cx} cy={cy + 1} rx="8" ry={Math.max(0.4, open * 2.5)} fill="rgba(210, 70, 80, 0.4)" />
        </g>
      );
    }
    if (open < 0.7) {
      // medium O shape
      return (
        <g>
          <ellipse cx={cx} cy={cy + 1} rx="9" ry={4 + open * 4} fill="#2a1410" stroke="#1a0a08" strokeWidth="1.3" />
          <ellipse cx={cx} cy={cy + 4} rx="6" ry={open * 3} fill="rgba(210, 70, 80, 0.55)" />
          {/* upper teeth hint */}
          <rect x={cx - 6} y={cy - (3 + open * 2)} width="12" height="2" fill="rgba(255, 250, 235, 0.9)" />
        </g>
      );
    }
    // wide open ("A")
    return (
      <g>
        <ellipse cx={cx} cy={cy + 2} rx="10" ry={6 + open * 5} fill="#1a0a08" stroke="#0a0506" strokeWidth="1.4" />
        <ellipse cx={cx} cy={cy + 6} rx="7" ry={open * 4} fill="rgba(210, 70, 80, 0.65)" />
        <rect x={cx - 7} y={cy - (5 + open * 2)} width="14" height="2.5" rx="0.5" fill="rgba(255, 250, 235, 0.95)" />
        {/* fangs in wide-open mouth */}
        <path d={`M ${cx - 4} ${cy - 4} L ${cx - 3} ${cy + open * 3} L ${cx - 2} ${cy - 4} Z`} fill="#fffaf0" />
        <path d={`M ${cx + 4} ${cy - 4} L ${cx + 3} ${cy + open * 3} L ${cx + 2} ${cy - 4} Z`} fill="#fffaf0" />
      </g>
    );
  }

  // Not speaking — emotion paths
  if (frust > 0.6) {
    // frown + bared fangs
    return (
      <g>
        <path d={`M ${cx - 14} ${cy + 4} Q ${cx} ${cy + 12} ${cx + 14} ${cy + 4}`} stroke="#2a1a10" strokeWidth="2.5" fill="none" strokeLinecap="round" />
        <path d={`M ${cx - 5} ${cy + 5} L ${cx - 4} ${cy + 11} L ${cx - 3} ${cy + 5} Z`} fill="#fffaf0" />
        <path d={`M ${cx + 5} ${cy + 5} L ${cx + 4} ${cy + 11} L ${cx + 3} ${cy + 5} Z`} fill="#fffaf0" />
      </g>
    );
  }
  if (sat > 0.6) {
    // big smile with fangs
    return (
      <g>
        <path d={`M ${cx - 16} ${cy - 1} Q ${cx} ${cy + 12} ${cx + 16} ${cy - 1}`} stroke="#2a1a10" strokeWidth="2.5" fill="#2a1410" strokeLinecap="round" />
        {/* upper teeth row */}
        <rect x={cx - 11} y={cy + 1} width="22" height="3.5" rx="0.5" fill="#fffaf0" />
        {/* fangs jutting down */}
        <path d={`M ${cx - 8} ${cy + 3} L ${cx - 7} ${cy + 9} L ${cx - 6} ${cy + 3} Z`} fill="#fffaf0" />
        <path d={`M ${cx + 8} ${cy + 3} L ${cx + 7} ${cy + 9} L ${cx + 6} ${cy + 3} Z`} fill="#fffaf0" />
      </g>
    );
  }
  if (sat > 0.3) {
    // slight smile
    return <path d={`M ${cx - 12} ${cy} Q ${cx} ${cy + 6} ${cx + 12} ${cy}`}
      stroke="#2a1a10" strokeWidth="2.5" fill="none" strokeLinecap="round" />;
  }
  // neutral
  return <path d={`M ${cx - 11} ${cy} Q ${cx} ${cy + 2} ${cx + 11} ${cy}`}
    stroke="#2a1a10" strokeWidth="2.5" fill="none" strokeLinecap="round" />;
}

export const GoblinLive = memo(function GoblinLive({
  emotionalState,
  presenceState,
  animationIntent,
}: GoblinLiveProps) {
  const { vector, mood, primaryEmotion, intensity } = emotionalState;
  const { blinkProgress, eyeGazeX, eyeGazeY, breathePhase, headTilt, earWiggle } = presenceState;
  const { animationState } = animationIntent;

  const goblinState = useAgentStore((s) => s.goblinState);
  const messages = useChatStore((s) => s.messages);
  const lastMessage = messages[messages.length - 1];
  const isSpeaking =
    goblinState === 'streaming' ||
    (goblinState === 'thinking' && Boolean(lastMessage) && lastMessage?.role === 'assistant');

  // Lipsync amplitude — animated via rAF
  const [mouthOpen, setMouthOpen] = useState(0);
  const [gestureSway, setGestureSway] = useState(0);
  const rafRef = useRef<number | null>(null);
  useEffect(() => {
    if (!isSpeaking) {
      setMouthOpen(0);
      setGestureSway(0);
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
      return;
    }
    let phase = 0;
    const tick = () => {
      phase += 0.16;
      const v =
        Math.abs(Math.sin(phase)) * 0.55 +
        Math.abs(Math.sin(phase * 1.9 + 1.2)) * 0.28 +
        Math.random() * 0.17;
      setMouthOpen(Math.min(1, v));
      setGestureSway(Math.sin(phase * 0.45));
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
    };
  }, [isSpeaking]);

  const breatheScale = 1 + Math.sin(breathePhase) * (presenceState.breatheAmplitude || 0.04) * 2.5;

  // Cartoon skin palette — moodful
  const skin = useMemo(() => {
    const palette: Record<string, { main: string; shade: string; cheek: string }> = {
      calm:        { main: '#7ab84a', shade: '#5a8a3c', cheek: '#e8a45c' },
      productive:  { main: '#82c052', shade: '#608c3e', cheek: '#e8a45c' },
      tense:       { main: '#a89248', shade: '#7a6a35', cheek: '#d97755' },
      playful:     { main: '#86c450', shade: '#658d3e', cheek: '#f4b074' },
      tired:       { main: '#6a8848', shade: '#506634', cheek: '#c89060' },
      supportive:  { main: '#74b245', shade: '#578437', cheek: '#e8a45c' },
      celebratory: { main: '#8ed058', shade: '#6aa040', cheek: '#fdc080' },
    };
    return palette[mood] ?? palette.calm;
  }, [mood]);

  const stroke = '#2c4422';

  const eyeColor = (vector?.frustration ?? 0) > 0.5 ? '#ff5544'
    : (vector?.curiosity ?? 0) > 0.7 ? '#44ccff'
    : (vector?.focus ?? 0) > 0.7 ? '#ffcc33'
    : '#fdde55';

  const eyeOpen = 1 - (blinkProgress ?? 0);
  const pupilX = (eyeGazeX ?? 0) * 4;
  const pupilY = (eyeGazeY ?? 0) * 3;

  const frust = vector?.frustration ?? 0;
  const sat = vector?.satisfaction ?? 0;

  // Eyebrow shapes — angled vs relaxed
  const browAngle = frust > 0.4 ? 12 : (sat > 0.5 ? -6 : 0);

  const ringColor = frust > 0.5 ? 'rgba(239,68,68,' : 'rgba(16,185,129,';
  const ringOpacity = 0.08 + (intensity ?? 0) * 0.12;

  const isActive = animationState !== 'idle_breathe';

  // Pose with subtle gesture sway when speaking
  const pose = useMemo<Pose>(() => {
    const p = isSpeaking ? POSES.speaking : basePose(animationState);
    if (!isSpeaking) return p;
    const dx = gestureSway * 6;
    const dy = Math.abs(gestureSway) * 4;
    return {
      left:  { ...p.left,  wrist: { x: p.left.wrist.x + dx,  y: p.left.wrist.y - dy }, elbow: { x: p.left.elbow.x + dx / 2, y: p.left.elbow.y - dy / 2 } },
      right: { ...p.right, wrist: { x: p.right.wrist.x - dx, y: p.right.wrist.y - dy }, elbow: { x: p.right.elbow.x - dx / 2, y: p.right.elbow.y - dy / 2 } },
    };
  }, [animationState, isSpeaking, gestureSway]);

  const stateLabel = isSpeaking ? 'Speaking'
    : animationState === 'idle_breathe' ? 'Ready'
    : animationState === 'thinking_deep' ? 'Thinking'
    : animationState === 'reading_scan' ? 'Reading'
    : animationState === 'writing_focused' ? 'Writing'
    : animationState === 'searching_explore' ? 'Searching'
    : animationState === 'running_active' ? 'Running'
    : animationState === 'error_shock' ? 'Error'
    : animationState === 'success_celebrate' ? 'Done'
    : (primaryEmotion ?? 'Ready');

  return (
    <div className={`goblin-live${isActive ? ' goblin-live-active' : ''}${isSpeaking ? ' goblin-live-speaking' : ''}`}>
      <div className="goblin-live-container">
        <svg
          width="320"
          height="380"
          viewBox="0 0 320 380"
          className="goblin-live-svg"
        >
          <defs>
            {/* Gradient for skin shading */}
            <radialGradient id="skinGrad" cx="0.35" cy="0.35" r="0.85">
              <stop offset="0%" stopColor={skin.main} />
              <stop offset="70%" stopColor={skin.main} />
              <stop offset="100%" stopColor={skin.shade} />
            </radialGradient>
            <radialGradient id="tunicGrad" cx="0.5" cy="0.3" r="0.8">
              <stop offset="0%" stopColor="#4a3220" />
              <stop offset="100%" stopColor="#2e1f12" />
            </radialGradient>
          </defs>

          {/* Atmosphere rings */}
          <circle cx="160" cy="135" r="155" fill="none" stroke={ringColor + ringOpacity + ')'} strokeWidth="1.5" />
          <circle cx="160" cy="135" r="135" fill="none" stroke={ringColor + (ringOpacity * 1.5) + ')'} strokeWidth="1" />
          <circle cx="160" cy="135" r="115" fill="none" stroke={ringColor + (ringOpacity * 0.7) + ')'} strokeWidth="0.5" />

          {/* Speaking pulse */}
          {isSpeaking && (
            <>
              <circle cx="160" cy="135" r={115 + mouthOpen * 10} fill="none" stroke="rgba(16,185,129,0.35)" strokeWidth="2" opacity={0.4 + mouthOpen * 0.4} />
              <circle cx="160" cy="135" r={130 + mouthOpen * 14} fill="none" stroke="rgba(16,185,129,0.20)" strokeWidth="1.5" opacity={0.3 + mouthOpen * 0.3} />
            </>
          )}

          {/* ============ BODY ============ */}
          <g style={{ transformOrigin: '160px 240px', transform: `scale(${0.99 + (isSpeaking ? mouthOpen * 0.005 : 0)})`, transition: 'transform 0.1s ease' }}>
            {/* Tunic */}
            <path
              d="M 110 200 Q 110 175 130 175 L 190 175 Q 210 175 210 200
                 L 220 290 Q 220 305 205 305 L 115 305 Q 100 305 100 290 Z"
              fill="url(#tunicGrad)" stroke="#1a0e08" strokeWidth="1.8"
            />
            {/* Belt */}
            <rect x="103" y="260" width="114" height="9" fill="#5a3a1a" stroke="#2a1a08" strokeWidth="1" />
            <rect x="148" y="259" width="24" height="11" fill="#c89a3a" stroke="#2a1a08" strokeWidth="1" rx="1" />
            <rect x="156" y="262" width="8" height="5" fill="#2a1a08" rx="0.5" />

            {/* Tunic V-neck collar shadow */}
            <path d="M 140 175 L 160 200 L 180 175" fill="none" stroke="#1a0e08" strokeWidth="1.5" strokeLinecap="round" />
            {/* Tunic stitch line */}
            <line x1="160" y1="200" x2="160" y2="260" stroke="#1a0e08" strokeWidth="0.8" opacity="0.5" strokeDasharray="2 3" />
          </g>

          {/* ============ ARMS ============ */}
          {/* Left arm: shoulder -> elbow -> wrist */}
          <path
            d={`M ${pose.left.shoulder.x} ${pose.left.shoulder.y} Q ${pose.left.elbow.x} ${pose.left.elbow.y}, ${pose.left.wrist.x} ${pose.left.wrist.y}`}
            fill="none" stroke={skin.main} strokeWidth="16" strokeLinecap="round"
            style={{ transition: 'd 0.45s cubic-bezier(0.4, 0, 0.2, 1)' }}
          />
          <path
            d={`M ${pose.left.shoulder.x} ${pose.left.shoulder.y} Q ${pose.left.elbow.x} ${pose.left.elbow.y}, ${pose.left.wrist.x} ${pose.left.wrist.y}`}
            fill="none" stroke={stroke} strokeWidth="1.5" strokeLinecap="round"
            style={{ transition: 'd 0.45s cubic-bezier(0.4, 0, 0.2, 1)' }}
          />
          <Hand x={pose.left.wrist.x} y={pose.left.wrist.y} rot={pose.left.rot} skin={skin.main} stroke={stroke} />

          {/* Right arm */}
          <path
            d={`M ${pose.right.shoulder.x} ${pose.right.shoulder.y} Q ${pose.right.elbow.x} ${pose.right.elbow.y}, ${pose.right.wrist.x} ${pose.right.wrist.y}`}
            fill="none" stroke={skin.main} strokeWidth="16" strokeLinecap="round"
            style={{ transition: 'd 0.45s cubic-bezier(0.4, 0, 0.2, 1)' }}
          />
          <path
            d={`M ${pose.right.shoulder.x} ${pose.right.shoulder.y} Q ${pose.right.elbow.x} ${pose.right.elbow.y}, ${pose.right.wrist.x} ${pose.right.wrist.y}`}
            fill="none" stroke={stroke} strokeWidth="1.5" strokeLinecap="round"
            style={{ transition: 'd 0.45s cubic-bezier(0.4, 0, 0.2, 1)' }}
          />
          <Hand x={pose.right.wrist.x} y={pose.right.wrist.y} rot={pose.right.rot} skin={skin.main} stroke={stroke} />

          {/* ============ HEAD ============ */}
          <g
            style={{
              transformOrigin: '160px 110px',
              transform: `rotate(${headTilt ?? 0}deg) scale(${breatheScale})`,
              transition: 'transform 0.3s ease',
            }}
          >
            {/* Ears (behind head) */}
            <g style={{ transformOrigin: '85px 95px', transform: `scale(${1 + (earWiggle ?? 0) * 0.18}) rotate(${(earWiggle ?? 0) * 6}deg)` }}>
              <path d="M 105 75 L 78 50 Q 70 48 72 60 L 110 110 Z"
                fill={skin.shade} stroke={stroke} strokeWidth="1.5" strokeLinejoin="round" />
              {/* inner ear */}
              <path d="M 100 75 L 86 60 L 105 100 Z" fill={skin.cheek} opacity="0.6" />
            </g>
            <g style={{ transformOrigin: '235px 95px', transform: `scale(${1 + (earWiggle ?? 0) * 0.18}) rotate(${-(earWiggle ?? 0) * 6}deg)` }}>
              <path d="M 215 75 L 242 50 Q 250 48 248 60 L 210 110 Z"
                fill={skin.shade} stroke={stroke} strokeWidth="1.5" strokeLinejoin="round" />
              <path d="M 220 75 L 234 60 L 215 100 Z" fill={skin.cheek} opacity="0.6" />
            </g>

            {/* Head — pear shape (narrower top) */}
            <path
              d="M 160 50
                 C 115 50, 95 75, 95 110
                 C 95 145, 115 175, 160 175
                 C 205 175, 225 145, 225 110
                 C 225 75, 205 50, 160 50 Z"
              fill="url(#skinGrad)" stroke={stroke} strokeWidth="2"
            />
            {/* Jaw shadow */}
            <path d="M 110 130 Q 160 195 210 130" fill={skin.shade} opacity="0.32" />
            {/* Chin highlight */}
            <ellipse cx="160" cy="160" rx="22" ry="8" fill={skin.main} opacity="0.5" />

            {/* Hair tuft (mohawk) */}
            <path d="M 145 55 Q 150 30 158 40 Q 162 25 168 38 Q 174 28 178 45 Q 175 55 168 55 Z"
              fill="#2a1a08" stroke="#1a0a04" strokeWidth="1.3" strokeLinejoin="round" />
            <path d="M 158 38 L 162 50 M 168 36 L 170 52" stroke="#3a2a14" strokeWidth="0.7" />

            {/* Forehead wrinkle */}
            <path d="M 135 78 Q 160 73 185 78" fill="none" stroke={stroke} strokeWidth="0.8" opacity="0.45" strokeLinecap="round" />

            {/* Eyebrows */}
            <g style={{ transformOrigin: '135px 90px', transform: `rotate(${browAngle}deg)` }}>
              <path d="M 117 88 Q 130 84 145 90 L 143 92 Q 130 88 119 92 Z"
                fill="#1a0a04" stroke="#1a0a04" strokeWidth="0.8" strokeLinejoin="round" />
            </g>
            <g style={{ transformOrigin: '185px 90px', transform: `rotate(${-browAngle}deg)` }}>
              <path d="M 175 90 Q 190 84 203 88 L 201 92 Q 190 88 177 92 Z"
                fill="#1a0a04" stroke="#1a0a04" strokeWidth="0.8" strokeLinejoin="round" />
            </g>

            {/* Eye sockets (slight depth shadow) */}
            <ellipse cx="132" cy="103" rx="15" ry="11" fill="#1a0a04" opacity="0.25" />
            <ellipse cx="188" cy="103" rx="15" ry="11" fill="#1a0a04" opacity="0.25" />

            {/* Eyes (big cartoon whites) */}
            <ellipse cx="132" cy="103" rx="13" ry={11 * eyeOpen} fill="#fdfaf2" stroke={stroke} strokeWidth="1.4" />
            <ellipse cx="188" cy="103" rx="13" ry={11 * eyeOpen} fill="#fdfaf2" stroke={stroke} strokeWidth="1.4" />

            {/* Pupils with iris ring */}
            {eyeOpen > 0.15 && (
              <>
                <circle cx={132 + pupilX} cy={103 + pupilY} r="6.5" fill={eyeColor} />
                <circle cx={132 + pupilX} cy={103 + pupilY} r="3.2" fill="#0a0608" />
                <circle cx={132 + pupilX + 1.5} cy={103 + pupilY - 1.5} r="1.5" fill="#fff" />
                <circle cx={188 + pupilX} cy={103 + pupilY} r="6.5" fill={eyeColor} />
                <circle cx={188 + pupilX} cy={103 + pupilY} r="3.2" fill="#0a0608" />
                <circle cx={188 + pupilX + 1.5} cy={103 + pupilY - 1.5} r="1.5" fill="#fff" />
              </>
            )}

            {/* Lower eyelashes hint */}
            {eyeOpen > 0.5 && (
              <>
                <path d="M 122 110 Q 132 113 142 110" stroke={stroke} strokeWidth="0.6" fill="none" opacity="0.5" />
                <path d="M 178 110 Q 188 113 198 110" stroke={stroke} strokeWidth="0.6" fill="none" opacity="0.5" />
              </>
            )}

            {/* Nose (small upturned button) */}
            <path d="M 155 118 Q 152 128 156 132 Q 160 134 164 132 Q 168 128 165 118 Q 160 117 155 118 Z"
              fill={skin.shade} stroke={stroke} strokeWidth="1.2" strokeLinejoin="round" />
            <ellipse cx="157" cy="129" rx="1.2" ry="1.8" fill="#1a0a04" opacity="0.7" />
            <ellipse cx="163" cy="129" rx="1.2" ry="1.8" fill="#1a0a04" opacity="0.7" />

            {/* Cheek blush */}
            <ellipse cx="118" cy="128" rx="9" ry="5" fill={skin.cheek} opacity="0.45" />
            <ellipse cx="202" cy="128" rx="9" ry="5" fill={skin.cheek} opacity="0.45" />

            {/* Mouth */}
            {mouthPath(mouthOpen, sat, frust, isSpeaking)}

            {/* Wart (character touch) */}
            <circle cx="146" cy="148" r="2.2" fill={skin.shade} stroke={stroke} strokeWidth="0.8" />
            <circle cx="146" cy="148" r="0.8" fill="#3a2a14" />

            {/* Highlight on forehead */}
            <ellipse cx="140" cy="75" rx="14" ry="6" fill="#fff" opacity="0.07" />
          </g>

          {/* ============ FEET (peek under tunic) ============ */}
          <ellipse cx="138" cy="320" rx="18" ry="6" fill="#2a1a08" stroke="#1a0e04" strokeWidth="1.2" />
          <ellipse cx="182" cy="320" rx="18" ry="6" fill="#2a1a08" stroke="#1a0e04" strokeWidth="1.2" />
          {/* Toenails */}
          <ellipse cx="125" cy="319" rx="1.5" ry="1.2" fill="#c8b080" />
          <ellipse cx="195" cy="319" rx="1.5" ry="1.2" fill="#c8b080" />
        </svg>

        <div className="goblin-live-label">{stateLabel}</div>
        <div className="goblin-live-hint">{animationState}</div>
      </div>
    </div>
  );
});
