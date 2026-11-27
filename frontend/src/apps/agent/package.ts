// AI Agent as a Package Center app: the window module the package ships
// (ui/app.js + ui/app.css, built by scripts/build-app.mjs).
import type { AppContext } from "../../core/app";
import { agentApp } from "./index";

export default {
  mount: (ctx: AppContext) => agentApp.mount(ctx),
};
