// Applications / Deployment application. OWNER: teammate `frontend-apps`.
import type { AppDefinition } from "../../core/app";
import { h } from "../../core/dom";

export const applicationsApp: AppDefinition = {
  id: "applications",
  name: "Applications",
  icon: "apps",
  description: "Deploy and manage containerised applications",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1000, h: 640 },
  mount(ctx) {
    ctx.root.appendChild(h("div", { class: "empty-state" }, h("div", { class: "empty-title" }, "Applications"), h("div", { class: "empty-detail" }, "Loading…")));
  },
};
