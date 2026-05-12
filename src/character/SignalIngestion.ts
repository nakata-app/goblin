// ═══════════════════════════════════════════════════
// SIGNAL INGESTION LAYER
// Captures realtime user and system behavior.
// Keyboard activity, mouse movement, idle detection,
// typing rhythm, build/error events.
//
// This is the first layer in the pipeline, converting
// raw browser events into semantic CharacterEvents.
// ═══════════════════════════════════════════════════

import { EventBus } from './EventBus';
import type { CharacterEvent } from './types';

export interface SignalConfig {
  /** Idle timeout in ms (default 30000 = 30s) */
  idleTimeout: number;
  /** Debounce mouse movement events (ms) */
  mouseDebounce: number;
  /** Fast typing threshold (chars per 2s) */
  fastTypingThreshold: number;
  /** Typing stopped delay (ms after last keystroke) */
  typingStoppedDelay: number;
}

const DEFAULT_CONFIG: SignalConfig = {
  idleTimeout: 30000,
  mouseDebounce: 500,
  fastTypingThreshold: 5,
  typingStoppedDelay: 2000,
};

export class SignalIngestion {
  private bus: EventBus;
  private config: SignalConfig;
  private isRunning = false;

  // Timers
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private typingTimer: ReturnType<typeof setTimeout> | null = null;
  private lastMouseMove = 0;
  private keystrokesInWindow = 0;
  private keystrokeWindowStart = 0;
  private isUserIdle = false;

  constructor(bus: EventBus, config?: Partial<SignalConfig>) {
    this.bus = bus;
    this.config = { ...DEFAULT_CONFIG, ...config };
  }

  /** Start listening to browser events. */
  start(): void {
    if (this.isRunning) return;
    this.isRunning = true;

    document.addEventListener('keydown', this.handleKeyDown);
    document.addEventListener('mousemove', this.handleMouseMove);
    document.addEventListener('click', this.handleClick);

    this.resetIdleTimer();
  }

  /** Stop listening. */
  stop(): void {
    this.isRunning = false;
    document.removeEventListener('keydown', this.handleKeyDown);
    document.removeEventListener('mousemove', this.handleMouseMove);
    document.removeEventListener('click', this.handleClick);

    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.typingTimer) clearTimeout(this.typingTimer);
  }

  /** Check if user is currently idle. */
  get isUserActive(): boolean {
    return !this.isUserIdle;
  }

  /** Time since last activity in ms. */
  get idleDuration(): number {
    return Date.now() - this.lastMouseMove;
  }

  // --- Handlers ---

  private handleKeyDown = (): void => {
    this.resetIdleTimer();
    const now = Date.now();

    // Track keystroke rate
    if (now - this.keystrokeWindowStart > 2000) {
      this.keystrokesInWindow = 0;
      this.keystrokeWindowStart = now;
    }
    this.keystrokesInWindow++;

    // Emit typing started
    if (this.keystrokesInWindow === 1) {
      this.emit('user.typing.started', 15);
    }

    // Fast typing detection
    if (this.keystrokesInWindow >= this.config.fastTypingThreshold) {
      this.emit('user.typing.fast', 25, { speed: this.keystrokesInWindow });
    }

    // Reset typing stopped timer
    if (this.typingTimer) clearTimeout(this.typingTimer);
    this.typingTimer = setTimeout(() => {
      this.emit('user.typing.stopped', 5);
    }, this.config.typingStoppedDelay);
  };

  private handleMouseMove = (): void => {
    const now = Date.now();
    if (now - this.lastMouseMove < this.config.mouseDebounce) return;
    this.lastMouseMove = now;
    this.resetIdleTimer();
    this.emit('user.mouse.moved', 2);
  };

  private handleClick = (): void => {
    this.resetIdleTimer();
  };

  private resetIdleTimer(): void {
    if (this.idleTimer) clearTimeout(this.idleTimer);

    if (this.isUserIdle) {
      this.isUserIdle = false;
      this.emit('user.idle.ended', 20);
    }

    this.idleTimer = setTimeout(() => {
      this.isUserIdle = true;
      this.emit('user.idle.started', 30);
    }, this.config.idleTimeout);
  }

  private emit(type: CharacterEvent['type'], priority: number, payload?: Record<string, unknown>): void {
    this.bus.emit({
      type,
      priority,
      payload,
      timestamp: Date.now(),
    });
  }
}
