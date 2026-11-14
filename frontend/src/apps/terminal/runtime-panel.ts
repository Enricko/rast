// Runtime environment dialog: what a new Terminal tab runs, and where the
// per-user default runtime versions are changed.
//
// Data comes from GET /api/terminal/env; defaults are saved with
// PATCH /api/terminal/env {"defaults":{id:version}}. A tab already running
// keeps its environment until it is reopened (the note in the dialog says so).

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { banner } from "../../components/states";
import { errorMessage, modal } from "../../core/ui";
import {
  effectiveRuntimeVersion,
  normalizeTerminalEnv,
  parseTerminalSessions,
  runtimeHasProblems,
  runtimeSourceLabel,
  runtimeVersionChoices,
  TERMINAL_ENV_PATH,
  TERMINAL_SESSIONS_PATH,
  validateRuntimeOverrides,
  type TerminalEnv,
  type TerminalRuntime,
  type TerminalSessionInfo,
} from "./model";

export interface RuntimePanelOptions {
  ctx: AppContext;
  env: TerminalEnv | null;
  /** Error from the last environment request, if any. */
  error: unknown;
  /** Versions selected for new tabs (per-runtime overrides). */
  overrides: Record<string, string>;
  onOverridesChange(next: Record<string, string>): void;
  onEnvUpdated(env: TerminalEnv): void;
}

export function openRuntimePanel(options: RuntimePanelOptions): void {
  const { ctx } = options;
  let env = options.env;
  let overrides = { ...options.overrides };
  let sessions: TerminalSessionInfo[] | null = null;
  let sessionsChecked = false;
  const body = h("div", { class: "term-runtime-panel" });
  const sessionsLine = h("span", { class: "term-runtime-sessions small muted" }, "Checking open sessions…");

  const renderSessions = (): void => {
    if (!env) {
      sessionsLine.textContent = "";
      return;
    }
    if (!sessionsChecked) return;
    const open = sessions?.length ?? 0;
    if (sessions === null) {
      sessionsLine.textContent = "";
      return;
    }
    sessionsLine.textContent = `${open} of ${env.maxSessions} terminal session${env.maxSessions === 1 ? "" : "s"} open`;
  };

  const refreshSessions = (): void => {
    void ctx.api
      .get<unknown>(TERMINAL_SESSIONS_PATH)
      .then((raw) => {
        sessions = parseTerminalSessions(raw);
        sessionsChecked = true;
        renderSessions();
      })
      .catch(() => {
        sessionsChecked = true;
        sessions = null;
        renderSessions();
      });
  };

  const setOverride = (runtimeId: string, version: string): void => {
    if (version) overrides[runtimeId] = version;
    else delete overrides[runtimeId];
    overrides = validateRuntimeOverrides(env ?? normalizeTerminalEnv(null), overrides);
    options.onOverridesChange({ ...overrides });
    render();
  };

  const resetOverrides = (): void => {
    overrides = {};
    options.onOverridesChange({});
    render();
  };

  const makeDefault = async (runtime: TerminalRuntime, version: string): Promise<void> => {
    try {
      const response = await ctx.api.patch<unknown>(TERMINAL_ENV_PATH, {
        defaults: { [runtime.id]: version },
      });
      env = normalizeTerminalEnv(response);
      options.onEnvUpdated(env);
      // A pick identical to the new default is not an override any more.
      if (overrides[runtime.id] === (env.defaults[runtime.id] ?? env.runtimes.find((entry) => entry.id === runtime.id)?.default)) {
        delete overrides[runtime.id];
        options.onOverridesChange({ ...overrides });
      }
      ctx.toast.success(`${runtime.name} ${version} is now your default`);
      render();
    } catch (error) {
      ctx.toast.error(`Could not save the default: ${errorMessage(error)}`);
    }
  };

  const renderRuntime = (runtime: TerminalRuntime): HTMLElement => {
    const versions = runtimeVersionChoices(runtime);
    const effective = env ? effectiveRuntimeVersion(env, runtime) : runtime.current;
    const userDefault = env?.defaults[runtime.id] ?? null;
    const selected = overrides[runtime.id] ?? "";
    const chosen = selected || effective;

    const head = h(
      "div",
      { class: "term-runtime-head" },
      h("span", { class: "term-runtime-name" }, runtime.name),
      h("span", { class: `badge ${runtime.source === "system" ? "" : "info"}` }, runtimeSourceLabel(runtime.source)),
      runtimeHasProblems(runtime) ? h("span", { class: "badge error" }, "missing dependencies") : null,
      h("span", { class: "term-runtime-id mono small muted" }, runtime.id),
    );

    const facts = h("div", { class: "term-runtime-sub" });
    if (runtime.current) facts.appendChild(h("span", {}, `Current: ${runtime.current}`));
    if (effective) facts.appendChild(h("span", {}, `New tabs: ${effective}`));
    if (userDefault) facts.appendChild(h("span", {}, `My default: ${userDefault}`));
    else if (runtime.default) facts.appendChild(h("span", {}, `RAST default: ${runtime.default}`));
    if (runtime.home) facts.appendChild(h("span", { class: "mono term-runtime-home", title: runtime.home }, runtime.home));

    const requirements = h("div", { class: "term-runtime-req" });
    for (const requirement of runtime.requirements) {
      requirements.appendChild(h("span", { class: "badge" }, requirement));
    }
    for (const missing of runtime.missing) {
      requirements.appendChild(h("span", { class: "badge error" }, `missing: ${missing}`));
    }
    if (runtime.binDirs.length > 0) {
      requirements.appendChild(
        h("span", { class: "term-runtime-note", title: runtime.binDirs.join("\n") }, `bin dirs: ${runtime.binDirs.length}`),
      );
    }

    const select = h("select", { class: "select term-runtime-select", ariaLabel: `Runtime version for new tabs (${runtime.name})` });
    const defaultOption = h("option", { value: "" }, effective ? `Use my default (${effective})` : "Use my default");
    select.appendChild(defaultOption);
    for (const version of versions) {
      const option = h("option", { value: version }, version);
      if (version === selected) option.selected = true;
      select.appendChild(option);
    }
    if (!selected) defaultOption.selected = true;
    select.addEventListener("change", () => setOverride(runtime.id, select.value));

    const defaultButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button" },
      iconSvg("check", 13),
      h("span", {}, chosen ? `Set ${chosen} as my default` : "Set as my default"),
    );
    const alreadyDefault = Boolean(chosen) && ((userDefault !== null && userDefault === chosen) || (!userDefault && chosen === runtime.default && runtime.default !== null));
    defaultButton.disabled = !chosen || alreadyDefault;
    if (alreadyDefault) defaultButton.title = "This is already your default version";
    defaultButton.addEventListener("click", () => {
      if (chosen) void makeDefault(runtime, chosen);
    });

    const actions = h(
      "div",
      { class: "term-runtime-actions" },
      h("label", { class: "term-runtime-field" }, h("span", { class: "small muted" }, "Next tabs:"), select),
      defaultButton,
    );

    const card = h("div", { class: "term-runtime" }, head, facts, requirements, actions);
    if (runtime.env && Object.keys(runtime.env).length > 0) {
      const envLine = Object.entries(runtime.env)
        .map(([key, value]) => `${key}=${value}`)
        .join("  ");
      card.appendChild(h("div", { class: "term-runtime-env mono small muted" }, envLine));
    }
    return card;
  };

  const render = (): void => {
    clear(body);
    if (!env) {
      body.append(
        banner(
          "error",
          "Runtime environment unavailable",
          options.error ? errorMessage(options.error) : "The server did not return the terminal environment.",
        ),
        h("p", { class: "small muted" }, "Close this dialog and retry from the Terminal toolbar once the server is reachable."),
      );
      return;
    }
    if (options.error) {
      body.appendChild(banner("warning", "Could not refresh the runtime environment", errorMessage(options.error)));
    }
    if (!env.enabled) {
      body.appendChild(banner("warning", "Terminal is disabled by the administrator", "Enable the app for this account to start terminals."));
      return;
    }
    body.appendChild(
      h(
        "div",
        { class: "term-runtime-overview" },
        h("div", { class: "term-runtime-overview-line" }, h("strong", {}, "Shell: "), h("span", { class: "mono" }, env.shell ?? "server login shell"), env.fallbackShell ? h("span", { class: "small muted" }, ` · fallback ${env.fallbackShell}`) : null),
        h("div", { class: "term-runtime-overview-line" }, h("strong", {}, "New tabs use: "), h("span", { class: "small muted" }, "the versions selected here, otherwise your defaults.")),
        h("div", { class: "term-runtime-overview-line" }, sessionsLine),
      ),
    );
    if (env.pathPreview.length > 0) {
      const details = h(
        "details",
        { class: "term-path-preview" },
        h("summary", { class: "small" }, "Terminal PATH"),
        h("div", { class: "term-path-preview-list mono small" }, env.pathPreview.join(":")),
      );
      body.appendChild(details);
    }
    if (env.runtimes.length === 0) {
      body.appendChild(
        h("p", { class: "small muted" }, "No managed runtimes are installed. Terminals run the server's login shell with its own PATH."),
      );
    } else {
      const list = h("div", { class: "term-runtimes" }, ...env.runtimes.map(renderRuntime));
      body.appendChild(list);
    }
    body.appendChild(
      h(
        "div",
        { class: "term-runtime-footer" },
        h(
          "p",
          { class: "term-runtime-note small muted" },
          "Existing tabs must be reopened to use a different runtime environment: a running shell keeps the environment it was started with.",
        ),
        h(
          "button",
          { class: "btn btn-ghost btn-sm", type: "button" },
          iconSvg("refresh", 13),
          h("span", {}, "Reset picks to my defaults"),
        ),
      ),
    );
    const reset = body.querySelector<HTMLButtonElement>(".term-runtime-footer .btn");
    reset?.addEventListener("click", resetOverrides);
    renderSessions();
  };

  render();
  refreshSessions();

  modal({
    title: "Runtime environment",
    body,
    wide: true,
    actions: [{ label: "Close", variant: "ghost", onClick: () => undefined }],
    onClose: () => undefined,
  });
}
