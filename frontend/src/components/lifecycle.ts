// Window-scoped lifecycle helpers shared by the four main applications.
//
// The shell does not (yet) deliver a reliable close callback to apps, so we
// poll `window.isClosed()` once per second. Every timer, animation frame,
// SSE subscription and AbortController registered through this helper is torn
// down exactly once when the window closes.

import type { AppContext } from "../core/app";

const CLOSE_POLL_MS = 1000;

export class AppLifecycle {
  private readonly ctx: AppContext;
  private readonly controller = new AbortController();
  private readonly cleanups = new Set<() => void>();
  private readonly timeouts = new Set<number>();
  private readonly intervals = new Set<number>();
  private readonly frames = new Set<number>();
  private readonly controllers = new Set<AbortController>();
  private closePoll: number | undefined;
  private disposed = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.closePoll = window.setInterval(() => {
      if (this.ctx.window.isClosed()) this.dispose();
    }, CLOSE_POLL_MS);
  }

  get signal(): AbortSignal {
    return this.controller.signal;
  }

  get isDisposed(): boolean {
    return this.disposed;
  }

  /** Register a cleanup; runs immediately when the app is already disposed. */
  add(cleanup: () => void): void {
    if (this.disposed) {
      safeRun(cleanup);
      return;
    }
    this.cleanups.add(cleanup);
  }

  /** Register an unsubscribe function returned by `events.on(...)`. */
  listen(unsubscribe: () => void): void {
    this.add(unsubscribe);
  }

  setTimeout(fn: () => void, ms: number): number {
    if (this.disposed) return 0;
    const id = window.setTimeout(() => {
      this.timeouts.delete(id);
      if (!this.disposed) safeRun(fn);
    }, ms);
    this.timeouts.add(id);
    return id;
  }

  clearTimeout(id: number): void {
    if (!id) return;
    window.clearTimeout(id);
    this.timeouts.delete(id);
  }

  setInterval(fn: () => void, ms: number): number {
    if (this.disposed) return 0;
    const id = window.setInterval(() => {
      if (!this.disposed) safeRun(fn);
    }, ms);
    this.intervals.add(id);
    return id;
  }

  clearInterval(id: number): void {
    if (!id) return;
    window.clearInterval(id);
    this.intervals.delete(id);
  }

  /** requestAnimationFrame tracked by the lifecycle. */
  frame(fn: () => void): number {
    if (this.disposed) return 0;
    const id = window.requestAnimationFrame(() => {
      this.frames.delete(id);
      if (!this.disposed) safeRun(fn);
    });
    this.frames.add(id);
    return id;
  }

  cancelFrame(id: number): void {
    if (!id) return;
    window.cancelAnimationFrame(id);
    this.frames.delete(id);
  }

  /**
   * Track an in-flight request controller so it is aborted when the window
   * closes. Bounded: call `releaseController` when the request settles.
   */
  trackController(controller: AbortController): AbortController {
    if (this.disposed) {
      controller.abort();
      return controller;
    }
    this.controllers.add(controller);
    return controller;
  }

  releaseController(controller: AbortController): void {
    this.controllers.delete(controller);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.closePoll !== undefined) {
      window.clearInterval(this.closePoll);
      this.closePoll = undefined;
    }
    for (const id of this.timeouts) window.clearTimeout(id);
    this.timeouts.clear();
    for (const id of this.intervals) window.clearInterval(id);
    this.intervals.clear();
    for (const id of this.frames) window.cancelAnimationFrame(id);
    this.frames.clear();
    for (const controller of this.controllers) safeRun(() => controller.abort());
    this.controllers.clear();
    safeRun(() => this.controller.abort());
    for (const cleanup of this.cleanups) safeRun(cleanup);
    this.cleanups.clear();
  }
}

function safeRun(fn: () => void): void {
  try {
    fn();
  } catch (error) {
    console.error("lifecycle cleanup failed", error);
  }
}

/** True when an error was caused by an AbortController cancellation. */
export function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}
