// Resource Monitor application. OWNER: teammate `frontend-apps`.
import type { AppDefinition } from "../../core/app";
import { h } from "../../core/dom";

export const monitorApp: AppDefinition = {
  id: "monitor",
  name: "Resource Monitor",
  icon: "monitor",
  description: "Live CPU, memory, disk and network metrics",
  singleton: true,
  defaultSize: { w: 980, h: 640 },
  mount(ctx) {
    ctx.root.appendChild(h("div", { class: "empty-state" }, h("div", { class: "empty-title" }, "Resource Monitor"), h("div", { class: "empty-detail" }, "Loading…")));
  },
};
