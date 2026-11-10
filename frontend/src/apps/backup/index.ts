// Backup Center application. OWNER: teammate `frontend-apps`.
import type { AppDefinition } from "../../core/app";
import { h } from "../../core/dom";

export const backupApp: AppDefinition = {
  id: "backup",
  name: "Backup Center",
  icon: "backup",
  description: "Back up and restore RAST configuration and data",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 980, h: 640 },
  mount(ctx) {
    ctx.root.appendChild(h("div", { class: "empty-state" }, h("div", { class: "empty-title" }, "Backup Center"), h("div", { class: "empty-detail" }, "Loading…")));
  },
};
