// Control Panel → Storage: filesystem capacity, storage locations, managed
// content and per-user quota usage.

import { apiErrorMessage } from "../../core/api";
import { card, codeValue, iconButton, usageBar } from "../../core/components";
import { h } from "../../core/dom";
import { formatBytes, formatDateTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { Page, StorageSettings, SystemInfo, User } from "../../core/types";
import { errorMessage } from "../../core/ui";
import { pageIntro, quotaCell, scrollToSection, type TabRenderContext } from "./common";

function diskBar(
  disk: { available: boolean; reason?: string; mountPoint?: string; filesystem?: string; totalBytes?: number; usedBytes?: number; availableBytes?: number },
  label: string,
): HTMLElement {
  if (!disk.available) {
    return h(
      "div",
      { class: "disk-block" },
      h("div", { class: "disk-block-title" }, label),
      h("div", { class: "muted small" }, `Unavailable: ${disk.reason ?? "unknown reason"}`),
    );
  }
  return h(
    "div",
    { class: "disk-block" },
    h(
      "div",
      { class: "disk-block-head" },
      h("div", { class: "disk-block-title" }, label),
      h("div", { class: "small muted" }, disk.mountPoint ?? ""),
    ),
    usageBar(disk.usedBytes ?? 0, disk.totalBytes ?? 0),
    h(
      "div",
      { class: "small faint" },
      `${formatBytes(disk.availableBytes)} free${disk.filesystem ? ` · ${disk.filesystem}` : ""}`,
    ),
  );
}

export async function renderStorageTab(tab: TabRenderContext): Promise<void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  actions.appendChild(
    iconButton("refresh", "Refresh storage data", () => void load(), { title: "Refresh" }),
  );

  const results = await Promise.allSettled([
    ctx.api.get<SystemInfo>("/api/system/info"),
    ctx.api.get<StorageSettings>("/api/settings/storage"),
    ctx.api.get<Page<User>>("/api/users", { page: 1, perPage: 100 }),
  ]);
  const info = results[0].status === "fulfilled" ? results[0].value : null;
  const storage = results[1].status === "fulfilled" ? results[1].value : null;
  const usersPage = results[2].status === "fulfilled" ? results[2].value : null;
  const firstError = results.find((result) => result.status === "rejected");
  if (!info && !storage) {
    throw new Error(
      firstError && firstError.status === "rejected" ? apiErrorMessage(firstError.reason) : "Storage data unavailable",
    );
  }

  const intro = pageIntro(
    "Capacity & allocations",
    "Where RAST keeps data, how much space is available and how quotas are distributed across accounts.",
  );
  host.appendChild(intro);

  const grid = h("div", { class: "cp-grid" });

  // Capacity -------------------------------------------------------------
  const capacityChildren: Array<Node | null> = [];
  if (storage) capacityChildren.push(diskBar(storage.filesystem, "Storage volume"));
  else if (info) capacityChildren.push(diskBar(info.storage, "Storage volume"));
  if (info) capacityChildren.push(diskBar(info.dataDisk, "Data volume"));
  if (capacityChildren.length === 0) {
    capacityChildren.push(h("div", { class: "muted small" }, "Capacity information is unavailable."));
  }
  grid.appendChild(card({ title: "Capacity", subtitle: "Filesystem utilisation for RAST directories", children: capacityChildren }));

  // Locations ------------------------------------------------------------
  const locationRows: Array<Node> = [];
  if (storage) {
    const rows: Array<[string, string]> = [
      ["Data directory", storage.dataDir],
      ["Storage directory", storage.storageDir],
      ["User homes", storage.homesDir],
      ["Shared folders", storage.sharedDir],
      ["Recycle bin", storage.trashDir],
    ];
    for (const [label, value] of rows) {
      locationRows.push(
        h("div", { class: "location-row" }, h("span", { class: "location-label" }, label), codeValue(value || "—")),
      );
    }
  } else if (info) {
    const server = info.server;
    const pick = (key: string): string => {
      const value = server[key];
      return typeof value === "string" ? value : "—";
    };
    locationRows.push(
      h("div", { class: "location-row" }, h("span", { class: "location-label" }, "Data directory"), codeValue(pick("dataDir"))),
      h("div", { class: "location-row" }, h("span", { class: "location-label" }, "Storage directory"), codeValue(pick("storageDir"))),
    );
    locationRows.push(h("div", { class: "banner banner-warn" }, "Detailed storage locations require the /api/settings/storage endpoint."));
  }
  grid.appendChild(card({ title: "Locations", subtitle: "Paths on the server filesystem", children: locationRows }));

  // Managed content ------------------------------------------------------
  if (storage) {
    const homeNames = storage.homes.slice(0, 12);
    grid.appendChild(
      card({
        title: "Managed content",
        subtitle: "What RAST currently stores",
        children: [
          h(
            "div",
            { class: "stat-row" },
            h(
              "div",
              { class: "stat-inline" },
              h("div", { class: "stat-label" }, "Files on disk"),
              h("div", { class: "stat-value-sm" }, formatBytes(storage.usedBytes)),
            ),
            h(
              "div",
              { class: "stat-inline" },
              h("div", { class: "stat-label" }, "User homes"),
              h("div", { class: "stat-value-sm" }, String(storage.homes.length)),
            ),
            h(
              "div",
              { class: "stat-inline" },
              h("div", { class: "stat-label" }, "Shared folders"),
              h("div", { class: "stat-value-sm" }, String(storage.sharedFolders.length)),
            ),
          ),
          homeNames.length > 0
            ? h(
                "div",
                { class: "chip-row" },
                ...homeNames.map((name) => h("span", { class: "chip" }, iconSvg("folder", 13), name)),
                storage.homes.length > homeNames.length
                  ? h("span", { class: "chip chip-more" }, `+${storage.homes.length - homeNames.length} more`)
                  : null,
              )
            : h("div", { class: "small faint" }, "No user home directories found."),
        ],
      }),
    );
  }

  // Quota note -----------------------------------------------------------
  grid.appendChild(
    card({
      title: "Quotas",
      subtitle: "Enforced on upload, copy and restore",
      children: [
        h(
          "p",
          { class: "muted small" },
          "Each account has a storage quota (0 means unlimited). RAST blocks writes that would exceed a user's quota and exposes usage here for administrators.",
        ),
        h(
          "button",
          {
            class: "btn btn-sm",
            type: "button",
            onClick: () => {
              ctx.openApp("controlpanel", { tab: "users" });
            },
          },
          iconSvg("users", 15),
          "Manage users & quotas",
        ),
      ],
    }),
  );

  if (usersPage) {
    const sorted = [...usersPage.items].sort((a, b) => b.usedBytes - a.usedBytes);
    const tableBody = h("tbody");
    for (const user of sorted) {
      tableBody.appendChild(
        h(
          "tr",
          {},
          h(
            "td",
            {},
            h(
              "div",
              { class: "user-cell" },
              h("span", { class: "avatar avatar-sm" }, (user.displayName || user.username).slice(0, 1).toUpperCase()),
              h(
                "span",
                { class: "user-cell-text" },
                h("span", { class: "user-cell-name" }, user.displayName || user.username),
                h("span", { class: "user-cell-id small faint" }, `@${user.username}`),
              ),
            ),
          ),
          h("td", {}, quotaCell(user)),
          h("td", { class: "num small" }, formatBytes(user.usedBytes)),
          h("td", { class: "hide-sm small muted" }, user.lastLoginAt ? formatDateTime(user.lastLoginAt) : "Never"),
        ),
      );
    }
    const table = h(
      "table",
      { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "User"), h("th", {}, "Quota usage"), h("th", { class: "num" }, "Used"), h("th", { class: "hide-sm" }, "Last sign-in"))),
      tableBody,
    );
    grid.appendChild(
      h(
        "div",
        { class: "cp-span-2" },
        card({
          title: "Per-user usage",
          subtitle:
            usersPage.total > usersPage.items.length
              ? `Showing the first ${usersPage.items.length} of ${usersPage.total} accounts`
              : `${usersPage.items.length} account${usersPage.items.length === 1 ? "" : "s"}`,
          children: [h("div", { class: "table-wrap" }, table)],
          padded: false,
        }),
      ),
    );
  } else if (firstError && firstError.status === "rejected") {
    grid.appendChild(
      h(
        "div",
        { class: "cp-span-2" },
        card({
          title: "Per-user usage",
          children: [
            h("div", { class: "error-state inline-state" }, h("div", { class: "error-detail" }, errorMessage(firstError.reason))),
          ],
        }),
      ),
    );
  }

  host.appendChild(grid);

  async function load(): Promise<void> {
    await renderStorageTab(tab);
  }

  scrollToSection(host, tab.section);
}
