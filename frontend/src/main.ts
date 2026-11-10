import "./styles/base.css";
import "./styles/apps.css";
import { boot } from "./shell";

const app = document.getElementById("app");
if (app) {
  void boot(app);
}
