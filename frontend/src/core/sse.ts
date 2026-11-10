// Server-sent events with lifecycle management.
//
// One EventSource is shared by every app window. The connection is closed when
// no app is listening, when the tab is hidden for a while, and re-opened on
// demand — this keeps subscription cost bounded regardless of tab/window count.

export interface RastEvent<T = unknown> {
  kind: string;
  ts: number;
  data: T;
}

export type ConnectionState = "connected" | "connecting" | "disconnected";

type Listener = (event: RastEvent) => void;
type StatusListener = (state: ConnectionState) => void;

const KNOWN_EVENTS = [
  "monitor",
  "notice",
  "job.queued",
  "job.start",
  "job.progress",
  "job.done",
  "job.failed",
  "job.cancelled",
  "backup.done",
  "app.status",
];

const IDLE_DISCONNECT_MS = 30_000;
const HIDDEN_DISCONNECT_MS = 45_000;

export class EventStream {
  private source: EventSource | null = null;
  private readonly listeners = new Map<string, Set<Listener>>();
  private readonly statusListeners = new Set<StatusListener>();
  private idleTimer: number | undefined;
  private hiddenTimer: number | undefined;
  private visibilityHandler: (() => void) | null = null;
  private lastEventAt = 0;
  private state: ConnectionState = "disconnected";
  private disposed = false;

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

  get status(): ConnectionState {
    return this.state;
  }

  start(): void {
    if (this.disposed) return;
    if (!this.visibilityHandler) {
      this.visibilityHandler = () => this.handleVisibility();
      document.addEventListener("visibilitychange", this.visibilityHandler);
    }
    this.ensureConnected();
  }

  stop(): void {
    this.disconnect();
    if (this.visibilityHandler) {
      document.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = null;
    }
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
    this.listeners.clear();
    this.statusListeners.clear();
  }

  isConnected(): boolean {
    return this.state === "connected";
  }

  get lastEventTimestamp(): number {
    return this.lastEventAt;
  }

  private listenerCount(): number {
    let count = 0;
    for (const set of this.listeners.values()) count += set.size;
    return count;
  }

  private setState(state: ConnectionState): void {
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

  private ensureConnected(): void {
    if (this.disposed) return;
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
      this.setState("disconnected");
      return;
    }

    this.setState("connecting");
    const source = new EventSource("/api/events", { withCredentials: true });
    this.source = source;

    source.addEventListener("open", () => {
      this.setState("connected");
    });
    source.addEventListener("error", () => {
      this.setState("disconnected");
      // EventSource reconnects automatically; close on fatal auth errors.
      if (source.readyState === EventSource.CLOSED) {
        this.source = null;
        this.setState("connecting");
        window.setTimeout(() => this.ensureConnected(), 5000);
      }
    });
    const dispatch = (kind: string) => (event: MessageEvent<string>) => {
      this.lastEventAt = Date.now();
      let data: unknown = null;
      try {
        data = JSON.parse(event.data);
      } catch {
        data = event.data;
      }
      const rastEvent: RastEvent = { kind, ts: Date.now(), data };
      this.emit(rastEvent);
    };
    for (const kind of KNOWN_EVENTS) {
      source.addEventListener(kind, dispatch(kind) as EventListener);
    }
    // Fallback for unnamed events.
    source.onmessage = dispatch("message");
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
        if (this.listenerCount() === 0) {
          this.disconnect();
        }
      }, HIDDEN_DISCONNECT_MS);
      return;
    }
    if (this.hiddenTimer !== undefined) {
      window.clearTimeout(this.hiddenTimer);
      this.hiddenTimer = undefined;
    }
    if (this.listenerCount() > 0) this.ensureConnected();
  }

  private disconnect(): void {
    if (this.source) {
      this.source.close();
      this.source = null;
    }
    this.setState("disconnected");
  }
}
