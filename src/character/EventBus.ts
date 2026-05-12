// ═══════════════════════════════════════════════════
// EVENT BUS — Lightweight typed pub-sub
// ═══════════════════════════════════════════════════

import type { CharacterEvent, CharacterEventType } from './types';

type Listener = (event: CharacterEvent) => void;

export class EventBus {
  private listeners = new Map<CharacterEventType, Set<Listener>>();
  private history: CharacterEvent[] = [];
  private maxHistory = 500;

  /** Subscribe to a specific event type. Returns unsubscribe function. */
  on(type: CharacterEventType, fn: Listener): () => void {
    if (!this.listeners.has(type)) {
      this.listeners.set(type, new Set());
    }
    this.listeners.get(type)!.add(fn);
    return () => this.listeners.get(type)?.delete(fn);
  }

  /** Subscribe to all event types (wildcard). */
  onAll(fn: Listener): () => void {
    return this.on('*' as CharacterEventType, fn);
  }

  /** Emit an event to all subscribers. Also stored in history. */
  emit(event: CharacterEvent): void {
    this.history.push(event);
    if (this.history.length > this.maxHistory) {
      this.history = this.history.slice(-this.maxHistory);
    }

    const specific = this.listeners.get(event.type);
    if (specific) {
      for (const fn of specific) fn(event);
    }

    const wildcard = this.listeners.get('*' as CharacterEventType);
    if (wildcard) {
      for (const fn of wildcard) fn(event);
    }
  }

  /** Get recent events of a specific type. */
  recentEvents(type?: CharacterEventType, count = 20): CharacterEvent[] {
    const filtered = type
      ? this.history.filter((e) => e.type === type)
      : this.history;
    return filtered.slice(-count);
  }

  /** Clear history. */
  clear(): void {
    this.history = [];
  }

  /** Remove all listeners. */
  reset(): void {
    this.listeners.clear();
    this.history = [];
  }
}
