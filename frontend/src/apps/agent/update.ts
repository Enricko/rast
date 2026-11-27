// AI Agent updates from its own window. When App Center has a newer AI Agent
// package, administrators get "AI Agent x is ready · Update" under the chat
// list; Update installs it (the job's progress shows as a bar) and RAST then
// opens this window again on the new version. Each account then sees what
// is new, once per version (whats-new.ts).

import type { AppContext } from "../../core/app";
import { ApiError } from "../../core/api";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { debounceTask, type RealtimeStore } from "../../core/realtime";
import { errorMessage, modal } from "../../core/ui";
import { whatsNewSince } from "./whats-new";

/** AI Agent's package in App Center. */
const PACKAGE_ID = "ai-agent";

interface PackageUpdate {
  from: string;
  to: string;
  replacesDefault: boolean;
}

interface PackageState {
  package: {
    updates: PackageUpdate[];
    activeJob?: { jobId: string; status: string; progress: number; total: number; message: string } | null;
  };
}

interface JobState {
  id?: string;
  status?: string;
  progress?: number;
  total?: number;
  message?: string;
  error?: string | null;
}

export interface UpdateBanner {
  el: HTMLElement;
  destroy(): void;
}

/** The Update banner (administrators only; hidden while nothing is newer). */
export function updateBanner(ctx: AppContext, realtime: RealtimeStore, isAdmin: boolean): UpdateBanner {
  const el = h("div", { class: "ai-update hidden", role: "status", ariaLive: "polite" });
  if (!isAdmin) return { el, destroy: () => undefined };
  let disposed = false;
  let update: PackageUpdate | null = null;
  let jobId = "";
  let pollTimer = 0;
  const unsubscribers: Array<() => void> = [];

  const show = (...children: Array<Node | null>) => {
    el.replaceChildren(...children.filter((c): c is Node => c !== null));
    el.classList.toggle("hidden", children.length === 0);
  };

  function showReady(next: PackageUpdate, error = ""): void {
    const button = h("button", { class: "btn btn-sm btn-primary ai-update-go", type: "button", onClick: () => void start(next) }, error ? "Try again" : "Update");
    show(
      h("span", { class: "ai-update-icon" }, iconSvg("download", 15)),
      h(
        "span",
        { class: "ai-update-text" },
        h("span", { class: "ai-update-title" }, `AI Agent ${next.to} is ready`),
        h("span", { class: `ai-update-sub small${error ? " is-error" : " muted"}`, title: error }, error || `You have ${next.from}`),
      ),
      button,
    );
  }

  function showWorking(job: JobState): void {
    const total = job.total ?? 0;
    const share = total > 0 ? Math.min(100, Math.round(((job.progress ?? 0) * 100) / total)) : null;
    const fill = h("span", { class: `ai-update-fill${share === null ? " is-waiting" : ""}` });
    if (share !== null) fill.style.width = `${share}%`;
    show(
      h("span", { class: "ai-update-icon" }, h("span", { class: "spinner" })),
      h(
        "span",
        { class: "ai-update-text" },
        h("span", { class: "ai-update-title" }, `Updating to ${update?.to ?? "the new version"}${share === null ? "" : ` · ${share}%`}`),
        h("span", { class: "ai-update-sub small muted" }, job.message || "Starting…"),
        h("span", { class: "ai-update-bar", role: "progressbar", ariaLabel: "Update progress", ...(share === null ? {} : { ariaValuenow: share }) }, fill),
      ),
    );
  }

  function showDone(): void {
    // RAST opens this window again on the new version; this is for a
    // desktop that does not.
    const version = update?.to ?? "";
    show(
      h("span", { class: "ai-update-icon is-done" }, iconSvg("check", 15)),
      h("span", { class: "ai-update-text" }, h("span", { class: "ai-update-title" }, `AI Agent ${version} is installed`), h("span", { class: "ai-update-sub small muted" }, "Opening it…")),
      h(
        "button",
        {
          class: "btn btn-sm",
          type: "button",
          onClick: () => {
            const open = ctx.openApp;
            ctx.window.close();
            open("agent");
          },
        },
        "Open it",
      ),
    );
  }

  async function check(): Promise<void> {
    if (jobId) return;
    let state: PackageState;
    try {
      state = await ctx.api.get<PackageState>(`/api/packages/${PACKAGE_ID}`);
    } catch (error) {
      // Without App Center (or the package), there is nothing to update.
      if (!(error instanceof ApiError)) console.warn("AI Agent update check", error);
      if (!disposed) show();
      return;
    }
    if (disposed || jobId) return;
    update = state.package.updates[0] ?? null;
    const active = state.package.activeJob;
    if (active && (active.status === "running" || active.status === "pending" || active.status === "queued")) {
      follow(active.jobId);
      showWorking(active);
      return;
    }
    if (update) showReady(update);
    else show();
  }

  async function start(next: PackageUpdate): Promise<void> {
    update = next;
    showWorking({ message: "Starting…" });
    try {
      const res = await ctx.api.post<{ jobId: string }>(`/api/packages/${PACKAGE_ID}/update`, { version: next.to, makeDefault: next.replacesDefault });
      follow(res.jobId);
    } catch (error) {
      if (!disposed) showReady(next, errorMessage(error));
    }
  }

  function finish(job: JobState): void {
    jobId = "";
    window.clearInterval(pollTimer);
    pollTimer = 0;
    if (disposed) return;
    if (job.status === "done") showDone();
    else if (update) showReady(update, job.status === "cancelled" ? "The update was cancelled." : `The update stopped: ${job.error || "unknown error"}. This version keeps running.`);
  }

  /** Follow the job by its events, and every few seconds in case one is missed. */
  function follow(id: string): void {
    if (jobId === id) return;
    jobId = id;
    window.clearInterval(pollTimer);
    pollTimer = window.setInterval(() => {
      if (!jobId) return;
      ctx.api
        .get<JobState>(`/api/jobs/${encodeURIComponent(jobId)}`)
        .then((job) => {
          if (disposed || job.id !== jobId) return;
          if (job.status === "done" || job.status === "failed" || job.status === "cancelled") finish(job);
          else showWorking(job);
        })
        .catch(() => undefined);
    }, 3000);
  }

  for (const kind of ["job.start", "job.progress"]) {
    unsubscribers.push(
      realtime.on<JobState>(kind, (data) => {
        if (jobId && data?.id === jobId) showWorking(data);
      }),
    );
  }
  for (const [kind, status] of [
    ["job.done", "done"],
    ["job.failed", "failed"],
    ["job.cancelled", "cancelled"],
  ] as const) {
    unsubscribers.push(
      realtime.on<JobState>(kind, (data) => {
        if (jobId && data?.id === jobId) finish({ ...data, status });
      }),
    );
  }
  const recheck = debounceTask(() => void check(), 800);
  unsubscribers.push(realtime.on("package.catalog", () => recheck.run()));
  unsubscribers.push(realtime.on("package.changed", () => recheck.run()));
  void check();

  return {
    el,
    destroy() {
      disposed = true;
      window.clearInterval(pollTimer);
      recheck.cancel();
      for (const unsubscribe of unsubscribers.splice(0)) unsubscribe();
    },
  };
}

/** What is new since the version this account last saw; shown once. */
export function showWhatsNew(ctx: AppContext, current: string | undefined, seen: string | null | undefined): void {
  if (!current) return;
  const releases = whatsNewSince(seen, current);
  if (!releases.length) return;
  let marked = false;
  const markSeen = () => {
    if (marked) return;
    marked = true;
    void ctx.api.post("/api/ai/whats-new", { version: current }).catch(() => undefined);
  };
  modal({
    within: ctx.root,
    title: `What's new in AI Agent ${current}`,
    body: h(
      "div",
      { class: "ai-whats-new" },
      ...releases.map((release) =>
        h(
          "section",
          { class: "ai-whats-new-release" },
          releases.length > 1 || release.version !== current
            ? h("h3", { class: "ai-whats-new-version" }, `Version ${release.version}`, h("span", { class: "small muted" }, release.date))
            : null,
          h("ul", { class: "ai-whats-new-items" }, ...release.items.map((item) => h("li", {}, item))),
        ),
      ),
    ),
    actions: [{ label: "Got it", variant: "primary", onClick: markSeen }],
    onClose: markSeen,
  });
}
