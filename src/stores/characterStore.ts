// ═══════════════════════════════════════════════════
// CHARACTER STORE — Zustand store owning all character subsystems.
// This is the single source of truth for character state in React.
// ═══════════════════════════════════════════════════

import { create } from 'zustand';
import {
  EventBus,
  EmotionalEngine,
  BehaviorOrchestrator,
  PresenceSystem,
  AnimationDirector,
  CognitiveEngine,
  SignalIngestion,
} from '../character';
import type {
  AnimationIntent,
  CharacterEvent,
  CharacterEventType,
  EmotionalState,
  PresenceState,
} from '../character/types';
import type { CognitiveSnapshot } from '../character/CognitiveEngine';
import type { LLMTargets } from '../character/LLMInterpreter';

interface CharacterStoreState {
  // Engine instances
  eventBus: EventBus;
  emotionalEngine: EmotionalEngine;
  behaviorOrchestrator: BehaviorOrchestrator;
  presenceSystem: PresenceSystem;
  animationDirector: AnimationDirector;
  cognitiveEngine: CognitiveEngine;
  signalIngestion: SignalIngestion;

  // Reactive state snapshots
  emotionalState: EmotionalState;
  presenceState: PresenceState;
  animationIntent: AnimationIntent;
  cognitiveSnapshot: CognitiveSnapshot;

  // Actions
  emit: (type: CharacterEventType, payload?: Record<string, unknown>, priority?: number) => void;
  setAttention: (focus: 'user' | 'code' | 'terminal' | 'thinking', locked?: boolean) => void;
  releaseAttention: () => void;
  forceAnimationState: (state: string) => void;
  applyLLMOutput: (targets: LLMTargets) => void;
  start: () => void;
  stop: () => void;
  reset: () => void;
}

// Create singletons
const eventBus = new EventBus();
const emotionalEngine = new EmotionalEngine();
const behaviorOrchestrator = new BehaviorOrchestrator(emotionalEngine);
const presenceSystem = new PresenceSystem();
const animationDirector = new AnimationDirector();
const cognitiveEngine = new CognitiveEngine();
const signalIngestion = new SignalIngestion(eventBus);

export const useCharacterStore = create<CharacterStoreState>((set, get) => {
  // Connect engine tick → React state updates
  emotionalEngine.onChange((es) => {
    const ps = presenceSystem.state;
    presenceSystem.updateEmotionalState(es);
    const intent = animationDirector.getIntent(es, ps);

    set({
      emotionalState: es,
      presenceState: { ...ps },
      animationIntent: intent,
    });
  });

  presenceSystem.onChange((ps) => {
    const es = emotionalEngine.snapshot();
    const intent = animationDirector.getIntent(es, ps);

    set({
      emotionalState: es,
      presenceState: ps,
      animationIntent: intent,
    });
  });

  // Route events from event bus → behavior orchestrator + cognitive engine
  eventBus.onAll((event) => {
    behaviorOrchestrator.processEvent(event);
    cognitiveEngine.feed(event);

    // Apply cognitive insights to emotions
    const cs = cognitiveEngine.snapshot();
    const engine = emotionalEngine;
    engine.setTarget('focus' as never, cs.estimatedFocus);
    engine.setTarget('frustration' as never, cs.estimatedFrustration);
    if (cs.inCodingFlow) {
      engine.setTarget('energy' as never, 0.7);
    }
    if (cs.typingSpeed > 0) {
      engine.setTarget('engagement' as never, 0.7);
    }

    set({ cognitiveSnapshot: cs });
  });

  return {
    eventBus,
    emotionalEngine,
    behaviorOrchestrator,
    presenceSystem,
    animationDirector,
    cognitiveEngine,
    signalIngestion,

    emotionalState: emotionalEngine.snapshot(),
    presenceState: presenceSystem.state,
    animationIntent: animationDirector.getIntent(
      emotionalEngine.snapshot(),
      presenceSystem.state
    ),
    cognitiveSnapshot: cognitiveEngine.snapshot(),

    emit: (type, payload, priority) => {
      const event: CharacterEvent = {
        type,
        priority: priority ?? behaviorOrchestrator.getPriority(type),
        payload,
        timestamp: Date.now(),
      };
      eventBus.emit(event);
    },

    setAttention: (focus, locked = false) => {
      get().presenceSystem.setAttention(focus, locked);
    },

    releaseAttention: () => {
      get().presenceSystem.releaseAttention();
    },

    forceAnimationState: (state) => {
      get().animationDirector.forceState(state);
    },

    applyLLMOutput: (targets) => {
      get().behaviorOrchestrator.applyLLMOutput(targets);

      // Also sync animation director with LLM posture/eye focus
      if (targets.confidence > 0.5) {
        const postureStateMap: Record<string, string> = {
          lean_forward: 'attentive_watch',
          lean_back: 'idle_breathe',
          upright: 'attentive_watch',
          tilt_left: 'curious_tilt',
          tilt_right: 'curious_tilt',
        };
        get().animationDirector.forceState(
          postureStateMap[targets.posture] ?? 'attentive_watch'
        );
        get().presenceSystem.setAttention(
          targets.eyeFocus as 'user' | 'code' | 'terminal' | 'thinking'
        );
      }
    },

    start: () => {
      emotionalEngine.start();
      presenceSystem.start();
      // signalIngestion.start(); // Disabled: causes crashes during typing
    },

    stop: () => {
      emotionalEngine.stop();
      presenceSystem.stop();
      // signalIngestion.stop();
    },

    reset: () => {
      eventBus.clear();
      emotionalEngine.reset();
      behaviorOrchestrator.reset();
      presenceSystem.reset();
      animationDirector.reset();
      cognitiveEngine.reset();
    },
  };
});
