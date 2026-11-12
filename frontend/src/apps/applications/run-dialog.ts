// "Run" an image (Docker Desktop's Run dialog): a name, the ports to publish,
// volumes and environment, turned into a managed RAST app so it can be
// stopped, redeployed and backed up like every other app.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { formatBytes } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { errorMessage, modal } from "../../core/ui";
import type { AppLifecycle } from "../../components/lifecycle";
import { jobProgressFraction, jobProgressLabel, watchJob } from "../../components/jobs";
import { formError, progressBar } from "../../components/states";
import {
  composeForRun,
  initialRunPlan,
  runPlanErrors,
  usedHostPorts,
  type EngineContainer,
  type ImageDetails,
  type RunEnv,
  type RunPlan,
  type RunVolume,
} from "./model";

export interface RunDialogDeps {
  ctx: AppContext;
  lc: AppLifecycle;
  reference: string;
  containers: EngineContainer[];
  /** The app was created and deployed. */
  onStarted: (name: string) => void;
}

/** Load the image details, then show the dialog. Resolves once it is shown. */
export async function openRunDialog(deps: RunDialogDeps): Promise<void> {
  const { ctx } = deps;
  let details: ImageDetails;
  try {
    details = await ctx.api.get<ImageDetails>("/api/docker/images/inspect", { image: deps.reference }, { signal: deps.lc.signal });
  } catch (error) {
    ctx.toast.error(`Cannot read ${deps.reference}: ${errorMessage(error)}`);
    return;
  }
  showDialog(deps, details);
}

function showDialog(deps: RunDialogDeps, details: ImageDetails): void {
  const { ctx } = deps;
  const used = usedHostPorts(deps.containers);
  const plan = initialRunPlan(deps.reference, details, deps.containers);

  const nameInput = h("input", { class: "input", type: "text", value: plan.name, autocomplete: "off", spellcheck: false, ariaLabel: "App name" });

  // Ports: one row per exposed port; an empty host port keeps it private.
  const portInputs = plan.ports.map((port) => {
    const input = h("input", {
      class: "input dk-run-port",
      type: "number",
      min: "1",
      max: "65535",
      inputMode: "numeric",
      placeholder: "Not published",
      value: port.host === null ? "" : String(port.host),
      ariaLabel: `Host port for ${port.container}/${port.protocol}`,
    });
    return { port, input };
  });
  const portsBlock = portInputs.length
    ? h(
        "div",
        { class: "field" },
        h("span", { class: "field-label" }, "Ports"),
        h(
          "div",
          { class: "dk-run-ports" },
          ...portInputs.map(({ port, input }) =>
            h("label", { class: "dk-run-port-row" }, h("span", { class: "dk-run-port-host" }, input), h("span", { class: "dk-run-arrow muted", ariaHidden: true }, "→"), h("span", { class: "mono" }, `${port.container}/${port.protocol}`)),
          ),
        ),
        h("span", { class: "field-hint" }, "Host port on this server → port in the container. Leave empty to keep a port private."),
      )
    : h("p", { class: "small muted dk-run-noports" }, "This image does not declare any ports. Add them later by editing the app’s Compose file.");

  // Volumes: named volumes owned by the app (data survives updates).
  const volumeRows = h("div", { class: "dk-run-rows" });
  const volumeList: Array<{ name: HTMLInputElement; path: HTMLInputElement; root: HTMLElement }> = [];
  const addVolume = (initial?: RunVolume): void => {
    const name = h("input", { class: "input dk-run-small mono", type: "text", placeholder: "data", value: initial?.name ?? "", ariaLabel: "Volume name", spellcheck: false });
    const path = h("input", { class: "input dk-run-small mono", type: "text", placeholder: "/data", value: initial?.path ?? "", ariaLabel: "Path in the container", spellcheck: false });
    const remove = removeButton("Remove volume");
    const root = h("div", { class: "dk-run-row dk-run-volume" }, name, h("span", { class: "dk-run-arrow muted", ariaHidden: true }, "→"), path, remove);
    remove.addEventListener("click", () => {
      volumeList.splice(volumeList.findIndex((row) => row.root === root), 1);
      root.remove();
    });
    volumeList.push({ name, path, root });
    volumeRows.append(root);
  };
  for (const volume of plan.volumes) addVolume(volume);
  const addVolumeButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, iconSvg("plus", 13), h("span", {}, "Add volume"));
  addVolumeButton.addEventListener("click", () => addVolume());

  // Environment variables (stored with the app; secrets are encrypted).
  const envRows = h("div", { class: "appmgmt-env-rows" });
  const envList: Array<{ key: HTMLInputElement; value: HTMLInputElement; secret: HTMLInputElement; root: HTMLElement }> = [];
  const addEnv = (initial?: RunEnv): void => {
    const key = h("input", { class: "input appmgmt-env-key mono", type: "text", placeholder: "KEY", value: initial?.key ?? "", ariaLabel: "Variable name", spellcheck: false });
    const value = h("input", { class: "input appmgmt-env-value", type: "text", placeholder: "value", value: initial?.value ?? "", ariaLabel: "Value" });
    const secret = h("input", { type: "checkbox", ariaLabel: "Secret value" });
    secret.addEventListener("change", () => {
      value.type = secret.checked ? "password" : "text";
    });
    const remove = removeButton("Remove variable");
    const root = h("div", { class: "appmgmt-env-row" }, key, value, h("label", { class: "checkbox appmgmt-secret-label", title: "Encrypt and mask this value" }, secret, "secret"), remove);
    remove.addEventListener("click", () => {
      envList.splice(envList.findIndex((row) => row.root === root), 1);
      root.remove();
    });
    envList.push({ key, value, secret, root });
    envRows.append(root);
    key.focus();
  };
  const addEnvButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, iconSvg("plus", 13), h("span", {}, "Add variable"));
  addEnvButton.addEventListener("click", () => addEnv());
  const defaults = details.env.map((entry) => entry.key).filter((key) => key !== "PATH");
  const defaultsHint = defaults.length ? h("span", { class: "field-hint" }, `The image already sets ${defaults.slice(0, 6).join(", ")}${defaults.length > 6 ? "…" : ""}. Add a variable with the same name to change it.`) : null;

  const more = h(
    "details",
    { class: "dk-run-more" },
    h("summary", {}, "Volumes and environment"),
    h("div", { class: "field" }, h("span", { class: "field-label" }, "Volumes"), volumeRows, h("span", { class: "field-hint" }, "Named volumes keep the app’s data when it is updated or recreated."), addVolumeButton),
    h("div", { class: "field" }, h("span", { class: "field-label" }, "Environment variables"), envRows, defaultsHint, addEnvButton),
  );

  const facts = [details.os && details.architecture ? `${details.os}/${details.architecture}` : "", details.sizeBytes ? formatBytes(details.sizeBytes) : ""].filter(Boolean).join(" · ");
  const validation = formError("");
  const progress = progressBar();
  progress.el.classList.add("hidden");
  const status = h("div", { class: "small muted", role: "status" });
  const body = h(
    "div",
    { class: "col dk-run-form" },
    h("div", { class: "dk-run-image" }, h("span", { class: "dk-glyph", ariaHidden: true }, iconSvg("layers", 18)), h("span", { class: "mono dk-run-ref" }, deps.reference), facts ? h("span", { class: "muted small" }, facts) : null),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), nameInput),
    portsBlock,
    more,
    h("p", { class: "small muted dk-run-note" }, "It runs as a RAST app: start, stop, redeploy and back it up like your other apps."),
    progress.el,
    status,
    validation,
  );

  const readPlan = (): RunPlan => ({
    name: nameInput.value.trim(),
    image: deps.reference,
    ports: portInputs.map(({ port, input }) => ({ ...port, host: input.value.trim() === "" ? null : Number(input.value) })),
    volumes: volumeList.map((row) => ({ name: row.name.value.trim(), path: row.path.value.trim() })).filter((volume) => volume.name || volume.path),
    env: envList.map((row) => ({ key: row.key.value.trim(), value: row.value.value, secret: row.secret.checked })).filter((variable) => variable.key || variable.value),
  });

  let submitted = false;
  modal({
    title: `Run ${plan.name}`,
    body,
    wide: true,
    actions: [
      { label: "Cancel", variant: "ghost", onClick: () => {} },
      {
        label: "Run",
        variant: "primary",
        keepOpen: true,
        onClick: async (closeDialog) => {
          if (submitted) return;
          validation.textContent = "";
          const next = readPlan();
          const errors = runPlanErrors(next, used);
          if (errors.length) {
            validation.textContent = errors[0]!;
            if (errors[0]!.includes("name for the app") || errors[0]!.includes("name is too long")) nameInput.focus();
            else if (/volume|container path/i.test(errors[0]!)) more.open = true;
            else if (/variable/i.test(errors[0]!)) more.open = true;
            return;
          }
          submitted = true;
          progress.el.classList.remove("hidden");
          progress.setError(false);
          progress.update(0.08);
          status.textContent = "Creating the app…";
          try {
            const response = await ctx.api.post<{ jobId?: string }>("/api/apps", { name: next.name, compose: composeForRun(next), env: next.env });
            if (response?.jobId) {
              status.textContent = "Starting…";
              const outcome = await watchJob(ctx, deps.lc, response.jobId, (update) => {
                progress.update(jobProgressFraction(update));
                status.textContent = jobProgressLabel(update) || "Starting…";
              });
              if (outcome.status !== "done") {
                progress.setError(true);
                validation.textContent = outcome.status === "failed" ? `It could not start: ${outcome.error ?? "unknown error"}. The app was created; fix it from its page.` : "Starting was cancelled.";
                submitted = false;
                deps.onStarted(next.name);
                return;
              }
            }
            progress.update(1);
            ctx.toast.success(`${next.name} is running`);
            closeDialog();
            deps.onStarted(next.name);
          } catch (error) {
            progress.setError(true);
            status.textContent = "";
            validation.textContent = errorMessage(error);
            submitted = false;
          }
        },
      },
    ],
  });
  window.setTimeout(() => {
    nameInput.focus();
    nameInput.select();
  }, 40);
}

function removeButton(label: string): HTMLButtonElement {
  const button = h("button", { class: "appmgmt-icon-btn is-danger", type: "button", ariaLabel: label, title: label });
  button.append(iconSvg("close", 14));
  return button;
}
