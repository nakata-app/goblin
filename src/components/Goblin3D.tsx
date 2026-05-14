// ═══════════════════════════════════════════════════════════════════
// Goblin3D — realistic three.js character for the center panel
// Procedural geometry: head + body + arms + lipsync + gestures.
// Driven by emotional state, presence state, agent activity.
// ═══════════════════════════════════════════════════════════════════

import { useRef, useMemo, useEffect, useState } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { Float, Environment, ContactShadows } from '@react-three/drei';
import * as THREE from 'three';
import type { EmotionalState, PresenceState, AnimationIntent } from '../character/types';

// ──────────────────────────────────────────────────────────────────
// Arm rig — shoulder, upper-arm, forearm, hand. Pose by 3 angles.
// ──────────────────────────────────────────────────────────────────
interface ArmTargets {
  shoulderX: number; // rotation around X (forward/back)
  shoulderZ: number; // rotation around Z (in/out, side)
  elbow: number;     // rotation at elbow on X
}

interface PoseTargets {
  left: ArmTargets;
  right: ArmTargets;
  headTiltZ: number;
  headTiltX: number;
}

// Pose presets by animation state. Z mirrors for left vs right.
const POSES: Record<string, PoseTargets> = {
  idle:      { left: { shoulderX: 0.05, shoulderZ:  0.05, elbow: -0.10 }, right: { shoulderX: 0.05, shoulderZ: -0.05, elbow: -0.10 }, headTiltZ: 0, headTiltX: 0 },
  thinking: { left: { shoulderX: 0.05, shoulderZ:  0.05, elbow: -0.10 }, right: { shoulderX: -1.20, shoulderZ: -0.20, elbow: -1.80 }, headTiltZ: -0.10, headTiltX: -0.05 },
  speaking: { left: { shoulderX: -0.50, shoulderZ:  0.30, elbow: -1.20 }, right: { shoulderX: -0.50, shoulderZ: -0.30, elbow: -1.20 }, headTiltZ: 0, headTiltX: 0 },
  writing:  { left: { shoulderX: -0.90, shoulderZ:  0.05, elbow: -1.30 }, right: { shoulderX: -0.90, shoulderZ: -0.05, elbow: -1.30 }, headTiltZ: 0, headTiltX:  0.15 },
  searching:{ left: { shoulderX: 0.05, shoulderZ:  0.05, elbow: -0.10 }, right: { shoulderX: -2.20, shoulderZ: -0.40, elbow: -1.40 }, headTiltZ: 0, headTiltX: -0.05 },
  running:  { left: { shoulderX: -0.30, shoulderZ:  0.10, elbow: -0.40 }, right: { shoulderX: -0.30, shoulderZ: -0.10, elbow: -0.40 }, headTiltZ: 0, headTiltX: 0 },
  error:    { left: { shoulderX: -0.20, shoulderZ:  0.80, elbow: -0.30 }, right: { shoulderX: -0.20, shoulderZ: -0.80, elbow: -0.30 }, headTiltZ: 0, headTiltX: -0.20 },
  success:  { left: { shoulderX: -2.40, shoulderZ:  0.20, elbow: -0.30 }, right: { shoulderX: -2.40, shoulderZ: -0.20, elbow: -0.30 }, headTiltZ: 0, headTiltX: -0.10 },
};

function poseFor(state: string): PoseTargets {
  switch (state) {
    case 'thinking_deep':     return POSES.thinking;
    case 'reading_scan':      return POSES.thinking;
    case 'writing_focused':   return POSES.writing;
    case 'searching_explore': return POSES.searching;
    case 'running_active':    return POSES.running;
    case 'error_shock':       return POSES.error;
    case 'success_celebrate': return POSES.success;
    default:                  return POSES.idle;
  }
}

// ──────────────────────────────────────────────────────────────────
// Arm component — 3 segments
// ──────────────────────────────────────────────────────────────────
function Arm({ side, skin, stroke, target, sway }: {
  side: 'L' | 'R';
  skin: THREE.Color;
  stroke: THREE.Color;
  target: ArmTargets;
  sway: number;
}) {
  const shoulderRef = useRef<THREE.Group>(null);
  const forearmRef = useRef<THREE.Group>(null);
  const sign = side === 'L' ? -1 : 1;

  useFrame((_, delta) => {
    if (!shoulderRef.current || !forearmRef.current) return;
    const s = shoulderRef.current;
    const f = forearmRef.current;
    // Lerp toward target with sway overlay
    s.rotation.x += (target.shoulderX + sway * 0.04 - s.rotation.x) * Math.min(1, delta * 5);
    s.rotation.z += (sign * target.shoulderZ + sway * 0.05 * sign - s.rotation.z) * Math.min(1, delta * 5);
    f.rotation.x += (target.elbow - f.rotation.x) * Math.min(1, delta * 5);
  });

  return (
    <group position={[sign * 0.55, 1.30, 0]}>
      <group ref={shoulderRef}>
        {/* upper arm */}
        <mesh position={[0, -0.32, 0]} castShadow>
          <capsuleGeometry args={[0.13, 0.50, 8, 16]} />
          <meshStandardMaterial color={skin} roughness={0.55} />
        </mesh>
        {/* elbow */}
        <group ref={forearmRef} position={[0, -0.65, 0]}>
          <mesh position={[0, -0.30, 0]} castShadow>
            <capsuleGeometry args={[0.115, 0.45, 8, 16]} />
            <meshStandardMaterial color={skin} roughness={0.55} />
          </mesh>
          {/* hand */}
          <group position={[0, -0.62, 0]}>
            <mesh castShadow>
              <sphereGeometry args={[0.14, 16, 16]} />
              <meshStandardMaterial color={skin} roughness={0.50} />
            </mesh>
            {/* fingers (5 small capsules) */}
            {[-0.08, -0.04, 0, 0.04].map((dx, i) => (
              <mesh key={i} position={[dx, -0.15, 0.04]} castShadow>
                <capsuleGeometry args={[0.028, 0.12, 6, 12]} />
                <meshStandardMaterial color={skin} roughness={0.55} />
              </mesh>
            ))}
            {/* thumb */}
            <mesh position={[-0.13, -0.05, 0.06]} rotation={[0, 0, 0.6]} castShadow>
              <capsuleGeometry args={[0.032, 0.10, 6, 12]} />
              <meshStandardMaterial color={skin} roughness={0.55} />
            </mesh>
            {/* hand outline shadow */}
            <mesh>
              <sphereGeometry args={[0.143, 8, 8]} />
              <meshBasicMaterial color={stroke} transparent opacity={0.0} />
            </mesh>
          </group>
        </group>
      </group>
    </group>
  );
}

// ──────────────────────────────────────────────────────────────────
// Mouth — animated jaw drop + lip shape (vertical scale)
// ──────────────────────────────────────────────────────────────────
function Mouth({ open, sat, frust, speaking }: {
  open: number; sat: number; frust: number; speaking: boolean;
}) {
  const groupRef = useRef<THREE.Group>(null);
  const innerRef = useRef<THREE.Mesh>(null);

  useFrame(() => {
    if (!innerRef.current) return;
    const target = speaking ? 0.3 + open * 0.7 : 0.15;
    innerRef.current.scale.y += (target - innerRef.current.scale.y) * 0.25;
  });

  // Curve for non-speaking lip line
  const lipRotZ = sat > 0.5 ? -0.10 : (frust > 0.5 ? 0.10 : 0);

  return (
    <group ref={groupRef} position={[0, -0.30, 0.85]}>
      {/* Lip outline */}
      <mesh rotation={[0, 0, lipRotZ]}>
        <torusGeometry args={[0.16, 0.025, 8, 24, Math.PI * 1.0]} />
        <meshStandardMaterial color="#3a1c12" roughness={0.4} />
      </mesh>
      {/* Inner mouth (dark cavity) — scaled vertically by lipsync */}
      <mesh ref={innerRef} position={[0, -0.02, 0.005]} scale={[1, 0.15, 0.4]}>
        <sphereGeometry args={[0.14, 18, 18]} />
        <meshStandardMaterial color="#1a0808" roughness={0.95} />
      </mesh>
      {/* Tongue glimpse when wide open */}
      {speaking && open > 0.45 && (
        <mesh position={[0, -0.05 - open * 0.04, 0.03]} scale={[1, 0.6, 0.3]}>
          <sphereGeometry args={[0.09, 12, 12]} />
          <meshStandardMaterial color="#c84455" roughness={0.6} />
        </mesh>
      )}
      {/* Upper fangs visible when smile or wide mouth */}
      {(sat > 0.5 || (speaking && open > 0.55)) && (
        <>
          <mesh position={[-0.05, 0.01, 0.06]}>
            <coneGeometry args={[0.018, 0.06, 8]} />
            <meshStandardMaterial color="#fffaf0" roughness={0.3} />
          </mesh>
          <mesh position={[0.05, 0.01, 0.06]}>
            <coneGeometry args={[0.018, 0.06, 8]} />
            <meshStandardMaterial color="#fffaf0" roughness={0.3} />
          </mesh>
        </>
      )}
    </group>
  );
}

// ──────────────────────────────────────────────────────────────────
// Head — pear shape via scaled sphere + ears + hair tuft
// ──────────────────────────────────────────────────────────────────
function Head({
  emotionalState,
  presenceState,
  animationIntent: _animationIntent,
  isSpeaking,
  mouthOpen,
  pose,
}: {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
  isSpeaking: boolean;
  mouthOpen: number;
  pose: PoseTargets;
}) {
  const groupRef = useRef<THREE.Group>(null);
  const leftEyeRef = useRef<THREE.Mesh>(null);
  const rightEyeRef = useRef<THREE.Mesh>(null);
  const earL = useRef<THREE.Group>(null);
  const earR = useRef<THREE.Group>(null);

  const { vector } = emotionalState;
  const { blinkProgress, eyeGazeX, eyeGazeY, breathePhase, earWiggle } = presenceState;

  const skin = useMemo(() => {
    const m: Record<string, string> = {
      calm: '#7ab84a',
      productive: '#82c052',
      tense: '#a89248',
      playful: '#86c450',
      tired: '#6a8848',
      supportive: '#74b245',
      celebratory: '#8ed058',
    };
    return new THREE.Color(m[emotionalState.mood] ?? '#7ab84a');
  }, [emotionalState.mood]);

  const stroke = useMemo(() => new THREE.Color('#2c4422'), []);
  const eyeColor = useMemo(() => {
    if (vector.frustration > 0.5) return new THREE.Color('#ff5544');
    if (vector.curiosity > 0.7)   return new THREE.Color('#44ccff');
    if (vector.focus > 0.7)       return new THREE.Color('#ffcc33');
    return new THREE.Color('#fdde55');
  }, [vector.frustration, vector.curiosity, vector.focus]);

  useFrame((_, delta) => {
    if (!groupRef.current) return;
    // Breathing
    const s = 1 + Math.sin(breathePhase) * 0.018;
    groupRef.current.scale.setScalar(s);
    // Head tilt — converge to pose target
    groupRef.current.rotation.z += (pose.headTiltZ - groupRef.current.rotation.z) * Math.min(1, delta * 4);
    groupRef.current.rotation.x += (pose.headTiltX - groupRef.current.rotation.x) * Math.min(1, delta * 4);

    // Eye blink
    const open = 1 - (blinkProgress ?? 0);
    if (leftEyeRef.current && rightEyeRef.current) {
      leftEyeRef.current.scale.y = Math.max(0.05, open);
      rightEyeRef.current.scale.y = Math.max(0.05, open);
    }

    // Ear wiggle
    const w = (earWiggle ?? 0) * 0.25;
    if (earL.current) earL.current.rotation.z = 0.4 + w;
    if (earR.current) earR.current.rotation.z = -0.4 - w;
  });

  return (
    <group ref={groupRef} position={[0, 2.1, 0]}>
      {/* Head — pear: scale Y > X to elongate, narrower top */}
      <mesh castShadow scale={[1.0, 1.08, 1.0]} position={[0, -0.06, 0]}>
        <sphereGeometry args={[0.85, 48, 48]} />
        <meshPhysicalMaterial
          color={skin}
          roughness={0.62}
          metalness={0.0}
          sheen={0.5}
          sheenRoughness={0.8}
          sheenColor={skin.clone().multiplyScalar(1.2)}
          clearcoat={0.15}
          clearcoatRoughness={0.4}
        />
      </mesh>

      {/* Forehead highlight */}
      <mesh position={[-0.15, 0.45, 0.62]} scale={[0.5, 0.2, 0.1]}>
        <sphereGeometry args={[0.3, 12, 12]} />
        <meshBasicMaterial color="#ffffff" transparent opacity={0.06} />
      </mesh>

      {/* Hair tuft (mohawk) */}
      <group position={[0, 0.85, 0]}>
        {[-0.08, 0, 0.08].map((dx, i) => (
          <mesh key={i} position={[dx, 0.08 + Math.abs(dx) * 0.1, 0]} rotation={[0, 0, dx * 2]} castShadow>
            <coneGeometry args={[0.06, 0.30 - Math.abs(dx) * 0.6, 6]} />
            <meshStandardMaterial color="#2a1a08" roughness={0.7} />
          </mesh>
        ))}
      </group>

      {/* Ears — pointy, rotated outward */}
      <group ref={earL} position={[-0.78, 0.10, 0]}>
        <mesh castShadow>
          <coneGeometry args={[0.20, 0.55, 12]} />
          <meshStandardMaterial color={skin.clone().multiplyScalar(0.85)} roughness={0.6} />
        </mesh>
        {/* inner ear pink */}
        <mesh position={[0, -0.05, 0.05]} scale={[0.5, 0.7, 0.4]}>
          <coneGeometry args={[0.18, 0.45, 8]} />
          <meshStandardMaterial color="#d97a55" roughness={0.5} />
        </mesh>
      </group>
      <group ref={earR} position={[0.78, 0.10, 0]}>
        <mesh castShadow>
          <coneGeometry args={[0.20, 0.55, 12]} />
          <meshStandardMaterial color={skin.clone().multiplyScalar(0.85)} roughness={0.6} />
        </mesh>
        <mesh position={[0, -0.05, 0.05]} scale={[0.5, 0.7, 0.4]}>
          <coneGeometry args={[0.18, 0.45, 8]} />
          <meshStandardMaterial color="#d97a55" roughness={0.5} />
        </mesh>
      </group>

      {/* Eyebrows */}
      <mesh position={[-0.30, 0.28, 0.78]} rotation={[0, 0, vector.frustration > 0.4 ? 0.25 : -0.05]}>
        <boxGeometry args={[0.28, 0.05, 0.04]} />
        <meshStandardMaterial color="#1a0a04" roughness={0.5} />
      </mesh>
      <mesh position={[0.30, 0.28, 0.78]} rotation={[0, 0, vector.frustration > 0.4 ? -0.25 : 0.05]}>
        <boxGeometry args={[0.28, 0.05, 0.04]} />
        <meshStandardMaterial color="#1a0a04" roughness={0.5} />
      </mesh>

      {/* Eye sockets (slight dark inset) */}
      <mesh position={[-0.30, 0.10, 0.72]}>
        <sphereGeometry args={[0.18, 24, 24]} />
        <meshStandardMaterial color="#0a0608" roughness={0.7} />
      </mesh>
      <mesh position={[0.30, 0.10, 0.72]}>
        <sphereGeometry args={[0.18, 24, 24]} />
        <meshStandardMaterial color="#0a0608" roughness={0.7} />
      </mesh>

      {/* Eye whites */}
      <mesh ref={leftEyeRef} position={[-0.30, 0.10, 0.79]}>
        <sphereGeometry args={[0.14, 24, 24]} />
        <meshStandardMaterial color="#fdfaf2" roughness={0.25} />
      </mesh>
      <mesh ref={rightEyeRef} position={[0.30, 0.10, 0.79]}>
        <sphereGeometry args={[0.14, 24, 24]} />
        <meshStandardMaterial color="#fdfaf2" roughness={0.25} />
      </mesh>

      {/* Pupils — iris + black + spec highlight */}
      <group position={[-0.30 + (eyeGazeX ?? 0) * 0.05, 0.10 + (eyeGazeY ?? 0) * 0.04, 0.91]}>
        <mesh>
          <sphereGeometry args={[0.07, 16, 16]} />
          <meshStandardMaterial color={eyeColor} emissive={eyeColor} emissiveIntensity={0.4} roughness={0.3} />
        </mesh>
        <mesh position={[0, 0, 0.03]}>
          <sphereGeometry args={[0.035, 12, 12]} />
          <meshStandardMaterial color="#0a0608" roughness={0.2} />
        </mesh>
        <mesh position={[0.018, 0.018, 0.045]}>
          <sphereGeometry args={[0.013, 8, 8]} />
          <meshBasicMaterial color="#ffffff" />
        </mesh>
      </group>
      <group position={[0.30 + (eyeGazeX ?? 0) * 0.05, 0.10 + (eyeGazeY ?? 0) * 0.04, 0.91]}>
        <mesh>
          <sphereGeometry args={[0.07, 16, 16]} />
          <meshStandardMaterial color={eyeColor} emissive={eyeColor} emissiveIntensity={0.4} roughness={0.3} />
        </mesh>
        <mesh position={[0, 0, 0.03]}>
          <sphereGeometry args={[0.035, 12, 12]} />
          <meshStandardMaterial color="#0a0608" roughness={0.2} />
        </mesh>
        <mesh position={[0.018, 0.018, 0.045]}>
          <sphereGeometry args={[0.013, 8, 8]} />
          <meshBasicMaterial color="#ffffff" />
        </mesh>
      </group>

      {/* Nose — button, upturned */}
      <mesh position={[0, -0.10, 0.84]} rotation={[0.3, 0, 0]}>
        <sphereGeometry args={[0.10, 16, 16]} />
        <meshStandardMaterial color={skin.clone().multiplyScalar(0.85)} roughness={0.55} />
      </mesh>
      {/* nostrils */}
      <mesh position={[-0.035, -0.13, 0.92]}>
        <sphereGeometry args={[0.014, 8, 8]} />
        <meshBasicMaterial color="#1a0a04" />
      </mesh>
      <mesh position={[0.035, -0.13, 0.92]}>
        <sphereGeometry args={[0.014, 8, 8]} />
        <meshBasicMaterial color="#1a0a04" />
      </mesh>

      {/* Cheek blush */}
      <mesh position={[-0.55, -0.08, 0.55]}>
        <sphereGeometry args={[0.12, 16, 16]} />
        <meshBasicMaterial color="#e8825c" transparent opacity={0.25} />
      </mesh>
      <mesh position={[0.55, -0.08, 0.55]}>
        <sphereGeometry args={[0.12, 16, 16]} />
        <meshBasicMaterial color="#e8825c" transparent opacity={0.25} />
      </mesh>

      {/* Mouth */}
      <Mouth
        open={mouthOpen}
        sat={vector.satisfaction ?? 0}
        frust={vector.frustration ?? 0}
        speaking={isSpeaking}
      />

      {/* Wart on jaw */}
      <mesh position={[-0.38, -0.25, 0.65]} castShadow>
        <sphereGeometry args={[0.04, 12, 12]} />
        <meshStandardMaterial color={skin.clone().multiplyScalar(0.75)} roughness={0.7} />
      </mesh>

      {/* Tiny chin shadow */}
      <mesh position={[0, -0.42, 0.55]} scale={[1, 0.4, 0.4]}>
        <sphereGeometry args={[0.22, 16, 16]} />
        <meshBasicMaterial color="#000000" transparent opacity={0.15} />
      </mesh>

      {/* discreet stroke param reference to avoid unused */}
      <mesh visible={false}><sphereGeometry args={[0.01]} /><meshBasicMaterial color={stroke} /></mesh>
    </group>
  );
}

// ──────────────────────────────────────────────────────────────────
// Body (torso + tunic + belt + simple legs)
// ──────────────────────────────────────────────────────────────────
function Body({ skin, swayPhase }: { skin: THREE.Color; swayPhase: number }) {
  void skin;
  const ref = useRef<THREE.Group>(null);
  useFrame(() => {
    if (!ref.current) return;
    ref.current.rotation.y = Math.sin(swayPhase * 0.6) * 0.06;
  });
  return (
    <group ref={ref}>
      {/* Torso */}
      <mesh position={[0, 0.95, 0]} castShadow scale={[1, 1.05, 0.85]}>
        <capsuleGeometry args={[0.55, 0.45, 12, 24]} />
        <meshStandardMaterial color="#3e2a16" roughness={0.7} />
      </mesh>

      {/* Tunic V-neck collar trim */}
      <mesh position={[0, 1.45, 0.45]} rotation={[0.3, 0, 0]}>
        <ringGeometry args={[0.20, 0.30, 18, 1, 0, Math.PI * 1.0]} />
        <meshStandardMaterial color="#2a1a08" side={THREE.DoubleSide} />
      </mesh>

      {/* Belt */}
      <mesh position={[0, 0.50, 0]} rotation={[0, 0, 0]}>
        <torusGeometry args={[0.50, 0.06, 10, 32]} />
        <meshStandardMaterial color="#5a3a1a" roughness={0.6} />
      </mesh>
      {/* Belt buckle */}
      <mesh position={[0, 0.50, 0.50]}>
        <boxGeometry args={[0.16, 0.10, 0.04]} />
        <meshStandardMaterial color="#c89a3a" roughness={0.35} metalness={0.6} />
      </mesh>

      {/* Pants/shorts */}
      <mesh position={[0, 0.20, 0]} castShadow scale={[1, 0.6, 0.85]}>
        <capsuleGeometry args={[0.42, 0.30, 8, 16]} />
        <meshStandardMaterial color="#2a1a08" roughness={0.75} />
      </mesh>

      {/* Legs — short stubs */}
      <mesh position={[-0.22, -0.30, 0]} castShadow>
        <capsuleGeometry args={[0.16, 0.30, 8, 16]} />
        <meshStandardMaterial color={skin} roughness={0.55} />
      </mesh>
      <mesh position={[0.22, -0.30, 0]} castShadow>
        <capsuleGeometry args={[0.16, 0.30, 8, 16]} />
        <meshStandardMaterial color={skin} roughness={0.55} />
      </mesh>

      {/* Feet */}
      <mesh position={[-0.22, -0.62, 0.10]} castShadow scale={[1, 0.5, 1.6]}>
        <sphereGeometry args={[0.17, 16, 16]} />
        <meshStandardMaterial color="#1a0a04" roughness={0.6} />
      </mesh>
      <mesh position={[0.22, -0.62, 0.10]} castShadow scale={[1, 0.5, 1.6]}>
        <sphereGeometry args={[0.17, 16, 16]} />
        <meshStandardMaterial color="#1a0a04" roughness={0.6} />
      </mesh>
    </group>
  );
}

// ──────────────────────────────────────────────────────────────────
// Particles around the character — emotion-coloured
// ──────────────────────────────────────────────────────────────────
function Particles({ active, color }: { active: boolean; color: string }) {
  const count = active ? 40 : 14;
  const positions = useMemo(() =>
    Array.from({ length: count }, () => ({
      pos: [
        (Math.random() - 0.5) * 6,
        Math.random() * 4 - 1,
        (Math.random() - 0.5) * 3,
      ] as [number, number, number],
      speed: 0.2 + Math.random() * 0.8,
      size: 0.015 + Math.random() * 0.03,
    }))
  , [count]);

  const ref = useRef<THREE.Group>(null);
  useFrame((_, delta) => {
    if (!ref.current) return;
    ref.current.children.forEach((child, i) => {
      const p = positions[i];
      if (!p) return;
      child.position.y += p.speed * delta * 0.6;
      if (child.position.y > 4) child.position.y = -1.5;
    });
  });

  return (
    <group ref={ref}>
      {positions.map((p, i) => (
        <mesh key={i} position={p.pos}>
          <sphereGeometry args={[p.size, 6, 6]} />
          <meshBasicMaterial color={color} transparent opacity={0.45} />
        </mesh>
      ))}
    </group>
  );
}

// ──────────────────────────────────────────────────────────────────
// Scene root — combines all subsystems with lipsync rAF
// ──────────────────────────────────────────────────────────────────
function GoblinScene({
  emotionalState,
  presenceState,
  animationIntent,
  isSpeaking,
  mouthOpen,
  swayPhase,
}: {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
  isSpeaking: boolean;
  mouthOpen: number;
  swayPhase: number;
}) {
  const skin = useMemo(() => {
    const m: Record<string, string> = {
      calm: '#7ab84a',
      productive: '#82c052',
      tense: '#a89248',
      playful: '#86c450',
      tired: '#6a8848',
      supportive: '#74b245',
      celebratory: '#8ed058',
    };
    return new THREE.Color(m[emotionalState.mood] ?? '#7ab84a');
  }, [emotionalState.mood]);

  const stroke = useMemo(() => new THREE.Color('#2c4422'), []);
  const pose = useMemo(() => {
    if (isSpeaking) return POSES.speaking;
    return poseFor(animationIntent.animationState);
  }, [animationIntent.animationState, isSpeaking]);

  const isError = animationIntent.animationState === 'error_shock';

  return (
    <>
      <ambientLight intensity={0.35} />
      <directionalLight position={[3, 4, 4]} intensity={1.0} castShadow shadow-mapSize-width={1024} shadow-mapSize-height={1024} />
      <pointLight position={[-3, 1.5, 2]} intensity={0.55} color={isError ? '#ff5544' : '#10b981'} />
      <pointLight position={[3, -1, 2]} intensity={0.25} color="#ffcc99" />
      <hemisphereLight args={['#9bd25c', '#1a1a22', 0.25]} />

      <Float speed={1.2} rotationIntensity={0.06} floatIntensity={0.18}>
        <Body skin={skin} swayPhase={swayPhase} />
        <Head
          emotionalState={emotionalState}
          presenceState={presenceState}
          animationIntent={animationIntent}
          isSpeaking={isSpeaking}
          mouthOpen={mouthOpen}
          pose={pose}
        />
        <Arm side="L" skin={skin} stroke={stroke} target={pose.left}  sway={isSpeaking ? Math.sin(swayPhase) : 0} />
        <Arm side="R" skin={skin} stroke={stroke} target={pose.right} sway={isSpeaking ? -Math.sin(swayPhase) : 0} />
      </Float>

      <ContactShadows position={[0, -0.95, 0]} opacity={0.45} blur={2.2} far={3} resolution={512} />

      <Particles
        active={animationIntent.animationState !== 'idle_breathe'}
        color={isError ? '#ff5544' : '#10b981'}
      />

      <Environment preset="city" />
    </>
  );
}

// ──────────────────────────────────────────────────────────────────
// Public component
// ──────────────────────────────────────────────────────────────────
interface Goblin3DProps {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
  isSpeaking?: boolean;
}

export function Goblin3D({ emotionalState, presenceState, animationIntent, isSpeaking = false }: Goblin3DProps) {
  // Lipsync driver
  const [mouthOpen, setMouthOpen] = useState(0);
  const [swayPhase, setSwayPhase] = useState(0);
  const raf = useRef<number | null>(null);

  useEffect(() => {
    if (!isSpeaking) {
      setMouthOpen(0);
      if (raf.current) cancelAnimationFrame(raf.current);
      return;
    }
    let phase = 0;
    const tick = () => {
      phase += 0.16;
      setMouthOpen(Math.min(1,
        Math.abs(Math.sin(phase)) * 0.55 +
        Math.abs(Math.sin(phase * 1.9 + 1.2)) * 0.28 +
        Math.random() * 0.17
      ));
      setSwayPhase(phase);
      raf.current = requestAnimationFrame(tick);
    };
    raf.current = requestAnimationFrame(tick);
    return () => { if (raf.current) cancelAnimationFrame(raf.current); };
  }, [isSpeaking]);

  return (
    <div className="goblin-3d-container">
      <Canvas
        camera={{ position: [0, 1.5, 4.2], fov: 38 }}
        shadows
        style={{ background: 'transparent' }}
        gl={{ antialias: true, alpha: true }}
      >
        <GoblinScene
          emotionalState={emotionalState}
          presenceState={presenceState}
          animationIntent={animationIntent}
          isSpeaking={isSpeaking}
          mouthOpen={mouthOpen}
          swayPhase={swayPhase}
        />
      </Canvas>
    </div>
  );
}
