// File Station application. OWNER: teammate `frontend-apps`.
import type { AppDefinition } from "../../core/app";
import { h } from "../../core/dom";
import { iconEl } from "../../core/icons";

export const filesApp: AppDefinition = {
  id: "files",
  name: "File Station",
  icon: "files",
  description: "Browse, upload, download and organise your files",
  singleton: true,
  defaultSize: { w: 1040, h: 660 },
  mount(ctx) {
    ctx.root.appendChild(
      h("div", { class: "empty-state" }, h("div", { class: "empty-title" }, "File Station"), h("div", { class: "empty-detail" }, "Loading…"), iconEl("folder", 32)),
    );
  },
};
