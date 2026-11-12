// Folder picker dialog used by File Station copy/move actions. With `files`,
// it picks one matching file instead (App Center uses it for Linux apps).

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import type { FileEntry, FileListing } from "../../core/types";
import { errorMessage, modal } from "../../core/ui";
import { iconSvg } from "../../core/icons";
import { isAbortError, type AppLifecycle } from "../../components/lifecycle";
import { isUnavailableError, unavailableReason, unavailableState } from "../../components/states";

export interface FolderPickerOptions {
  title: string;
  confirmLabel: string;
  startPath: string;
  /** Paths being copied/moved: cannot be used as the destination. */
  exclude?: Set<string>;
  /** Pick a file instead of a folder: the files that may be chosen. */
  files?: (entry: FileEntry) => boolean;
  /** Shown when a folder has nothing to choose (file mode). */
  emptyHint?: string;
}

/** Resolves with the chosen destination directory, or null when cancelled. */
export function pickDestination(
  ctx: AppContext,
  lifecycle: AppLifecycle,
  options: FolderPickerOptions,
): Promise<string | null> {
  return new Promise((resolve) => {
    let chosen: string | null = null;
    let currentPath = options.startPath;
    let selectedFile: string | null = null;
    const fileMode = Boolean(options.files);
    let loading = false;
    let controller = lifecycle.trackController(new AbortController());
    let closed = false;

    const crumbs = h("div", { class: "picker-crumbs" });
    const list = h("div", { class: "picker-list", role: "listbox", ariaLabel: "Folders" });
    const status = h("div", { class: "picker-status small muted" });
    const error = h("div", { class: "form-error", role: "alert" });
    const body = h("div", { class: "col picker-body" }, crumbs, list, status, error);

    const isExcluded = (path: string): boolean => {
      if (!options.exclude) return false;
      for (const excluded of options.exclude) {
        if (path === excluded || path.startsWith(`${excluded}/`)) return true;
      }
      return false;
    };

    const navigate = (path: string): void => {
      currentPath = path;
      selectedFile = null;
      error.textContent = "";
      void load(path);
    };

    const load = async (path: string): Promise<void> => {
      controller.abort();
      controller = lifecycle.trackController(new AbortController());
      const request = controller;
      loading = true;
      clear(list);
      clear(crumbs);
      status.textContent = "Loading folders…";
      const parent = path === "/" ? null : path.slice(0, path.lastIndexOf("/")) || "/";
      const crumbRoot = h("button", { class: "picker-crumb", type: "button" }, "Home");
      crumbRoot.addEventListener("click", () => navigate("/"));
      crumbs.appendChild(crumbRoot);
      if (path !== "/") {
        const parts = path.split("/").filter(Boolean);
        let accumulated = "";
        for (const part of parts) {
          accumulated += `/${part}`;
          const target = accumulated;
          const crumb = h("button", { class: "picker-crumb", type: "button" }, part);
          crumb.addEventListener("click", () => navigate(target));
          crumbs.appendChild(h("span", { class: "picker-crumb-sep" }, "/"));
          crumbs.appendChild(crumb);
        }
      }
      try {
        const listing = await ctx.api.get<FileListing>(
          "/api/files",
          { path, page: 1, perPage: 200, sort: "name", order: "asc" },
          { signal: request.signal },
        );
        if (closed) return;
        loading = false;
        clear(list);
        if (parent && !isExcluded(parent)) {
          list.appendChild(folderRow("..", parent, "Parent folder"));
        }
        const dirs = (Array.isArray(listing?.items) ? listing.items : []).filter(
          (entry: FileEntry) => entry.kind === "dir",
        );
        for (const dir of dirs) {
          if (isExcluded(dir.path)) continue;
          list.appendChild(folderRow(dir.name, dir.path, dir.path));
        }
        let files = 0;
        if (options.files) {
          const all = Array.isArray(listing?.items) ? listing.items : [];
          for (const entry of all) {
            if (entry.kind === "dir" || !options.files(entry)) continue;
            list.appendChild(fileRow(entry));
            files += 1;
          }
        }
        if (fileMode && files === 0) {
          list.appendChild(h("div", { class: "picker-empty small muted" }, options.emptyHint ?? "No matching files in this folder."));
        } else if (list.children.length === 0) {
          list.appendChild(h("div", { class: "picker-empty small muted" }, "No sub-folders here. Use this folder as the destination."));
        }
        status.textContent = fileMode ? "Choose a file." : `Destination: ${path}`;
      } catch (err) {
        if (closed || isAbortError(err)) return;
        loading = false;
        if (isUnavailableError(err)) {
          list.replaceChildren(unavailableState({ title: "Cannot browse folders", reason: unavailableReason(err) }));
        } else {
          list.replaceChildren(
            h("div", { class: "error-state" }, h("div", { class: "error-title" }, "Could not list this folder"), h("div", { class: "error-detail" }, errorMessage(err))),
          );
        }
        status.textContent = fileMode ? "" : `Destination: ${path}`;
      } finally {
        lifecycle.releaseController(request);
      }
    };

    const folderRow = (label: string, path: string, hint: string): HTMLElement => {
      const row = h(
        "button",
        { class: "picker-row", type: "button", role: "option" },
        iconSvg("folder", 18),
        h("span", { class: "picker-row-label" }, label),
        h("span", { class: "picker-row-hint small muted" }, hint),
      );
      row.addEventListener("dblclick", () => navigate(path));
      row.addEventListener("click", () => navigate(path));
      return row;
    };

    let confirmDialog: (() => void) | null = null;
    const fileRow = (entry: FileEntry): HTMLElement => {
      const row = h(
        "button",
        { class: "picker-row is-file", type: "button", role: "option", ariaSelected: false },
        iconSvg("file", 18),
        h("span", { class: "picker-row-label" }, entry.name),
      );
      row.addEventListener("click", () => {
        selectedFile = entry.path;
        error.textContent = "";
        for (const other of list.querySelectorAll(".picker-row.is-selected")) {
          other.classList.remove("is-selected");
          other.setAttribute("aria-selected", "false");
        }
        row.classList.add("is-selected");
        row.setAttribute("aria-selected", "true");
        status.textContent = `Selected: ${entry.name}`;
      });
      row.addEventListener("dblclick", () => {
        selectedFile = entry.path;
        confirmDialog?.();
      });
      return row;
    };

    const closeModal = modal({
      title: options.title,
      body,
      wide: true,
      onClose: () => {
        closed = true;
        controller.abort();
        resolve(chosen);
      },
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: options.confirmLabel,
          variant: "primary",
          keepOpen: true,
          onClick: (closeDialog) => {
            if (loading) {
              error.textContent = "Still loading the folder, please wait.";
              return;
            }
            if (fileMode) {
              if (!selectedFile) {
                error.textContent = "Choose a file first.";
                return;
              }
              chosen = selectedFile;
              closeDialog();
              return;
            }
            if (isExcluded(currentPath)) {
              error.textContent = "The destination cannot be inside the item being copied or moved.";
              return;
            }
            chosen = currentPath;
            closeDialog();
          },
        },
      ],
    });
    confirmDialog = () => {
      if (!selectedFile) return;
      chosen = selectedFile;
      closeModal();
    };
    navigate(options.startPath);
  });
}
