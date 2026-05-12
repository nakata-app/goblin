import { useRef, useMemo } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { Float } from '@react-three/drei';
import * as THREE from 'three';
import type { EmotionalState, PresenceState, AnimationIntent } from '../character/types';

// ── Procedural Goblin Head ──
function GoblinHead({
  emotionalState,
  presenceState,
  animationIntent,
}: {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
}) {
  const headRef = useRef<THREE.Group>(null);
  const leftEyeRef = useRef<THREE.Mesh>(null);
  const rightEyeRef = useRef<THREE.Mesh>(null);
  const mouthRef = useRef<THREE.Mesh>(null);

  const { vector } = emotionalState;
  const { blinkProgress, eyeGazeX, eyeGazeY, breathePhase } = presenceState;

  const skinColor = useMemo(() => {
    const frust = vector.frustration;
    if (frust > 0.6) return new THREE.Color('#c0392b');
    if (vector.satisfaction > 0.6) return new THREE.Color('#27ae60');
    return new THREE.Color('#4a9a3c');
  }, [vector.frustration, vector.satisfaction]);

  const eyeColor = useMemo(() => {
    if (vector.frustration > 0.5) return '#ff4444';
    if (vector.curiosity > 0.7) return '#44ccff';
    return '#ffdd44';
  }, [vector.frustration, vector.curiosity]);

  const earColor = skinColor.clone().multiplyScalar(0.8);
  const earInner = new THREE.Color('#8B4513');

  useFrame((_, delta) => {
    if (!headRef.current) return;

    // Breathing scale
    const breatheScale = 1 + Math.sin(breathePhase) * 0.02;
    headRef.current.scale.setScalar(breatheScale);

    // Head tilt based on state
    const state = animationIntent.animationState;
    if (state === 'curious_tilt') {
      headRef.current.rotation.z = Math.sin(Date.now() * 0.002) * 0.15;
    } else if (state === 'thinking_deep') {
      headRef.current.rotation.z = Math.sin(Date.now() * 0.001) * 0.05;
    } else if (state === 'frustrated_tense') {
      headRef.current.rotation.z = -0.1;
    } else {
      headRef.current.rotation.z += (0 - headRef.current.rotation.z) * delta * 3;
    }

    // Eye blink
    if (leftEyeRef.current) {
      const eyeScale = 1 - (blinkProgress ?? 0) * 0.9;
      leftEyeRef.current.scale.y = eyeScale;
      rightEyeRef.current?.scale.set(1, eyeScale, 1);
    }
  });

  return (
    <group ref={headRef}>
      {/* Head sphere */}
      <mesh position={[0, 0, 0]} castShadow>
        <sphereGeometry args={[1, 32, 32]} />
        <meshStandardMaterial color={skinColor} roughness={0.6} metalness={0.05} />
      </mesh>

      {/* Left ear */}
      <group position={[-1.05, 0.2, 0]} rotation={[0, 0, 0.3]}>
        <mesh castShadow>
          <coneGeometry args={[0.35, 0.9, 16]} />
          <meshStandardMaterial color={earColor} roughness={0.6} />
        </mesh>
        <mesh position={[0, -0.05, 0.02]} scale={[0.4, 0.5, 0.4]}>
          <sphereGeometry args={[0.7, 8, 8]} />
          <meshStandardMaterial color={earInner} roughness={0.5} />
        </mesh>
      </group>

      {/* Right ear */}
      <group position={[1.05, 0.2, 0]} rotation={[0, 0, -0.3]}>
        <mesh castShadow>
          <coneGeometry args={[0.35, 0.9, 16]} />
          <meshStandardMaterial color={earColor} roughness={0.6} />
        </mesh>
        <mesh position={[0, -0.05, 0.02]} scale={[0.4, 0.5, 0.4]}>
          <sphereGeometry args={[0.7, 8, 8]} />
          <meshStandardMaterial color={earInner} roughness={0.5} />
        </mesh>
      </group>

      {/* Eyes */}
      <group position={[0, 0.15, 0.92]}>
        {/* Left eye socket */}
        <mesh position={[-0.35, 0, -0.05]}>
          <sphereGeometry args={[0.22, 16, 16]} />
          <meshStandardMaterial color="#1a1a1a" roughness={0.3} />
        </mesh>
        {/* Left eye glow */}
        <mesh ref={leftEyeRef} position={[-0.35 + (eyeGazeX ?? 0) * 0.05, (eyeGazeY ?? 0) * 0.03, 0.08]}>
          <sphereGeometry args={[0.15, 16, 16]} />
          <meshStandardMaterial color={eyeColor} roughness={0.2} emissive={eyeColor} emissiveIntensity={0.6} />
        </mesh>

        {/* Right eye socket */}
        <mesh position={[0.35, 0, -0.05]}>
          <sphereGeometry args={[0.22, 16, 16]} />
          <meshStandardMaterial color="#1a1a1a" roughness={0.3} />
        </mesh>
        {/* Right eye glow */}
        <mesh ref={rightEyeRef} position={[0.35 + (eyeGazeX ?? 0) * 0.05, (eyeGazeY ?? 0) * 0.03, 0.08]}>
          <sphereGeometry args={[0.15, 16, 16]} />
          <meshStandardMaterial color={eyeColor} roughness={0.2} emissive={eyeColor} emissiveIntensity={0.6} />
        </mesh>
      </group>

      {/* Nose */}
      <mesh position={[0, -0.15, 0.95]} rotation={[0.3, 0, 0]}>
        <coneGeometry args={[0.09, 0.3, 8]} />
        <meshStandardMaterial color={skinColor.clone().multiplyScalar(0.7)} roughness={0.5} />
      </mesh>

      {/* Mouth */}
      <mesh ref={mouthRef} position={[0, -0.35, 0.9]}>
        <torusGeometry args={[0.22, 0.04, 8, 16, Math.PI]} />
        <meshStandardMaterial color="#2a1a1a" roughness={0.4} />
      </mesh>

      {/* Eyebrows */}
      <mesh position={[-0.35, 0.35, 0.9]} rotation={[0, 0, 0.1]}>
        <boxGeometry args={[0.3, 0.05, 0.03]} />
        <meshStandardMaterial color="#1a1a1a" roughness={0.5} />
      </mesh>
      <mesh position={[0.35, 0.35, 0.9]} rotation={[0, 0, -0.1]}>
        <boxGeometry args={[0.3, 0.05, 0.03]} />
        <meshStandardMaterial color="#1a1a1a" roughness={0.5} />
      </mesh>
    </group>
  );
}

// ── Environment Particles ──
function Particles({ active }: { active: boolean }) {
  const count = active ? 30 : 10;
  const positions = useMemo(() =>
    Array.from({ length: count }, () => ({
      pos: [ (Math.random() - 0.5) * 8, (Math.random() - 0.5) * 8, (Math.random() - 0.5) * 4 ] as [number, number, number],
      speed: 0.3 + Math.random() * 0.7,
      size: 0.02 + Math.random() * 0.04,
    }))
  , [count]);

  const ref = useRef<THREE.Group>(null);

  useFrame((_, delta) => {
    if (!ref.current) return;
    ref.current.children.forEach((child, i) => {
      child.position.y += positions[i].speed * delta;
      if (child.position.y > 4) child.position.y = -4;
    });
  });

  return (
    <group ref={ref}>
      {positions.map((p, i) => (
        <mesh key={i} position={p.pos}>
          <sphereGeometry args={[p.size, 4, 4]} />
          <meshBasicMaterial color="#10b981" transparent opacity={0.3} />
        </mesh>
      ))}
    </group>
  );
}

// ── Main Goblin3D Component ──
interface Goblin3DProps {
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
}

export function Goblin3D({ emotionalState, presenceState, animationIntent }: Goblin3DProps) {
  const isActive = animationIntent.animationState !== 'idle_breathe';

  return (
    <div className="goblin-3d-container">
      <Canvas
        camera={{ position: [0, 0.3, 5.5], fov: 35 }}
        style={{ background: 'transparent' }}
        gl={{ antialias: true, alpha: true }}
      >
        <ambientLight intensity={0.4} />
        <directionalLight position={[5, 5, 5]} intensity={0.8} castShadow />
        <pointLight position={[-3, 2, 3]} intensity={0.4} color="#10b981" />
        <pointLight position={[3, -1, 2]} intensity={0.2} color="#ff6644" />

        <Float speed={1.5} rotationIntensity={0.1} floatIntensity={0.3}>
          <GoblinHead
            emotionalState={emotionalState}
            presenceState={presenceState}
            animationIntent={animationIntent}
          />
        </Float>

        <Particles active={isActive} />
      </Canvas>

      <div className="goblin-3d-label">
        {animationIntent.animationState === 'idle_breathe' ? 'Ready'
          : animationIntent.animationState === 'thinking_deep' ? 'Thinking'
          : animationIntent.animationState === 'reading_scan' ? 'Reading'
          : animationIntent.animationState === 'writing_focused' ? 'Writing'
          : animationIntent.animationState === 'running_active' ? 'Running'
          : animationIntent.animationState === 'error_shock' ? 'Error!'
          : animationIntent.animationState === 'success_celebrate' ? 'Done!'
          : 'Ready'}
      </div>
    </div>
  );
}
