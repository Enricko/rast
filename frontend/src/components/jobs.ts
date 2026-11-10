// Follow a background job via SSE (`job.progress|done|failed|cancelled`) with
// a throttled HTTP polling fallback, then resolve with a compact outcome.

import type { AppContext } from "../core/app";
import type { Job } from "../core/types";
import { isAbortError } from "./lifecycle";
import type { AppLifecycle } from "./lifecycle";

export interface JobUpdate {
  progress: number;
  total: number;
  message: string;
}

export interface JobOutcome {
  status: "done" | "failed" | "cancelled";
  error?: string;
  result?: unknown;
}

interface JobEventData {
  id?: string;
  progress?: number;
  total?: number;
  message?: string;
  error?: string;
  result?: unknown;
}

const POLL_MS = 2000;
const TIMEOUT_MS = 15 * 60 * 1000;

/**
 * Watch a job until it finishes. Resolves with `cancelled` when the window is
 * closed before the job settles, so callers never leak pending promises.
 */
export function watchJob(
  ctx: AppContext,
  lifecycle: AppLifecycle,
  jobId: string,
  onUpdate?: (update: JobUpdate) => void,
): Promise<JobOutcome> {
  return new Promise<JobOutcome>((resolve) => {
    let settled = false;
    let lastReport = 0;
    let pollId = 0;
    const unsubscribers: Array<() => void> = [];
    let timeoutId = 0;
    const controller = lifecycle.trackController(new AbortController());

    const cleanup = () => {
      for (const unsubscribe of unsubscribers) {
        try {
          unsubscribe();
        } catch {
          /* ignore */
        }
      }
      unsubscribers.length = 0;
      lifecycle.clearTimeout(timeoutId);
      if (pollId) lifecycle.clearInterval(pollId);
      lifecycle.releaseController(controller);
    };

    const finish = (outcome: JobOutcome) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(outcome);
    };

    const report = (update: JobUpdate) => {
      if (settled) return;
      const now = Date.now();
      // Progress events can be chatty; repaint at most 4x/second.
      if (now - lastReport < 250) return;
      lastReport = now;
      onUpdate?.(update);
    };

    const matches = (data: JobEventData | null): boolean =>
      typeof data?.id === "string" && data.id === jobId;

    unsubscribers.push(
      ctx.events.on("job.progress", (event) => {
        const data = event.data as JobEventData | null;
        if (!matches(data)) return;
        report({
          progress: data?.progress ?? 0,
          total: data?.total ?? 0,
          message: data?.message ?? "",
        });
      }),
    );
    unsubscribers.push(
      ctx.events.on("job.start", (event) => {
        const data = event.data as JobEventData | null;
        if (!matches(data)) return;
        report({ progress: 0, total: 0, message: "Started" });
      }),
    );
    unsubscribers.push(
      ctx.events.on("job.done", (event) => {
        const data = event.data as JobEventData | null;
        if (!matches(data)) return;
        finish({ status: "done", result: data?.result });
      }),
    );
    unsubscribers.push(
      ctx.events.on("job.failed", (event) => {
        const data = event.data as JobEventData | null;
        if (!matches(data)) return;
        finish({ status: "failed", error: data?.error ?? "Job failed" });
      }),
    );
    unsubscribers.push(
      ctx.events.on("job.cancelled", (event) => {
        const data = event.data as JobEventData | null;
        if (!matches(data)) return;
        finish({ status: "cancelled" });
      }),
    );

    const poll = async (): Promise<void> => {
      if (settled) return;
      try {
        const job = await ctx.api.get<Job>(`/api/jobs/${encodeURIComponent(jobId)}`, undefined, {
          signal: controller.signal,
        });
        if (settled) return;
        report({ progress: job.progress ?? 0, total: job.total ?? 0, message: job.message ?? "" });
        if (job.status === "done") finish({ status: "done", result: job.result });
        else if (job.status === "failed") finish({ status: "failed", error: job.error ?? "Job failed" });
        else if (job.status === "cancelled") finish({ status: "cancelled" });
      } catch (error) {
        if (isAbortError(error) || settled) return;
        // Job lookups can race the job row creation; keep polling quietly.
      }
    };

    pollId = lifecycle.setInterval(() => void poll(), POLL_MS);
    lifecycle.setTimeout(() => void poll(), 250);
    timeoutId = lifecycle.setTimeout(() => {
      finish({ status: "failed", error: "Timed out waiting for the job to finish" });
    }, TIMEOUT_MS);

    lifecycle.add(() => finish({ status: "cancelled" }));
  });
}

/** Human-readable progress label, e.g. "3 / 10 · copying files". */
export function jobProgressLabel(update: JobUpdate): string {
  const parts: string[] = [];
  if (update.total > 0) parts.push(`${Math.min(update.progress, update.total)} / ${update.total}`);
  else if (update.progress > 0) parts.push(String(update.progress));
  if (update.message) parts.push(update.message);
  return parts.join(" · ");
}

export function jobProgressFraction(update: JobUpdate): number {
  if (update.total <= 0) return 0;
  return Math.min(1, Math.max(0, update.progress / update.total));
}
