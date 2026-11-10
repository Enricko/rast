// Control Panel → Components: component health, host resources, database and
// job counts. Realtime events refresh the affected numbers; there is no routine
// Refresh button or polling timer.

import { apiErrorMessage } from "../../core/api";
import { card, statusBadge, usageBar } from "../../core/components";
import { debounceTask, getRealtime } from "../../core/realtime";
import { h } from "../../core/dom";
import { formatBytes, formatDateTime, formatDuration, formatTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { ComponentStatusResponse, SystemInfo } from "../../core/types";
import { pageIntro, scrollToSection, type TabRenderContext } from "./common";

export async function renderComponentsTab(tab: TabRenderContext): Promise<(() => void) | void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  let lastUpdated: number | null = null;
  let loading = false;
  let disposed = false;

  const intro = pageIntro(
    "Components",
    "Live health of RAST internals plus host resources and database footprint.",
  );

  const lastUpdatedLabel = h("span", { class: "small muted" });
  const toolbar = h(
    "div",
    { class: "cp-toolbar" },
    h("div", { class: "cp-toolbar-spacer" }),
    lastUpdatedLabel,
  );
  const content = h("div", { class: "components-content" });
  host.append(intro, toolbar, content);

  async function load(silent = false): Promise<void> {
    if (loading || disposed) return;
    loading = true;
    if (!silent) {
      content.replaceChildren(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading component health…")));
    }
    try {
      const [components, info] = await Promise.all([
        ctx.api.get<ComponentStatusResponse>("/api/system/components"),
        ctx.api.get<SystemInfo>("/api/system/info"),
      ]);
      if (disposed) return;
      render(components, info);
      lastUpdated = Date.now();
      lastUpdatedLabel.textContent = `Updated ${formatTime(lastUpdated)}`;
    } catch (error) {
      if (disposed) return;
      lastUpdatedLabel.textContent = "";
      content.replaceChildren(
        h(
          "div",
          { class: "error-state" },
          h("div", { class: "error-title" }, "Could not load component health"),
          h("div", { class: "error-detail" }, apiErrorMessage(error)),
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void load() }, "Retry"),
        ),
      );
    } finally {
      loading = false;
    }
  }

  function render(components: ComponentStatusResponse, info: SystemInfo): void {
    content.replaceChildren();
    const grid = h("div", { class: "cp-grid" });

    // Health -------------------------------------------------------------
    const healthList = h("div", { class: "component-list" });
    const sorted = [...components.components].sort((a, b) => {
      const weight: Record<string, number> = { error: 0, warning: 1, ok: 2, disabled: 3 };
      return (weight[a.status] ?? 4) - (weight[b.status] ?? 4);
    });
    for (const component of sorted) {
      healthList.appendChild(
        h(
          "div",
          { class: `component-row component-${component.status}` },
          h("span", { class: "component-dot", ariaHidden: true }),
          h(
            "div",
            { class: "component-main" },
            h("div", { class: "component-name" }, component.name),
            h("div", { class: "component-detail small muted" }, component.detail || "—"),
          ),
          statusBadge(component.status),
        ),
      );
    }
    if (sorted.length === 0) {
      healthList.appendChild(h("div", { class: "table-empty" }, "No components reported."));
    }
    grid.appendChild(
      card({
        title: "Component health",
        subtitle: `${components.components.filter((component) => component.status === "ok").length} of ${components.components.length} healthy`,
        children: [healthList],
      }),
    );

    // Resources ----------------------------------------------------------
    const memoryTotal = info.host.memoryTotalBytes;
    const memoryUsed = Math.max(0, memoryTotal - info.host.memoryAvailableBytes);
    grid.appendChild(
      card({
        title: "Host resources",
        subtitle: `${info.host.hostname} · ${info.host.os}`,
        children: [
          info.host.memoryTotalBytes > 0
            ? h(
                "div",
                { class: "resource-row" },
                h(
                  "div",
                  { class: "resource-head" },
                  h("span", { class: "resource-label" }, iconSvg("memory", 15), "Memory"),
                  h("span", { class: "small muted" }, `${formatBytes(memoryUsed)} / ${formatBytes(memoryTotal)}`),
                ),
                usageBar(memoryUsed, memoryTotal),
              )
            : h("div", { class: "muted small" }, "Memory information unavailable."),
          h(
            "div",
            { class: "resource-row" },
            h(
              "div",
              { class: "resource-head" },
              h("span", { class: "resource-label" }, iconSvg("cpu", 15), "CPU"),
              h("span", { class: "small muted" }, `${info.host.cpuCount} core${info.host.cpuCount === 1 ? "" : "s"}`),
            ),
            h("div", { class: "small muted" }, `Uptime ${formatDuration(info.host.uptimeSecs)} · ${info.host.arch}`),
          ),
          diskRow("Storage disk", info.storage),
          diskRow("Data disk", info.dataDisk),
        ],
      }),
    );

    // Database -----------------------------------------------------------
    grid.appendChild(
      card({
        title: "Database",
        subtitle: "SQLite footprint and record counts",
        children: [
          h(
            "div",
            { class: "stat-row" },
            statInline("Database size", formatBytes(info.database.totalBytes)),
            statInline("WAL", formatBytes(info.database.walBytes)),
            statInline("Users", String(info.counts.users)),
            statInline("Administrators", String(info.counts.admins)),
            statInline("Managed apps", String(info.counts.managedApps)),
            statInline("Jobs running", String(components.jobsRunning)),
          ),
          h(
            "div",
            { class: "small faint", style: "margin-top:10px" },
            `Host: ${info.host.kernel} · RAST ${info.version} · server time ${formatTime(info.serverTime)}`,
          ),
        ],
      }),
    );

    // Summary strip ------------------------------------------------------
    const failing = components.components.filter((component) => component.status === "error").length;
    const warnings = components.components.filter((component) => component.status === "warning").length;
    const summary = h(
      "div",
      { class: "cp-span-2 component-summary" },
      h("div", { class: `summary-pill${failing > 0 ? " is-error" : warnings > 0 ? " is-warn" : " is-ok"}` }, failing > 0 ? `${failing} component${failing === 1 ? "" : "s"} failing` : warnings > 0 ? `${warnings} warning${warnings === 1 ? "" : "s"}` : "All components healthy"),
      h("div", { class: "small muted" }, `Updated ${lastUpdated ? formatDateTime(lastUpdated) : "—"}`),
    );
    grid.appendChild(summary);
    content.appendChild(grid);
  }

  function diskRow(label: string, disk: SystemInfo["storage"]): HTMLElement {
    if (!disk.available) {
      return h(
        "div",
        { class: "resource-row" },
        h("div", { class: "resource-head" }, h("span", { class: "resource-label" }, iconSvg("disk", 15), label), h("span", { class: "small muted" }, "unavailable")),
        h("div", { class: "small faint" }, disk.reason ?? ""),
      );
    }
    return h(
      "div",
      { class: "resource-row" },
      h(
        "div",
        { class: "resource-head" },
        h("span", { class: "resource-label" }, iconSvg("disk", 15), label),
        h("span", { class: "small muted" }, `${formatBytes(disk.usedBytes)} / ${formatBytes(disk.totalBytes)}`),
      ),
      usageBar(disk.usedBytes ?? 0, disk.totalBytes ?? 0),
    );
  }

  function statInline(label: string, value: string): HTMLElement {
    return h(
      "div",
      { class: "stat-inline" },
      h("div", { class: "stat-label" }, label),
      h("div", { class: "stat-value-sm" }, value),
    );
  }

  await load();
  scrollToSection(host, tab.section);

  // Realtime: jobs, backups, apps, docker, settings and account changes all
  // affect something shown here. Progress events are ignored on purpose.
  const realtime = getRealtime(ctx.events);
  const refresh = debounceTask(() => void load(true), 600);
  const unsubscribers: Array<() => void> = [
    realtime.on("job.queued", () => refresh.run()),
    realtime.on("job.start", () => refresh.run()),
    realtime.on("job.done", () => refresh.run()),
    realtime.on("job.failed", () => refresh.run()),
    realtime.on("job.cancelled", () => refresh.run()),
    realtime.on("backup.changed", () => refresh.run()),
    realtime.on("app.changed", () => refresh.run()),
    realtime.on("docker.changed", () => refresh.run()),
    realtime.on("settings.changed", () => refresh.run()),
    realtime.on("user.changed", () => refresh.run()),
    realtime.onResync(() => refresh.run()),
  ];

  return () => {
    disposed = true;
    refresh.cancel();
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}
