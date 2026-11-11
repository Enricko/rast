// Server-sent events with sequencing, gap detection, heartbeats and resilient
// reconnection.
//
// There is exactly one authenticated EventSource per browser tab. Applications
// subscribe through `core/realtime.ts`; this module owns the transport:
//   * server sequence numbers as a per-connection cursor: every `hello` starts
//     a new cursor (a restarted server counts from 1 again), older frames on
//     the same connection are dropped, and missed events are signalled by the
//     server's `resync` frame. Ids are global and skip events this account
//     may not see, so an id jump is *not* a gap;
//   * ping heartbeats and stale detection (force reconnect),
//   * exponential backoff with jitter,
//   * online/offline + visibility/wake handling,
//   * lifecycle: idle disconnect when nothing listens, resume on demand.

export interface RastEvent<T = unknown> {
  kind: string;
  ts: number;
  seq: number;
  data: T;
}

export type ConnectionState = "connected" | "connecting" | "reconnecting" | "offline";

type Listener = (event: RastEvent) => void;
type StatusListener = (state: ConnectionState) => void;
type LifecycleListener = (reason: string) => void;

const KNOWN_EVENTS = [
  "hello",
  "ping",
  "resync",
  "monitor",
  "notice",
  "session.revoked",
  "job.queued",
  "job.start",
  "job.progress",
  "job.done",
  "job.failed",
  "job.cancelled",
  "backup.done",
  "backup.changed",
  "app.status",
  "app.changed",
  "app.deploy",
  "app.logs",
  "docker.changed",
  "file.changed",
  "file.quota",
  "files.resync",
  "audit.created",
  "user.changed",
  "settings.changed",
  "appearance.changed",
  "group.changed",
  "access.changed",
  "package.changed",
  "package.catalog",
];

const IDLE_DISCONNECT_MS = 30_000;
const HIDDEN_DISCONNECT_MS = 45_000;
const STALE_AFTER_MS = 45_000;
const STALE_CHECK_MS = 10_000;
const BACKOFF_BASE_MS = 1_000;
const BACKOFF_MAX_MS = 30_000;

export class EventStream {
  private source: EventSource | null = null;
  private readonly listeners = new Map<string, Set<Listener>>();
  private readonly statusListeners = new Set<StatusListener>();
  private readonly lifecycleListeners = new Set<LifecycleListener>();
  private idleTimer: number | undefined;
  private hiddenTimer: number | undefined;
  private reconnectTimer: number | undefined;
  private staleTimer: number | undefined;
  private visibilityHandler: (() => void) | null = null;
  private onlineHandler: (() => void) | null = null;
  private offlineHandler: (() => void) | null = null;
  private lastEventAt = 0;
  private seq = 0;
  private attempt = 0;
  private everConnected = false;
  private state: ConnectionState = "offline";
  private disposed = false;
  private manualStop = false;

  on(kind: string, listener: Listener): () => void {
    let set = this.listeners.get(kind);
    if (!set) {
      set = new Set();
      this.listeners.set(kind, set);
    }
    set.add(listener);
    this.ensureConnected();
    return () => {
      const current = this.listeners.get(kind);
      if (!current) return;
      current.delete(listener);
      if (current.size === 0) this.listeners.delete(kind);
      if (this.listenerCount() === 0) this.scheduleIdleDisconnect();
    };
  }

  /** Subscribe to monitor samples; convenience wrapper. */
  onMonitor(listener: (event: RastEvent) => void): () => void {
    return this.on("monitor", listener);
  }

  /** Observe connection state (used by the shell system tray). */
  onStatus(listener: StatusListener): () => void {
    this.statusListeners.add(listener);
    listener(this.state);
    return () => this.statusListeners.delete(listener);
  }

  /**
   * Called after a reconnection or detected sequence gap: subscribers should
   * fetch a fresh authorized snapshot instead of trusting missed events.
   */
  onResync(listener: LifecycleListener): () => void {
    this.lifecycleListeners.add(listener);
    return () => this.lifecycleListeners.delete(listener);
  }

  get status(): ConnectionState {
    return this.state;
  }

  get isConnected(): boolean {
    return this.state === "connected";
  }

  get lastEventTimestamp(): number {
    return this.lastEventAt;
  }

  get lastSequence(): number {
    return this.seq;
  }

  get isStale(): boolean {
    return this.state === "connected" && this.lastEventAt > 0 && Date.now() - this.lastEventAt > STALE_AFTER_MS;
  }

  start(): void {
    if (this.disposed) return;
    this.manualStop = false;
    if (!this.visibilityHandler) {
      this.visibilityHandler = () => this.handleVisibility();
      document.addEventListener("visibilitychange", this.visibilityHandler);
    }
    if (!this.onlineHandler && typeof window !== "undefined") {
      this.onlineHandler = () => this.handleWake("browser online");
      this.offlineHandler = () => this.setStatus("offline");
      window.addEventListener("online", this.onlineHandler);
      window.addEventListener("offline", this.offlineHandler);
    }
    if (!this.staleTimer) {
      this.staleTimer = window.setInterval(() => this.checkStale(), STALE_CHECK_MS);
    }
    this.ensureConnected();
  }

  stop(): void {
    this.manualStop = true;
    this.disconnect();
    if (this.visibilityHandler) {
      document.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = null;
    }
    if (this.onlineHandler) {
      window.removeEventListener("online", this.onlineHandler);
      this.onlineHandler = null;
    }
    if (this.offlineHandler) {
      window.removeEventListener("offline", this.offlineHandler);
      this.offlineHandler = null;
    }
    if (this.staleTimer) {
      window.clearInterval(this.staleTimer);
      this.staleTimer = undefined;
    }
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
    this.listeners.clear();
    this.statusListeners.clear();
    this.lifecycleListeners.clear();
  }

  /** Force an immediate reconnect (used by the status popover "Reconnect"). */
  reconnectNow(): void {
    if (this.disposed) return;
    this.disconnect();
    this.attempt = 0;
    this.ensureConnected();
  }

  private listenerCount(): number {
    let count = 0;
    for (const set of this.listeners.values()) count += set.size;
    return count;
  }

  private ensureConnected(): void {
    if (this.disposed || this.manualStop) return;
    if (this.idleTimer !== undefined) {
      window.clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }
    if (document.visibilityState === "hidden") {
      // Wait until the tab is visible again; do not consume resources.
      return;
    }
    if (this.source) return;
    if (typeof EventSource === "undefined") {
      this.setStatus("offline");
      return;
    }

    this.setStatus(this.everConnected ? "reconnecting" : "connecting");
    let source: EventSource;
    try {
      // Same-origin cookie auth: credentials never appear in the URL.
      source = new EventSource("/api/events", { withCredentials: true });
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.source = source;
    // The new connection's `hello` sets the cursor; until then accept any id.
    this.seq = 0;
    let opened = false;

    source.addEventListener("open", () => {
      opened = true;
      this.attempt = 0;
      this.setStatus("connected");
      this.lastEventAt = Date.now();
      if (this.everConnected) {
        this.emitResync("reconnected");
      }
      this.everConnected = true;
    });

    source.addEventListener("error", () => {
      if (this.source !== source) return;
      // EventSource retries on its own; take control so backoff is bounded.
      this.disconnect();
      this.setStatus("offline");
      this.scheduleReconnect();
    });

    const dispatch = (kind: string) => (event: MessageEvent<string>) => {
      if (this.source !== source) return; // a superseded connection
      this.lastEventAt = Date.now();
      let data: unknown = null;
      try {
        data = JSON.parse(event.data);
      } catch {
        data = event.data;
      }
      const seq = acceptSequence(this.seq, kind, event.lastEventId, data);
      if (seq === null) return; // older than the cursor of this connection
      this.seq = seq;
      if (kind === "resync") {
        this.emitResync("server");
      }
      const rastEvent: RastEvent = { kind, ts: Date.now(), seq, data };
      this.emit(rastEvent);
    };

    for (const kind of KNOWN_EVENTS) {
      source.addEventListener(kind, dispatch(kind) as EventListener);
    }
    source.onmessage = dispatch("message");

    // If the connection never opens (proxy/cookie problem), back off instead of
    // silently looping forever.
    window.setTimeout(() => {
      if (!opened && this.source === source) {
        this.disconnect();
        this.setStatus("offline");
        this.scheduleReconnect();
      }
    }, 8_000);
  }

  private scheduleReconnect(): void {
    if (this.disposed || this.manualStop) return;
    if (this.reconnectTimer !== undefined) return;
    const exp = Math.min(BACKOFF_MAX_MS, BACKOFF_BASE_MS * 2 ** Math.min(this.attempt, 5));
    const jitter = Math.floor(exp * 0.25 * Math.random());
    const delay = exp + jitter;
    this.attempt += 1;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = undefined;
      if (this.listenerCount() === 0) return;
      this.ensureConnected();
    }, delay);
  }

  private emit(event: RastEvent): void {
    const sets = [this.listeners.get(event.kind), this.listeners.get("*")];
    for (const set of sets) {
      if (!set) continue;
      for (const listener of set) {
        try {
          listener(event);
        } catch (error) {
          console.error("event listener failed", error);
        }
      }
    }
  }

  private emitResync(reason: string): void {
    for (const listener of this.lifecycleListeners) {
      try {
        listener(reason);
      } catch (error) {
        console.error("resync listener failed", error);
      }
    }
  }

  private setStatus(state: ConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    for (const listener of this.statusListeners) {
      try {
        listener(state);
      } catch (error) {
        console.error("status listener failed", error);
      }
    }
  }

  private checkStale(): void {
    if (this.state !== "connected") return;
    if (this.lastEventAt > 0 && Date.now() - this.lastEventAt > STALE_AFTER_MS) {
      // The proxy may have silently dropped the stream (heartbeats missing).
      this.disconnect();
      this.setStatus("offline");
      this.emitResync("stale");
      this.ensureConnected();
    }
  }

  private handleWake(reason: string): void {
    if (this.disposed) return;
    this.attempt = 0;
    if (this.listenerCount() === 0) return;
    if (this.state !== "connected") {
      this.disconnect();
      this.ensureConnected();
      this.emitResync(reason);
    }
  }

  private scheduleIdleDisconnect(): void {
    if (this.idleTimer !== undefined) window.clearTimeout(this.idleTimer);
    this.idleTimer = window.setTimeout(() => {
      this.idleTimer = undefined;
      if (this.listenerCount() === 0) this.disconnect();
    }, IDLE_DISCONNECT_MS);
  }

  private handleVisibility(): void {
    if (document.visibilityState === "hidden") {
      if (this.hiddenTimer !== undefined) window.clearTimeout(this.hiddenTimer);
      this.hiddenTimer = window.setTimeout(() => {
        this.hiddenTimer = undefined;
        if (this.listenerCount() === 0) this.disconnect();
      }, HIDDEN_DISCONNECT_MS);
      return;
    }
    if (this.hiddenTimer !== undefined) {
      window.clearTimeout(this.hiddenTimer);
      this.hiddenTimer = undefined;
    }
    if (this.listenerCount() > 0) {
      this.handleWake("tab visible");
    }
  }

  private disconnect(): void {
    if (this.source) {
      this.source.close();
      this.source = null;
    }
    if (this.reconnectTimer !== undefined) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = undefined;
    }
    if (this.state === "connected") this.setStatus("offline");
  }
}

/**
 * Sequence bookkeeping for one connection, kept pure for tests. Returns the
 * new cursor, or `null` when the frame is older than the cursor and must be
 * dropped. `hello` always starts a fresh cursor (its `seq`): after a server
 * restart ids begin at 1 again, and a cursor carried over from the previous
 * process would silently discard every new event. Frames without an id
 * (EventSource repeats the last id) keep the cursor.
 */
export function acceptSequence(cursor: number, kind: string, rawId: string, data: unknown): number | null {
  if (kind === "hello") {
    const fromData = (data as { seq?: unknown } | null)?.seq;
    const helloSeq = typeof fromData === "number" ? fromData : Number.parseInt(rawId, 10);
    return Number.isFinite(helloSeq) && helloSeq >= 0 ? helloSeq : 0;
  }
  const seq = rawId ? Number.parseInt(rawId, 10) : Number.NaN;
  if (!Number.isFinite(seq) || seq <= 0) return cursor;
  // Same id as the cursor: the initial monitor snapshot shares the hello id
  // and pings repeat the stream position; neither is a duplicate event.
  if (seq < cursor) return null;
  return seq;
}
