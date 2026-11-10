// Shared realtime layer.
//
// One EventStream per tab feeds every window through this store. Applications
// subscribe to event kinds or to cache keys, and invalidate the affected data
// instead of rebuilding the whole view. Events carry a monotonic sequence, so
// duplicates are ignored and gaps trigger an authorized snapshot refresh.
//
// The store also owns a small key/value cache: windows showing the same data
// (e.g. two File Station windows on one directory) share one fetch and one
// invalidation.

import type { ConnectionState, EventStream, RastEvent } from "./sse";

export type EventHandler<T = unknown> = (data: T, event: RastEvent<T>) => void;

interface CacheEntry<T = unknown> {
  value?: T;
  loadedAt: number;
  inflight?: Promise<T>;
}

/** Minimal key/value cache with scoped invalidation and subscribers. */
export class QueryCache {
  private readonly entries = new Map<string, CacheEntry>();
  private readonly pending = new Map<string, Promise<unknown>>();
  private readonly keys = new Set<string>();
  private readonly listeners = new Map<string, Set<() => void>>();

  /** Read a cached value without fetching. */
  peek<T>(key: string): T | undefined {
    return this.entries.get(key)?.value as T | undefined;
  }

  /** Age of the cached value in ms (Infinity when absent). */
  age(key: string): number {
    const entry = this.entries.get(key);
    if (!entry || entry.value === undefined) return Number.POSITIVE_INFINITY;
    return Date.now() - entry.loadedAt;
  }

  set<T>(key: string, value: T): void {
    this.entries.set(key, { value, loadedAt: Date.now() });
    this.notify(key);
  }

  /** Invalidate one key or every key with the given prefix. */
  invalidate(keyOrPrefix: string, options: { prefix?: boolean } = {}): void {
    const matches: string[] = [];
    if (options.prefix) {
      for (const key of this.entries.keys()) {
        if (key.startsWith(keyOrPrefix)) matches.push(key);
      }
    } else if (this.entries.has(keyOrPrefix)) {
      matches.push(keyOrPrefix);
    }
    for (const key of matches) this.entries.delete(key);
    for (const key of matches) this.notify(key);
    if (matches.length === 0 && options.prefix) {
      // Still notify listeners so mounted views can refetch even if nothing was
      // cached under that prefix yet.
      for (const [key, set] of this.listeners) {
        if (key.startsWith(keyOrPrefix)) {
          for (const listener of set) {
            try {
              listener();
            } catch (error) {
              console.error("cache listener failed", error);
            }
          }
        }
      }
    }
  }

  invalidateAll(): void {
    const keys = Array.from(this.entries.keys());
    this.entries.clear();
    for (const key of keys) this.notify(key);
  }

  /**
   * Subscribe to changes for a set of keys/prefixes. The listener is called when
   * the data may have changed; it should refetch (or read `peek`).
   */
  watch(keys: string[], listener: () => void, options: { prefix?: boolean } = {}): () => void {
    const registered: string[] = [];
    for (const key of keys) {
      const token = options.prefix ? `${key}*` : key;
      registered.push(token);
      const id = `${options.prefix ? "p" : "k"}:${key}`;
      this.keys.add(id);
      let set = this.listeners.get(id);
      if (!set) {
        set = new Set();
        this.listeners.set(id, set);
      }
      set.add(listener);
    }
    return () => {
      for (const token of registered) {
        const id = token.endsWith("*") ? `p:${token.slice(0, -1)}` : `k:${token}`;
        const set = this.listeners.get(id);
        if (!set) continue;
        set.delete(listener);
        if (set.size === 0) this.listeners.delete(id);
        this.keys.delete(id);
      }
    };
  }

  /**
   * Load through the cache. Concurrent callers share one request; a freshness
   * window lets callers decide when a cached value is good enough.
   */
  async load<T>(
    key: string,
    fetcher: () => Promise<T>,
    options: { maxAgeMs?: number; force?: boolean } = {},
  ): Promise<T> {
    const inflight = this.pending.get(key);
    if (!options.force && inflight) return inflight as Promise<T>;

    const existing = this.entries.get(key);
    const maxAge = options.maxAgeMs ?? 0;
    if (!options.force && existing?.value !== undefined && Date.now() - existing.loadedAt <= maxAge) {
      return existing.value as T;
    }

    const task = (async (): Promise<T> => {
      try {
        const value = await fetcher();
        this.entries.set(key, { value, loadedAt: Date.now() });
        this.notify(key);
        return value;
      } finally {
        this.pending.delete(key);
      }
    })();
    this.pending.set(key, task);
    return task;
  }

  private notify(key: string): void {
    for (const listener of this.listeners.get(`k:${key}`) ?? []) {
      try {
        listener();
      } catch (error) {
        console.error("cache listener failed", error);
      }
    }
    for (const [id, set] of this.listeners) {
      if (id.startsWith("p:") && key.startsWith(id.slice(2))) {
        for (const listener of set) {
          try {
            listener();
          } catch (error) {
            console.error("cache listener failed", error);
          }
        }
      }
    }
  }
}

/**
 * Event-driven store shared by every window. Applications either subscribe to
 * event kinds directly or invalidate/watch cache keys.
 */
export class RealtimeStore {
  readonly cache = new QueryCache();
  private readonly unsubscribers: Array<() => void> = [];
  private readonly listeners = new Map<string, Set<EventHandler>>();
  private readonly resyncListeners = new Set<(reason: string) => void>();

  constructor(private readonly events: EventStream) {}

  on<T = unknown>(kind: string, handler: EventHandler<T>): () => void {
    let set = this.listeners.get(kind);
    if (!set) {
      set = new Set();
      this.listeners.set(kind, set);
      const unsubscribe = this.events.on(kind, (event) => {
        for (const listener of set ?? []) {
          try {
            listener(event.data as T, event as RastEvent<T>);
          } catch (error) {
            console.error(`realtime listener for ${kind} failed`, error);
          }
        }
      });
      this.unsubscribers.push(unsubscribe);
    }
    set.add(handler as EventHandler);
    return () => {
      const current = this.listeners.get(kind);
      if (!current) return;
      current.delete(handler as EventHandler);
      if (current.size === 0) this.listeners.delete(kind);
    };
  }

  /** Called after a reconnect, gap or browser wake; refetch snapshots here. */
  onResync(listener: (reason: string) => void): () => void {
    this.resyncListeners.add(listener);
    return () => this.resyncListeners.delete(listener);
  }

  onStatus(listener: (state: ConnectionState) => void): () => void {
    return this.events.onStatus(listener);
  }

  get status(): ConnectionState {
    return this.events.status;
  }

  get lastUpdate(): number {
    return this.events.lastEventTimestamp;
  }

  get lastSequence(): number {
    return this.events.lastSequence;
  }

  invalidate(keys: string | string[], options: { prefix?: boolean } = {}): void {
    if (Array.isArray(keys)) {
      for (const key of keys) this.cache.invalidate(key, options);
    } else {
      this.cache.invalidate(keys, options);
    }
  }

  watch(keys: string[], listener: () => void, options: { prefix?: boolean } = {}): () => void {
    return this.cache.watch(keys, listener, options);
  }

  dispose(): void {
    for (const unsubscribe of this.unsubscribers.splice(0)) {
      try {
        unsubscribe();
      } catch {
        /* ignore */
      }
    }
    this.listeners.clear();
    this.resyncListeners.clear();
    this.cache.invalidateAll();
  }

  /** Used by the shell when the EventStream is stopped (sign-out). */
  notifyResync(reason: string): void {
    for (const listener of this.resyncListeners) {
      try {
        listener(reason);
      } catch (error) {
        console.error("resync listener failed", error);
      }
    }
  }
}

const STORE_MAP = new WeakMap<EventStream, RealtimeStore>();

/** Shared realtime store for this tab's event stream. */
export function getRealtime(events: EventStream): RealtimeStore {
  let store = STORE_MAP.get(events);
  if (!store) {
    store = new RealtimeStore(events);
    STORE_MAP.set(events, store);
    events.onResync((reason) => store!.notifyResync(reason));
  }
  return store;
}

/** Debounce helper used by views that react to bursts of file/system events. */
export function debounceTask(fn: () => void, waitMs = 250): { run: () => void; cancel: () => void } {
  let timer: number | undefined;
  return {
    run: () => {
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = undefined;
        fn();
      }, waitMs);
    },
    cancel: () => {
      if (timer !== undefined) window.clearTimeout(timer);
      timer = undefined;
    },
  };
}
