// Install from GitHub: step 1 resolves a repository/release link to one
// release and validates its manifest (server-side); step 2 approves the source
// (first time) and installs exactly that release, pinned by digest.

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { formatBytes } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { errorMessage, modal } from "../../core/ui";
import { banner } from "../../components/states";
import { shortSha } from "./format";
import type { Check, Preview } from "./types";

export interface ImportCallbacks {
  onStarted(jobId: string, packageId: string, version: string): void;
}

export function openGithubImport(ctx: AppContext, callbacks: ImportCallbacks): void {
  const input = h("input", {
    class: "input pkg-import-input",
    type: "url",
    placeholder: "https://github.com/owner/repo/releases/tag/v1.0.0",
    ariaLabel: "GitHub repository or release link",
    autocomplete: "off",
    spellcheck: false,
    dataset: { autofocus: "true" },
  });
  const inspectBtn = h("button", { class: "btn btn-primary", type: "button" }, "Inspect");
  const error = h("div", { class: "form-error hidden", role: "alert" });
  const preview = h("div", { class: "pkg-import-preview" });
  const body = h(
    "div",
    { class: "col pkg-import" },
    h(
      "p",
      { class: "small muted" },
      "Paste a repository or release link. RAST resolves it to one release, reads its rast-package.json and checks every artifact against the release assets before anything is downloaded.",
    ),
    h("div", { class: "pkg-import-row" }, input, inspectBtn),
    error,
    preview,
  );
  let close: () => void = () => {};
  let current: Preview | null = null;

  const showError = (message: string) => {
    error.textContent = message;
    error.classList.remove("hidden");
  };

  const inspect = async () => {
    const url = input.value.trim();
    error.classList.add("hidden");
    if (!url) {
      showError("Enter a GitHub repository or release link.");
      input.focus();
      return;
    }
    inspectBtn.disabled = true;
    inspectBtn.textContent = "Inspecting…";
    clear(preview);
    current = null;
    try {
      current = await ctx.api.post<Preview>("/api/packages/github/inspect", { url });
      renderPreview(current);
    } catch (err) {
      showError(errorMessage(err));
    } finally {
      inspectBtn.disabled = false;
      inspectBtn.textContent = "Inspect";
    }
  };

  const renderPreview = (p: Preview) => {
    clear(preview);
    const m = p.manifest;
    const failed = p.checks.some((c) => c.status === "failed");
    const approve = h("input", { type: "checkbox", class: "checkbox-input" });
    const makeDefault = h("input", { type: "checkbox", class: "checkbox-input", checked: true });
    const installBtn = h("button", { class: "btn btn-primary", type: "button" }, p.sourceApproved ? `Install ${m.version}` : `Approve and install ${m.version}`);
    const blocked = failed || !p.compatible || p.alreadyInstalled;
    const sync = () => {
      installBtn.disabled = blocked || (!p.sourceApproved && !approve.checked);
    };
    approve.addEventListener("change", sync);
    sync();
    installBtn.addEventListener("click", async () => {
      installBtn.disabled = true;
      error.classList.add("hidden");
      try {
        const res = await ctx.api.post<{ jobId: string }>("/api/packages/github/install", {
          inspectionId: p.inspectionId,
          approveSource: approve.checked,
          makeDefault: makeDefault.checked,
        });
        callbacks.onStarted(res.jobId, m.id, m.version);
        ctx.toast.info(`Installing ${m.name} ${m.version} from ${p.repo}…`);
        close();
      } catch (err) {
        showError(errorMessage(err));
        sync();
      }
    });

    const artifact = p.artifact;
    preview.append(
      h(
        "div",
        { class: "pkg-import-card" },
        h(
          "div",
          { class: "pkg-import-head" },
          h("div", {}, h("div", { class: "pkg-name" }, m.name), h("div", { class: "pkg-publisher" }, `${m.publisher.name} (${m.publisher.id})`)),
          h("span", { class: "badge warn pkg-source" }, iconSvg("github", 12), "Third-party"),
        ),
        m.description ? h("p", { class: "pkg-desc pkg-desc-full" }, m.description) : null,
        h(
          "dl",
          { class: "kv pkg-kv" },
          h("dt", {}, "Repository"),
          h("dd", {}, h("a", { href: p.repoUrl, target: "_blank", rel: "noopener noreferrer" }, p.repo)),
          h("dt", {}, "Release"),
          h(
            "dd",
            {},
            h("a", { href: p.release.htmlUrl, target: "_blank", rel: "noopener noreferrer" }, p.release.tag),
            p.release.prerelease ? " · pre-release" : "",
            p.release.publishedAt ? ` · published ${p.release.publishedAt.slice(0, 10)}` : "",
          ),
          h("dt", {}, "Package"),
          h("dd", {}, `${m.id} ${m.version} (revision ${m.revision}) · ${m.type}`),
          h("dt", {}, "Targets"),
          h("dd", {}, m.artifacts.map((a) => (a.target === p.hostTarget ? `${a.target} (this server)` : a.target)).join(", ")),
          h("dt", {}, "Manifest"),
          h("dd", {}, h("code", { title: p.manifestSha256 }, `sha256 ${shortSha(p.manifestSha256)}`)),
          artifact ? h("dt", {}, "Artifact") : null,
          artifact ? h("dd", {}, `${formatBytes(artifact.size)} · `, h("code", { title: artifact.sha256 }, `sha256 ${shortSha(artifact.sha256)}`)) : null,
          m.minRastVersion ? h("dt", {}, "Requires") : null,
          m.minRastVersion ? h("dd", {}, `RAST ${m.minRastVersion} or newer`) : null,
        ),
        h("div", { class: "pkg-checks" }, h("div", { class: "pkg-section-title" }, "Verification"), ...p.checks.map(checkRow)),
      ),
    );
    if (!p.compatible) preview.appendChild(banner("warning", "Not installable on this server", p.reason ?? "Incompatible."));
    if (p.alreadyInstalled) preview.appendChild(banner("info", "Already installed", `${m.name} ${m.version} is already installed.`));
    if (failed) preview.appendChild(banner("error", "Verification failed", "This release cannot be installed. Fix the failed checks and publish a new release."));
    preview.appendChild(
      banner(
        "warning",
        "Third-party package",
        `RAST verifies that the files match this release, but it does not review what ${m.publisher.name} publishes. Installing runs the package's own programs on this server: its version checks run during the install (sandboxed, without network or access to RAST's data), and whatever uses the runtime runs them afterwards. Install only from publishers you trust.`,
      ),
    );
    const options = h("div", { class: "col pkg-import-options" });
    if (!p.sourceApproved && !ctx.store.isAdmin()) {
      // Group package managers install from approved sources only.
      installBtn.disabled = true;
      options.appendChild(
        h("div", { class: "small muted" }, `An administrator must approve ${p.repo} as a package source before it can be installed.`),
      );
      options.appendChild(h("div", { class: "pkg-import-actions" }, installBtn));
      preview.appendChild(options);
      return;
    }
    if (p.sourceApproved) {
      options.appendChild(h("div", { class: "small muted" }, `${p.repo} was approved as a source by ${p.sourceApprovedBy ?? "an administrator"} for publisher ${m.publisher.id}.`));
    } else {
      options.appendChild(
        h("label", { class: "checkbox" }, approve, h("span", {}, `Trust ${p.repo} as a package source for publisher “${m.publisher.id}”`)),
      );
    }
    options.appendChild(h("label", { class: "checkbox" }, makeDefault, h("span", {}, "Make it the RAST default for this package")));
    options.appendChild(h("div", { class: "pkg-import-actions" }, installBtn));
    preview.appendChild(options);
  };

  inspectBtn.addEventListener("click", () => void inspect());
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void inspect();
    }
  });
  close = modal({
    title: "Install from GitHub",
    body,
    wide: true,
    actions: [{ label: "Close", variant: "ghost", onClick: (c) => c() }],
  });
}

function checkRow(c: Check): HTMLElement {
  const icon = c.status === "ok" ? "check" : c.status === "warning" ? "warning" : "close";
  return h(
    "div",
    { class: `pkg-check pkg-check-${c.status}` },
    iconSvg(icon, 14),
    h("div", {}, h("div", { class: "pkg-check-label" }, c.label), c.detail ? h("div", { class: "small muted" }, c.detail) : null),
  );
}
