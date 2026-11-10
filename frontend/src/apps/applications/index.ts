// Applications: Docker status, managed app lifecycle, deploy dialog, logs,
// deployment history and explicit import of discovered containers.
// OWNER: teammate `frontend-apps`.

import type { AppDefinition, AppContext } from "../../core/app";
import { h, clear } from "../../core/dom";
import { formatBytes, formatPercent, formatDateTime, relativeTime, formatTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type {
  AppEnvVar,
  ContainerInfo,
  DockerStatus,
  ManagedApp,
} from "../../core/types";
import { confirmDialog, errorMessage, modal } from "../../core/ui";
import { AppLifecycle, isAbortError } from "../../components/lifecycle";
import { watchJob, jobProgressFraction, jobProgressLabel } from "../../components/jobs";
import {
  banner,
  emptyBlock,
  formError,
  isUnavailableError,
  progressBar,
  renderLoadFailure,
  unavailableReason,
} from "../../components/states";

// ---------------------------------------------------------------------------
// API payload shapes (docs/API.md).
// ---------------------------------------------------------------------------

interface ContainerResourceStat {
  name?: string;
  cpuPercent?: number;
  memoryBytes?: number;
  memoryLimitBytes?: number;
}

interface PortMappingLike {
  hostPort?: number;
  containerPort?: number;
  protocol?: string;
  hostIp?: string | null;
}

interface AppDetail extends ManagedApp {
  description?: string;
  /** The backend returns one resource sample per container. */
  resources?: ContainerResourceStat[];
  compose?: string;
}

interface DeployResponse {
  app?: ManagedApp;
  jobId?: string;
}

interface LogLine {
  ts: number;
  stream: string;
  message: string;
}

interface LogsResponse {
  lines?: LogLine[];
}

interface DeploymentRecord {
  id?: number | string;
  action: string;
  status: string;
  detail: string;
  createdAt: number;
  finishedAt?: number | null;
}

interface HistoryResponse {
  items?: DeploymentRecord[];
  history?: DeploymentRecord[];
}

interface ActionResponse {
  jobId?: string;
  app?: ManagedApp;
}

type DetailTab = "overview" | "logs" | "history";
type AppAction = "start" | "stop" | "restart" | "redeploy";

const TAIL_OPTIONS = [200, 500, 1000];
const MAX_APP_ROWS = 200;
const MAX_CONTAINER_ROWS = 100;

// ---------------------------------------------------------------------------
// Application wrapper
// ---------------------------------------------------------------------------

class ApplicationsApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly bannerHost: HTMLElement;
  private readonly appsHost: HTMLElement;
  private readonly containersHost: HTMLElement;
  private readonly statusBadge: HTMLElement;
  private readonly deployButton: HTMLButtonElement;
  private readonly importButton: HTMLButtonElement;
  private readonly refreshButton: HTMLButtonElement;
  private readonly importLabel: HTMLElement;

  private docker: DockerStatus | null = null;
  private dockerError: unknown = null;
  private apps: ManagedApp[] = [];
  private appsLoading = true;
  private appsError: unknown = null;
  private containers: ContainerInfo[] = [];
  private containersLoading = false;
  private containersError: unknown = null;
  private importSelection = new Set<string>();
  private refreshTimer = 0;
  private disposed = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);

    this.statusBadge = h("span", { class: "badge app-docker-badge" }, "Checking Docker…");
    this.deployButton = h(
      "button",
      { class: "btn btn-primary btn-sm", type: "button", disabled: true },
      iconSvg("plus", 14),
      document.createTextNode("Deploy application"),
    );
    this.deployButton.addEventListener("click", () => void this.openDeployDialog());
    this.importLabel = h("span", {}, "Import selected");
    this.importButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", disabled: true },
      iconSvg("download", 14),
      this.importLabel,
    );
    this.importButton.addEventListener("click", () => void this.importSelected());
    this.refreshButton = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("refresh", 14), document.createTextNode("Refresh"));
    this.refreshButton.addEventListener("click", () => void this.refreshAll());

    const toolbar = h(
      "div",
      { class: "app-toolbar", role: "toolbar", ariaLabel: "Application actions" },
      this.statusBadge,
      h("div", { class: "app-toolbar-spacer" }),
      this.refreshButton,
      this.importButton,
      this.deployButton,
    );

    this.bannerHost = h("div", { class: "appmgmt-banners" });
    this.appsHost = h("div", { class: "appmgmt-apps" });
    this.containersHost = h("div", { class: "appmgmt-containers" });

    ctx.root.classList.add("applications-app");
    ctx.root.append(toolbar, h("div", { class: "app-content appmgmt-content" }, this.bannerHost, this.appsHost, this.containersHost));

    document.addEventListener("visibilitychange", this.visibilityHandler);
    const unsubStatus = ctx.events.on("app.status", () => this.scheduleRefresh());
    this.lc.add(() => {
      this.disposed = true;
      document.removeEventListener("visibilitychange", this.visibilityHandler);
      unsubStatus();
    });
    this.lc.setInterval(() => {
      if (document.visibilityState === "visible") this.scheduleRefresh();
    }, 15000);
  }

  private readonly visibilityHandler = (): void => {
    if (document.visibilityState === "visible") this.scheduleRefresh();
  };

  async start(): Promise<void> {
    this.ctx.setTitle("Applications");
    await this.refreshAll();
  }

  private scheduleRefresh(): void {
    if (this.disposed || this.refreshTimer) return;
    this.refreshTimer = this.lc.setTimeout(() => {
      this.refreshTimer = 0;
      void this.refreshAll(true);
    }, 1500);
  }

  private async refreshAll(silent = false): Promise<void> {
    if (silent) {
      await Promise.all([this.loadDockerStatus(), this.loadApps(), this.loadContainers()]);
    } else {
      this.appsLoading = true;
      this.render();
      await Promise.all([this.loadDockerStatus(), this.loadApps(), this.loadContainers()]);
    }
  }

  private async loadDockerStatus(): Promise<void> {
    try {
      const status = await this.ctx.api.get<DockerStatus>("/api/docker/status");
      if (this.disposed) return;
      this.docker = status;
      this.dockerError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.docker = null;
      this.dockerError = error;
    }
    this.render();
  }

  private async loadApps(): Promise<void> {
    try {
      const payload = await this.ctx.api.get<ManagedApp[] | { items?: ManagedApp[] }>("/api/apps");
      if (this.disposed) return;
      this.apps = listOf<ManagedApp>(payload);
      this.appsError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.appsError = error;
      this.apps = [];
    } finally {
      this.appsLoading = false;
      if (!this.disposed) this.render();
    }
  }

  private async loadContainers(): Promise<void> {
    this.containersLoading = true;
    try {
      const payload = await this.ctx.api.get<ContainerInfo[] | { items?: ContainerInfo[]; containers?: ContainerInfo[] }>(
        "/api/docker/containers",
      );
      if (this.disposed) return;
      this.containers = listOf<ContainerInfo>(payload);
      this.containersError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.containers = [];
      this.containersError = error;
    } finally {
      this.containersLoading = false;
      if (!this.disposed) this.render();
    }
  }

  private dockerUsable(): boolean {
    return Boolean(this.docker?.enabled && this.docker?.available);
  }

  // ------------------------------------------------------------- rendering

  private render(): void {
    if (this.disposed) return;
    const usable = this.dockerUsable();
    this.statusBadge.className = `badge app-docker-badge ${usable ? "ok" : this.docker?.enabled === false ? "" : "warn"}`;
    this.statusBadge.textContent = !this.docker
      ? this.dockerError
        ? "Docker status unknown"
        : "Checking Docker…"
      : !this.docker.enabled
        ? "Docker disabled"
        : this.docker.available
          ? `Docker ${this.docker.version ? `v${this.docker.version}` : "available"}`
          : "Docker unavailable";
    this.deployButton.disabled = !usable;
    this.importButton.disabled = this.importSelection.size === 0;
    this.renderBanners(usable);
    this.renderApps(usable);
    this.renderContainers(usable);
  }

  private renderBanners(usable: boolean): void {
    clear(this.bannerHost);
    if (this.dockerError) {
      if (isUnavailableError(this.dockerError)) {
        this.bannerHost.appendChild(
          banner(
            "warning",
            "Docker integration is not available on this server",
            unavailableReason(this.dockerError),
            this.dockerPrerequisites(),
          ),
        );
      } else {
        this.bannerHost.appendChild(
          banner("error", "Could not read Docker status", errorMessage(this.dockerError)),
        );
      }
      return;
    }
    if (!this.docker) return;
    if (!this.docker.enabled) {
      this.bannerHost.appendChild(
        banner(
          "info",
          "Docker integration is disabled",
          "Application deployment is turned off. Enable it with RAST_DOCKER_ENABLED=true and restart the RAST service.",
          this.dockerPrerequisites(),
        ),
      );
      return;
    }
    if (!this.docker.available) {
      this.bannerHost.appendChild(
        banner(
          "warning",
          "Docker worker unavailable",
          this.docker.reason || "The privileged rast-worker service is not responding, so container actions are disabled.",
          this.dockerPrerequisites(),
        ),
      );
      return;
    }
    if (!usable) return;
  }

  private dockerPrerequisites(): HTMLElement {
    const list = h(
      "ul",
      { class: "appmgmt-prereqs small muted" },
      h("li", {}, "RAST_DOCKER_ENABLED=true in /etc/rast/rast.env"),
      h("li", {}, "Docker Engine installed and running on the host"),
      h("li", {}, "rast-worker.service started (systemctl status rast-worker)"),
      h("li", {}, "Worker socket readable by the rast service user"),
    );
    return h("div", { class: "appmgmt-prereq-box" }, h("div", { class: "small" }, "Prerequisites"), list);
  }

  private sectionHeader(title: string, detail?: string, extra?: Node): HTMLElement {
    return h(
      "div",
      { class: "appmgmt-section-head" },
      h("div", { class: "appmgmt-section-titles" }, h("h3", { class: "appmgmt-section-title" }, title), detail ? h("div", { class: "small muted" }, detail) : null),
      extra ?? null,
    );
  }

  private renderApps(usable: boolean): void {
    clear(this.appsHost);
    const count = this.apps.length;
    this.appsHost.appendChild(
      this.sectionHeader(
        count > 0 ? `Managed applications (${count})` : "Managed applications",
        "Deployed through RAST; each application maps to one Docker Compose project.",
      ),
    );

    const host = h("div", { class: "appmgmt-body" });
    if (this.appsError) {
      renderLoadFailure(host, this.appsError, {
        title: "Could not load applications",
        retry: () => void this.loadApps(),
      });
      this.appsHost.appendChild(host);
      return;
    }
    if (this.appsLoading) {
      host.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading applications…"));
      this.appsHost.appendChild(host);
      return;
    }
    if (this.apps.length === 0) {
      const deploy = h("button", { class: "btn btn-primary", type: "button", disabled: !usable }, "Deploy your first application");
      deploy.addEventListener("click", () => void this.openDeployDialog());
      host.appendChild(
        emptyBlock(
          "No managed applications",
          usable
            ? "Deploy a Compose application to get started. RAST validates the Compose file before starting it."
            : "Application deployment is unavailable until the Docker worker prerequisites are met.",
          deploy,
        ),
      );
      this.appsHost.appendChild(host);
      return;
    }

    const shown = this.apps.slice(0, MAX_APP_ROWS);
    const head = h(
      "tr",
      {},
      h("th", {}, "Application"),
      h("th", {}, "Status"),
      h("th", { class: "appmgmt-col-containers" }, "Containers"),
      h("th", { class: "appmgmt-col-ports" }, "Ports"),
      h("th", { class: "appmgmt-col-updated" }, "Updated"),
      h("th", { class: "appmgmt-col-actions" }, "Actions"),
    );
    const body = h("tbody");
    for (const app of shown) body.appendChild(this.renderAppRow(app, usable));
    host.appendChild(h("div", { class: "table-wrap" }, h("table", { class: "table appmgmt-table" }, h("thead", {}, head), body)));
    if (count > shown.length) {
      host.appendChild(h("div", { class: "small muted appmgmt-more" }, `Showing the first ${shown.length} of ${count} applications.`));
    }
    this.appsHost.appendChild(host);
  }

  private renderAppRow(app: ManagedApp, usable: boolean): HTMLTableRowElement {
    const statusBadge = h("span", { class: `badge ${statusClass(app.status)}` }, statusLabel(app.status));
    if (app.lastError) statusBadge.title = app.lastError;
    const containers = app.containers ?? [];
    const running = containers.filter((container) => container.state === "running").length;
    const ports = publishedPorts(app);
    const actions = h("div", { class: "appmgmt-row-actions" });
    const busy = app.status === "deploying";

    const actionButton = (icon: string, label: string, onClick: () => void, danger = false) => {
      const btn = h(
        "button",
        {
          class: `appmgmt-icon-btn${danger ? " is-danger" : ""}`,
          type: "button",
          ariaLabel: label,
          title: label,
          disabled: !usable || busy,
        },
      );
      btn.appendChild(iconSvg(icon, 15));
      btn.addEventListener("click", (event) => {
        event.stopPropagation();
        onClick();
      });
      return btn;
    };
    actions.append(
      actionButton("play", "Start application", () => void this.performAction(app, "start")),
      actionButton("stop", "Stop application", () => void this.performAction(app, "stop")),
      actionButton("restart", "Restart application", () => void this.performAction(app, "restart")),
      actionButton("terminal", "Logs and details", () => void this.openDetail(app.id)),
      actionButton("trash", "Delete application", () => void this.openDeleteDialog(app), true),
    );

    const nameCell = h(
      "div",
      { class: "appmgmt-name-cell" },
      h("div", { class: "appmgmt-name" }, app.name || app.slug),
      h("div", { class: "mono small muted" }, app.slug),
      app.lastError ? h("div", { class: "appmgmt-error small" }, `Last error: ${app.lastError}`) : null,
    );
    const row = h(
      "tr",
      {},
      h("td", {}, nameCell),
      h("td", {}, statusBadge),
      h(
        "td",
        { class: "appmgmt-col-containers small muted" },
        containers.length > 0 ? `${running} / ${containers.length} running` : "—",
      ),
      h(
        "td",
        { class: "appmgmt-col-ports small" },
        ports.length > 0
          ? h("div", { class: "appmgmt-ports" }, ...ports.slice(0, 3).map((port) => h("span", { class: "badge" }, port)), ports.length > 3 ? h("span", { class: "small muted" }, `+${ports.length - 3}`) : null)
          : "—",
      ),
      h("td", { class: "appmgmt-col-updated small muted", title: formatDateTime(app.updatedAt) }, relativeTime(app.updatedAt)),
      h("td", { class: "appmgmt-col-actions" }, actions),
    );
    row.addEventListener("click", () => void this.openDetail(app.id));
    return row;
  }

  private renderContainers(usable: boolean): void {
    clear(this.containersHost);
    const unmanaged = this.containers.filter((container) => !container.managed);
    const selected = [...this.importSelection];
    this.importButton.disabled = selected.length === 0;
    this.importLabel.textContent = selected.length > 0 ? `Import selected (${selected.length})` : "Import selected";
    this.containersHost.appendChild(
      this.sectionHeader(
        "Discovered containers",
        "Containers running on the host. Unmanaged containers are listed but never adopted automatically — import them explicitly.",
      ),
    );
    const host = h("div", { class: "appmgmt-body" });
    if (this.containersError) {
      if (isUnavailableError(this.containersError)) {
        host.appendChild(
          h(
            "div",
            { class: "appmgmt-inline-muted small muted" },
            `Container discovery is unavailable: ${unavailableReason(this.containersError)}`,
          ),
        );
      } else {
        host.appendChild(h("div", { class: "appmgmt-inline-muted small" }, `Could not list containers: ${errorMessage(this.containersError)}`));
      }
      this.containersHost.appendChild(host);
      return;
    }
    if (this.containersLoading) {
      host.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Discovering containers…"));
      this.containersHost.appendChild(host);
      return;
    }
    if (this.containers.length === 0) {
      host.appendChild(
        h(
          "div",
          { class: "appmgmt-inline-muted small muted" },
          usable ? "No containers discovered on this host." : "Container discovery requires a working Docker worker.",
        ),
      );
      this.containersHost.appendChild(host);
      return;
    }
    const shown = this.containers.slice(0, MAX_CONTAINER_ROWS);
    const head = h(
      "tr",
      {},
      h("th", { class: "appmgmt-check" }),
      h("th", {}, "Container"),
      h("th", { class: "appmgmt-col-image" }, "Image"),
      h("th", { class: "appmgmt-col-state" }, "State"),
      h("th", { class: "appmgmt-col-managed" }, "Managed"),
    );
    const body = h("tbody");
    for (const container of shown) {
      const checkbox = h("input", {
        type: "checkbox",
        ariaLabel: `Select ${container.name} for import`,
        disabled: container.managed,
      });
      checkbox.checked = this.importSelection.has(container.id);
      checkbox.addEventListener("change", () => {
        if (checkbox.checked) this.importSelection.add(container.id);
        else this.importSelection.delete(container.id);
        this.renderContainers(usable);
      });
      body.appendChild(
        h(
          "tr",
          { class: container.managed ? "" : "is-unmanaged" },
          h("td", { class: "appmgmt-check" }, checkbox),
          h("td", {}, h("div", { class: "appmgmt-name" }, container.name), h("div", { class: "mono small muted" }, container.id.slice(0, 12))),
          h("td", { class: "appmgmt-col-image small mono" }, container.image),
          h("td", { class: "appmgmt-col-state small" }, container.state),
          h(
            "td",
            { class: "appmgmt-col-managed" },
            h("span", { class: `badge ${container.managed ? "info" : ""}` }, container.managed ? "managed" : "unmanaged"),
          ),
        ),
      );
    }
    host.appendChild(h("div", { class: "table-wrap" }, h("table", { class: "table appmgmt-table" }, h("thead", {}, head), body)));
    if (unmanaged.length > 0) {
      host.appendChild(
        h(
          "div",
          { class: "appmgmt-import-hint small muted" },
          `${unmanaged.length} unmanaged container(s). Tick the ones you want to convert into managed applications, then choose “Import selected”.`,
        ),
      );
    }
    if (this.containers.length > shown.length) {
      host.appendChild(h("div", { class: "small muted appmgmt-more" }, `Showing the first ${shown.length} of ${this.containers.length} containers.`));
    }
    this.containersHost.appendChild(host);
  }

  // --------------------------------------------------------------- actions

  private findApp(id: string): ManagedApp | undefined {
    return this.apps.find((app) => app.id === id);
  }

  private async performAction(app: ManagedApp, action: AppAction): Promise<void> {
    const labels: Record<AppAction, string> = {
      start: "Start",
      stop: "Stop",
      restart: "Restart",
      redeploy: "Redeploy",
    };
    const messages: Record<AppAction, string> = {
      start: `Start “${app.name}”? Docker will start its containers.`,
      stop: `Stop “${app.name}”? Its containers will be stopped; data volumes are kept.`,
      restart: `Restart “${app.name}”? Containers will be stopped and started again.`,
      redeploy: `Redeploy “${app.name}”? Containers are recreated from the stored Compose definition; running services will briefly stop.`,
    };
    const confirmed = await confirmDialog({
      title: `${labels[action]} application`,
      message: messages[action],
      confirmLabel: labels[action],
      danger: action === "stop" || action === "restart" || action === "redeploy",
    });
    if (!confirmed || this.disposed) return;
    try {
      const response = await this.ctx.api.post<ActionResponse>(`/api/apps/${encodeURIComponent(app.id)}/${action}`);
      if (response?.jobId) {
        const outcome = await watchJob(this.ctx, this.lc, response.jobId, () => undefined);
        if (this.disposed || this.ctx.window.isClosed()) return;
        if (outcome.status === "done") this.ctx.toast.success(`${labels[action]} requested`);
        else if (outcome.status === "failed") this.ctx.toast.error(`${labels[action]} failed: ${outcome.error ?? "unknown error"}`);
        else this.ctx.toast.warning(`${labels[action]} cancelled`);
      } else {
        this.ctx.toast.success(`${labels[action]} requested for ${app.name}`);
      }
    } catch (error) {
      this.ctx.toast.error(`${labels[action]} failed: ${errorMessage(error)}`);
    }
    await Promise.all([this.loadApps(), this.loadDockerStatus()]);
  }

  // ---------------------------------------------------------------- deploy

  private async openDeployDialog(): Promise<void> {
    if (!this.dockerUsable()) {
      this.ctx.toast.warning("Application deployment is unavailable: Docker worker is not ready.");
      return;
    }
    const nameInput = h("input", { class: "input", type: "text", placeholder: "my-application", autocomplete: "off" });
    const composeInput = h("textarea", {
      class: "textarea appmgmt-compose",
      placeholder: "services:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"8080:80\"",
    });
    const envRows = h("div", { class: "appmgmt-env-rows" });
    const envList: Array<{ key: HTMLInputElement; value: HTMLInputElement; secret: HTMLInputElement; root: HTMLElement }> = [];
    const validation = formError("");
    const submitProgress = progressBar();
    submitProgress.el.classList.add("hidden");
    const submitStatus = h("div", { class: "small muted" });
    const addRow = (initial?: AppEnvVar) => {
      const key = h("input", { class: "input appmgmt-env-key", type: "text", placeholder: "KEY", value: initial?.key ?? "" });
      const value = h("input", { class: "input appmgmt-env-value", type: "text", placeholder: "value", value: initial?.value ?? "" });
      if (initial?.secret) value.type = "password";
      const secret = h("input", { type: "checkbox", ariaLabel: "Secret value" });
      secret.checked = Boolean(initial?.secret);
      secret.addEventListener("change", () => {
        value.type = secret.checked ? "password" : "text";
      });
      const remove = h("button", { class: "appmgmt-icon-btn is-danger", type: "button", ariaLabel: "Remove variable", title: "Remove variable" });
      remove.appendChild(iconSvg("close", 14));
      const root = h(
        "div",
        { class: "appmgmt-env-row" },
        key,
        value,
        h("label", { class: "checkbox appmgmt-secret-label", title: "Encrypt and mask this value at rest" }, secret, "secret"),
        remove,
      );
      remove.addEventListener("click", () => {
        const index = envList.findIndex((row) => row.root === root);
        if (index >= 0) envList.splice(index, 1);
        root.remove();
      });
      envList.push({ key, value, secret, root });
      envRows.appendChild(root);
    };
    const addButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, iconSvg("plus", 13), document.createTextNode("Add variable"));
    addButton.addEventListener("click", () => addRow());

    const header = h(
      "div",
      { class: "appmgmt-deploy-head" },
      h("div", { class: "small muted" }, "The application name becomes the Docker Compose project name. The Compose file is validated on the server before anything runs."),
    );
    const body = h(
      "div",
      { class: "col appmgmt-deploy-form" },
      header,
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Application name"), nameInput),
      h(
        "label",
        { class: "field" },
        h("span", { class: "field-label" }, "Compose YAML"),
        composeInput,
        h("span", { class: "field-hint" }, "Must define a services: section. Invalid YAML is rejected by the server with a reason."),
      ),
      h("div", { class: "field" }, h("span", { class: "field-label" }, "Environment variables"), envRows, addButton),
      submitProgress.el,
      submitStatus,
      validation,
    );

    let submitted = false;
    const close = modal({
      title: "Deploy application",
      body,
      wide: true,
      onClose: () => {
        /* nothing to abort: the request completes server-side */
      },
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Deploy",
          variant: "primary",
          keepOpen: true,
          onClick: async (closeDialog) => {
            if (submitted) return;
            validation.textContent = "";
            const name = nameInput.value.trim();
            const compose = composeInput.value;
            const nameError = validateAppName(name);
            if (nameError) {
              validation.textContent = nameError;
              nameInput.focus();
              return;
            }
            const composeError = validateCompose(compose);
            if (composeError) {
              validation.textContent = composeError;
              composeInput.focus();
              return;
            }
            const env: AppEnvVar[] = [];
            const seen = new Set<string>();
            for (const row of envList) {
              const key = row.key.value.trim();
              if (!key && !row.value.value) continue;
              if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
                validation.textContent = `Invalid environment variable name: “${key || "(empty)"}”`;
                row.key.focus();
                return;
              }
              if (seen.has(key)) {
                validation.textContent = `Duplicate environment variable: ${key}`;
                row.key.focus();
                return;
              }
              seen.add(key);
              env.push({ key, value: row.value.value, secret: row.secret.checked });
            }
            submitted = true;
            submitProgress.el.classList.remove("hidden");
            submitProgress.update(0.08);
            submitStatus.textContent = "Validating Compose file and creating the project…";
            try {
              const response = await this.ctx.api.post<DeployResponse>("/api/apps", { name, compose, env });
              if (response?.jobId) {
                submitStatus.textContent = "Deployment queued…";
                const outcome = await watchJob(this.ctx, this.lc, response.jobId, (update) => {
                  submitProgress.update(jobProgressFraction(update));
                  submitStatus.textContent = jobProgressLabel(update) || "Deploying…";
                });
                if (outcome.status === "done") {
                  submitProgress.update(1);
                  this.ctx.toast.success(`Application “${name}” deployed`);
                  closeDialog();
                  await Promise.all([this.loadApps(), this.loadContainers()]);
                  return;
                }
                submitProgress.setError(true);
                validation.textContent =
                  outcome.status === "failed" ? `Deployment failed: ${outcome.error ?? "unknown error"}` : "Deployment was cancelled.";
                submitted = false;
                return;
              }
              submitProgress.update(1);
              this.ctx.toast.success(`Application “${name}” deployed`);
              closeDialog();
              await Promise.all([this.loadApps(), this.loadContainers()]);
            } catch (error) {
              submitProgress.setError(true);
              validation.textContent = `Deployment failed: ${errorMessage(error)}`;
              submitted = false;
            }
          },
        },
      ],
    });
    void close;
    window.setTimeout(() => nameInput.focus(), 40);
  }

  // --------------------------------------------------------------- details

  private async openDetail(appId: string): Promise<void> {
    const summary = this.findApp(appId);
    if (!summary) return;
    const localCleanups: Array<() => void> = [];
    let tab: DetailTab = "overview";
    let logsTail = 200;
    let autoRefresh = true;
    let detail: AppDetail = { ...summary };
    let logsController: AbortController | null = null;
    let detailController: AbortController | null = null;
    let historyLoaded = false;

    const statusLine = h("div", { class: "appmgmt-detail-status row wrap" });
    const overviewHost = h("div", { class: "appmgmt-tab-body" });
    const logsHost = h("div", { class: "appmgmt-tab-body" });
    const historyHost = h("div", { class: "appmgmt-tab-body" });
    const tabs: Array<{ id: DetailTab; label: string }> = [
      { id: "overview", label: "Overview" },
      { id: "logs", label: "Logs" },
      { id: "history", label: "History" },
    ];
    const tabBar = h("div", { class: "tabs appmgmt-detail-tabs" });
    const actionRow = h("div", { class: "appmgmt-detail-actions" });
    const refreshButton = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("refresh", 13), document.createTextNode("Refresh"));
    refreshButton.addEventListener("click", () => void refreshDetail());
    actionRow.append(refreshButton);
    const actionButton = (icon: string, label: string, variant: "ghost" | "danger", onClick: () => void) => {
      const btn = h("button", { class: `btn btn-${variant} btn-sm`, type: "button" }, iconSvg(icon, 13), document.createTextNode(label));
      btn.addEventListener("click", onClick);
      return btn;
    };
    actionRow.append(
      actionButton("play", "Start", "ghost", () => void this.performAction(detail, "start")),
      actionButton("stop", "Stop", "ghost", () => void this.performAction(detail, "stop")),
      actionButton("restart", "Restart", "ghost", () => void this.performAction(detail, "restart")),
      actionButton("refresh", "Redeploy", "ghost", () => void this.performAction(detail, "redeploy")),
      actionButton("trash", "Delete", "danger", () => {
        close();
        void this.openDeleteDialog(detail);
      }),
    );
    const body = h(
      "div",
      { class: "appmgmt-detail" },
      statusLine,
      actionRow,
      tabBar,
      overviewHost,
      logsHost,
      historyHost,
    );

    const setTab = (next: DetailTab) => {
      tab = next;
      for (const button of Array.from(tabBar.children)) {
        const el = button as HTMLElement;
        el.classList.toggle("is-active", el.dataset.tab === next);
      }
      overviewHost.classList.toggle("hidden", next !== "overview");
      logsHost.classList.toggle("hidden", next !== "logs");
      historyHost.classList.toggle("hidden", next !== "history");
      if (next === "logs") void loadLogs();
      if (next === "history") void loadHistory();
    };
    for (const entry of tabs) {
      const btn = h("button", { class: `tab${entry.id === tab ? " is-active" : ""}`, type: "button", dataset: { tab: entry.id } }, entry.label);
      btn.addEventListener("click", () => setTab(entry.id));
      tabBar.appendChild(btn);
    }

    const renderStatus = () => {
      clear(statusLine);
      statusLine.append(
        h("span", { class: `badge ${statusClass(detail.status)}` }, statusLabel(detail.status)),
        h("span", { class: "mono small muted" }, detail.slug),
        h("span", { class: "small muted" }, `Updated ${relativeTime(detail.updatedAt)}`),
      );
      if (detail.lastError) statusLine.appendChild(h("span", { class: "badge error" }, "last error"));
    };

    const renderOverview = () => {
      clear(overviewHost);
      if (detail.lastError) {
        overviewHost.appendChild(banner("error", "Last deployment error", detail.lastError));
      }
      const kv = h("dl", { class: "kv appmgmt-kv" });
      kv.append(
        h("dt", {}, "Status"),
        h("dd", {}, statusLabel(detail.status)),
        h("dt", {}, "Created"),
        h("dd", {}, formatDateTime(detail.createdAt)),
        h("dt", {}, "Updated"),
        h("dd", {}, formatDateTime(detail.updatedAt)),
      );
      overviewHost.appendChild(h("div", { class: "appmgmt-panel" }, kv));

      const resources = Array.isArray(detail.resources) ? detail.resources : [];
      if (resources.length > 0) {
        overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Resource usage"));
        const resourceRows = resources.map((resource) =>
          h(
            "tr",
            {},
            h("td", { class: "mono small" }, resource.name ?? "—"),
            h("td", { class: "num" }, formatPercent(resource.cpuPercent, 1)),
            h("td", { class: "num" }, formatBytes(resource.memoryBytes)),
            h("td", { class: "num small muted" }, formatBytes(resource.memoryLimitBytes)),
          ),
        );
        overviewHost.appendChild(
          h(
            "div",
            { class: "table-wrap appmgmt-mini-table" },
            h(
              "table",
              { class: "table" },
              h("thead", {}, h("tr", {}, h("th", {}, "Container"), h("th", { class: "num" }, "CPU"), h("th", { class: "num" }, "Memory"), h("th", { class: "num" }, "Limit"))),
              h("tbody", {}, ...resourceRows),
            ),
          ),
        );
      }

      const containers = detail.containers ?? [];
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, `Containers (${containers.length})`));
      if (containers.length === 0) {
        overviewHost.appendChild(h("div", { class: "small muted" }, "No containers reported. The application may not be deployed yet."));
      } else {
        const rows = containers.map((container) =>
          h(
            "tr",
            {},
            h("td", {}, container.name),
            h("td", { class: "mono small" }, container.image),
            h("td", {}, h("span", { class: `badge ${container.state === "running" ? "ok" : ""}` }, container.state)),
            h("td", { class: "small muted" }, portLabels(container.ports).join(", ") || "—"),
          ),
        );
        overviewHost.appendChild(
          h(
            "div",
            { class: "table-wrap appmgmt-mini-table" },
            h(
              "table",
              { class: "table" },
              h("thead", {}, h("tr", {}, h("th", {}, "Name"), h("th", {}, "Image"), h("th", {}, "State"), h("th", {}, "Ports"))),
              h("tbody", {}, ...rows),
            ),
          ),
        );
      }

      const ports = publishedPorts(detail);
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Published ports"));
      overviewHost.appendChild(
        ports.length > 0
          ? h("div", { class: "appmgmt-chips" }, ...ports.map((port) => h("span", { class: "badge" }, port)))
          : h("div", { class: "small muted" }, "No published ports."),
      );

      const volumes = detail.volumes ?? [];
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Volumes"));
      overviewHost.appendChild(
        volumes.length > 0
          ? h(
              "ul",
              { class: "appmgmt-plain-list small" },
              ...volumes.slice(0, 50).map((volume) => h("li", { class: "mono" }, volume)),
            )
          : h("div", { class: "small muted" }, "No volumes declared."),
      );

      const env = detail.env ?? [];
      if (env.length > 0) {
        overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Environment"));
        const envRowsEl = h("div", { class: "appmgmt-env-view" });
        for (const row of env.slice(0, 100)) {
          envRowsEl.appendChild(
            h(
              "div",
              { class: "appmgmt-env-view-row" },
              h("span", { class: "mono" }, row.key),
              h("span", { class: "mono muted" }, row.secret ? "•••••• (secret)" : row.value),
            ),
          );
        }
        overviewHost.appendChild(envRowsEl);
      }
    };

    const loadLogs = async (): Promise<void> => {
      logsController?.abort();
      logsController = this.lc.trackController(new AbortController());
      const request = logsController;
      clear(logsHost);
      const controls = h("div", { class: "appmgmt-log-controls row wrap" });
      const tailSelect = h("select", { class: "select appmgmt-tail-select", ariaLabel: "Log tail length" });
      for (const size of TAIL_OPTIONS) {
        const option = h("option", { value: String(size) }, `${size} lines`);
        if (size === logsTail) option.selected = true;
        tailSelect.appendChild(option);
      }
      tailSelect.addEventListener("change", () => {
        logsTail = Number(tailSelect.value) || 200;
        void loadLogs();
      });
      const autoLabel = h("label", { class: "checkbox small" });
      const autoBox = h("input", { type: "checkbox" });
      autoBox.checked = autoRefresh;
      autoBox.addEventListener("change", () => {
        autoRefresh = autoBox.checked;
      });
      autoLabel.append(autoBox, h("span", {}, "Auto-refresh (5 s)"));
      const download = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("download", 13), document.createTextNode("Download logs"));
      const pre = h("pre", { class: "appmgmt-logs mono" });
      pre.textContent = "Loading logs…";
      download.addEventListener("click", () => {
        const blob = new Blob([pre.textContent ?? ""], { type: "text/plain;charset=utf-8" });
        const url = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = `${detail.slug || "application"}-logs.txt`;
        anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 4000);
      });
      controls.append(tailSelect, autoLabel, download);
      logsHost.append(controls, pre);
      try {
        const payload = await this.ctx.api.get<LogsResponse>(
          `/api/apps/${encodeURIComponent(detail.id)}/logs`,
          { tail: logsTail },
          { signal: request.signal },
        );
        if (request.signal.aborted) return;
        const lines = Array.isArray(payload?.lines) ? payload.lines : [];
        pre.textContent =
          lines.length > 0
            ? lines
                .map((line) => `${formatTime(line.ts)} ${String(line.stream || "out").slice(0, 5).padEnd(5)} ${line.message}`)
                .join("\n")
            : "(no log output)";
        pre.scrollTop = pre.scrollHeight;
      } catch (error) {
        if (isAbortError(error) || request.signal.aborted) return;
        pre.textContent = isUnavailableError(error)
          ? `Application logs are not available: ${unavailableReason(error)}`
          : `Could not load logs: ${errorMessage(error)}`;
      } finally {
        this.lc.releaseController(request);
      }
    };

    const loadHistory = async (): Promise<void> => {
      if (historyLoaded) return;
      historyLoaded = true;
      clear(historyHost);
      historyHost.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading deployment history…"));
      try {
        const payload = await this.ctx.api.get<HistoryResponse | DeploymentRecord[]>(
          `/api/apps/${encodeURIComponent(detail.id)}/history`,
        );
        const records = listOf<DeploymentRecord>(payload, "history");
        clear(historyHost);
        if (records.length === 0) {
          historyHost.appendChild(h("div", { class: "small muted" }, "No deployments recorded yet."));
          return;
        }
        const timeline = h("ol", { class: "appmgmt-timeline" });
        for (const record of records.slice(0, 100)) {
          timeline.appendChild(
            h(
              "li",
              { class: `appmgmt-timeline-item is-${recordStatusClass(record.status)}` },
              h("div", { class: "appmgmt-timeline-dot" }),
              h(
                "div",
                { class: "appmgmt-timeline-body" },
                h(
                  "div",
                  { class: "appmgmt-timeline-head" },
                  h("span", { class: "appmgmt-timeline-action" }, record.action || "deployment"),
                  h("span", { class: `badge ${statusClass(record.status)}` }, record.status),
                  h("span", { class: "small muted" }, formatDateTime(record.createdAt)),
                ),
                record.detail ? h("div", { class: "small muted" }, record.detail) : null,
              ),
            ),
          );
        }
        historyHost.appendChild(timeline);
      } catch (error) {
        clear(historyHost);
        renderLoadFailure(historyHost, error, {
          title: "Could not load deployment history",
          retry: () => {
            historyLoaded = false;
            void loadHistory();
          },
        });
      }
    };

    const refreshDetail = async (): Promise<void> => {
      detailController?.abort();
      detailController = this.lc.trackController(new AbortController());
      const request = detailController;
      try {
        const fresh = await this.ctx.api.get<AppDetail>(`/api/apps/${encodeURIComponent(appId)}`, undefined, {
          signal: request.signal,
        });
        if (request.signal.aborted) return;
        detail = { ...detail, ...fresh };
      } catch (error) {
        if (isAbortError(error) || request.signal.aborted) return;
        if (isUnavailableError(error)) {
          statusLine.appendChild(h("span", { class: "badge warn" }, unavailableReason(error)));
          return;
        }
      } finally {
        this.lc.releaseController(request);
      }
      renderStatus();
      if (tab === "overview") renderOverview();
    };

    renderStatus();
    renderOverview();

    const interval = window.setInterval(() => {
      if (document.visibilityState === "hidden") return;
      void refreshDetail();
      if (tab === "logs" && autoRefresh) void loadLogs();
    }, 5000);
    localCleanups.push(() => window.clearInterval(interval));
    const unsub = this.ctx.events.on("app.status", () => {
      if (detail.id) void refreshDetail();
    });
    localCleanups.push(unsub);

    const close = modal({
      title: `Application — ${detail.name || detail.slug}`,
      body,
      wide: true,
      onClose: () => {
        for (const cleanup of localCleanups) {
          try {
            cleanup();
          } catch {
            /* ignore */
          }
        }
        localCleanups.length = 0;
        logsController?.abort();
        detailController?.abort();
      },
    });
    this.lc.add(close);
    setTab("overview");
  }

  // ----------------------------------------------------------------- delete

  private async openDeleteDialog(app: ManagedApp): Promise<void> {
    let removeVolumes = false;
    const typed = h("input", { class: "input", type: "text", placeholder: app.name, autocomplete: "off" });
    const validation = formError("");
    const keepRadio = h("input", { type: "radio", name: `delete-mode-${app.id}` });
    const purgeRadio = h("input", { type: "radio", name: `delete-mode-${app.id}` });
    keepRadio.checked = true;
    keepRadio.addEventListener("change", () => {
      removeVolumes = false;
    });
    purgeRadio.addEventListener("change", () => {
      removeVolumes = true;
    });
    const body = h(
      "div",
      { class: "col" },
      h("p", {}, `Delete the managed application “${app.name}”? This removes its containers and RAST configuration.`),
      h(
        "label",
        { class: "appmgmt-delete-option" },
        keepRadio,
        h(
          "span",
          {},
          h("span", { class: "appmgmt-delete-option-title" }, "Keep data (recommended)"),
          h("span", { class: "small muted" }, "Container volumes and bind-mount data stay on disk."),
        ),
      ),
      h(
        "label",
        { class: "appmgmt-delete-option is-danger" },
        purgeRadio,
        h(
          "span",
          {},
          h("span", { class: "appmgmt-delete-option-title" }, "Remove volumes"),
          h("span", { class: "small muted" }, "Deletes the application's Docker volumes. This cannot be undone."),
        ),
      ),
      h("p", { class: "small muted" }, `Type “${app.name}” to confirm.`),
      typed,
      validation,
    );
    modal({
      title: "Delete application",
      body,
      onClose: () => undefined,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Delete application",
          variant: "danger",
          keepOpen: true,
          onClick: async (closeDialog) => {
            validation.textContent = "";
            if (typed.value.trim() !== app.name) {
              validation.textContent = `Type “${app.name}” exactly to confirm.`;
              typed.focus();
              return;
            }
            try {
              await this.ctx.api.del(`/api/apps/${encodeURIComponent(app.id)}`, {
                removeVolumes: removeVolumes ? "true" : "false",
              });
              this.ctx.toast.success(`Application “${app.name}” deleted${removeVolumes ? " (volumes removed)" : " (volumes kept)"}`);
              closeDialog();
              await Promise.all([this.loadApps(), this.loadContainers()]);
            } catch (error) {
              validation.textContent = errorMessage(error);
            }
          },
        },
      ],
    });
  }

  // ----------------------------------------------------------------- import

  private async importSelected(): Promise<void> {
    const ids = [...this.importSelection];
    if (ids.length === 0) return;
    const confirmed = await confirmDialog({
      title: "Import discovered containers",
      message: `Import ${ids.length} container(s) into RAST management? Only the selected containers are adopted; nothing else is touched.`,
      confirmLabel: "Import selected",
    });
    if (!confirmed) return;
    try {
      await this.ctx.api.post("/api/docker/import", { containerIds: ids });
      this.ctx.toast.success(`${ids.length} container(s) imported`);
      this.importSelection.clear();
      await Promise.all([this.loadApps(), this.loadContainers()]);
    } catch (error) {
      this.ctx.toast.error(`Import failed: ${errorMessage(error)}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function listOf<T>(payload: unknown, key = "items"): T[] {
  if (Array.isArray(payload)) return payload as T[];
  if (payload && typeof payload === "object") {
    const record = payload as Record<string, unknown>;
    const candidate = record[key] ?? record.items ?? record.containers;
    if (Array.isArray(candidate)) return candidate as T[];
  }
  return [];
}

function portLabel(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (value && typeof value === "object") {
    const mapping = value as PortMappingLike;
    if (typeof mapping.containerPort !== "number") return null;
    const protocol = mapping.protocol && mapping.protocol !== "tcp" ? `/${mapping.protocol}` : "";
    if (typeof mapping.hostPort === "number") return `${mapping.hostPort}:${mapping.containerPort}${protocol}`;
    return `${mapping.containerPort}${protocol}`;
  }
  return null;
}

function portLabels(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  const out: string[] = [];
  for (const value of values) {
    const label = portLabel(value);
    if (label && !out.includes(label)) out.push(label);
  }
  return out;
}

/** Published ports for an app: top-level list when present, else container mappings. */
function publishedPorts(app: { ports?: unknown; containers?: Array<{ ports?: unknown }> }): string[] {
  const direct = portLabels(app.ports);
  if (direct.length > 0) return direct;
  const out: string[] = [];
  for (const container of app.containers ?? []) {
    for (const label of portLabels(container.ports)) {
      if (!out.includes(label)) out.push(label);
    }
  }
  return out;
}

function statusLabel(status: string): string {
  switch (status) {
    case "running":
      return "running";
    case "stopped":
      return "stopped";
    case "partial":
      return "partially running";
    case "deploying":
      return "deploying";
    case "error":
      return "error";
    default:
      return status || "unknown";
  }
}

function statusClass(status: string): string {
  switch (status) {
    case "running":
    case "done":
      return "ok";
    case "partial":
    case "deploying":
    case "pending":
    case "running_deployment":
      return "warn";
    case "error":
    case "failed":
      return "error";
    case "stopped":
      return "";
    default:
      return "";
  }
}

function recordStatusClass(status: string): string {
  return status === "failed" || status === "error" ? "error" : status === "running" ? "warn" : "ok";
}

function validateAppName(name: string): string | null {
  if (!name) return "Application name is required";
  if (name.length > 63) return "Application name must be 63 characters or fewer";
  if (!/^[a-z0-9][a-z0-9_-]*$/.test(name)) {
    return "Use lowercase letters, digits, dashes and underscores; must start with a letter or digit";
  }
  return null;
}

function validateCompose(compose: string): string | null {
  const text = compose.trim();
  if (!text) return "Compose YAML is required";
  if (!/^\s*services\s*:/m.test(text)) return "Compose YAML must define a services: section";
  if (/\t/.test(text)) return "Compose YAML must not contain tab characters — use spaces for indentation";
  return null;
}

// ---------------------------------------------------------------------------

export const applicationsApp: AppDefinition = {
  id: "applications",
  name: "Applications",
  icon: "apps",
  description: "Deploy and manage containerised applications",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1040, h: 680 },
  minSize: { w: 380, h: 320 },
  async mount(ctx: AppContext) {
    const app = new ApplicationsApp(ctx);
    await app.start();
  },
};
