// File Station application. OWNER: teammate `frontend-apps`.
import type { AppDefinition } from "../../core/app";
import { mountFileStation } from "./station";

export const filesApp: AppDefinition = {
  id: "files",
  name: "File Station",
  icon: "files",
  description: "Browse, upload, download and organise your files",
  singleton: true,
  defaultSize: { w: 1080, h: 680 },
  minSize: { w: 380, h: 300 },
  mount(ctx) {
    return mountFileStation(ctx);
  },
};
