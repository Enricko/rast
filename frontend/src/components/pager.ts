// Bounded pagination control shared by File Station, Backup Center and the
// recycle bin. Keeps the DOM constant-size regardless of result totals.

import { clear, h } from "../core/dom";
import { iconSvg } from "../core/icons";

export interface PagerOptions {
  onChange: (page: number, perPage: number) => void;
  perPageOptions?: number[];
  label?: string;
}

export class Pager {
  readonly el: HTMLElement;
  private readonly summary: HTMLElement;
  private readonly buttons: HTMLElement;
  private readonly perPageSelect: HTMLSelectElement;
  private readonly options: PagerOptions;
  private readonly perPageChoices: number[];
  private page = 1;
  private perPage = 50;
  private total = 0;

  constructor(options: PagerOptions) {
    this.options = options;
    this.perPageChoices = options.perPageOptions ?? [25, 50, 100];
    this.summary = h("span", { class: "pager-summary muted small" });
    this.buttons = h("div", { class: "pager-buttons" });

    this.perPageSelect = h("select", { class: "select pager-size", ariaLabel: "Items per page" });
    for (const size of this.perPageChoices) {
      const option = h("option", { value: String(size) }, `${size} / page`);
      this.perPageSelect.appendChild(option);
    }
    this.perPageSelect.value = String(this.perPage);
    this.perPageSelect.addEventListener("change", () => {
      const value = Number(this.perPageSelect.value);
      if (!Number.isFinite(value) || value <= 0) return;
      this.perPage = value;
      this.options.onChange(1, this.perPage);
    });

    this.el = h(
      "div",
      { class: "pager", role: "navigation", ariaLabel: options.label ?? "Pagination" },
      this.summary,
      this.buttons,
      this.perPageSelect,
    );
    this.render();
  }

  get currentPage(): number {
    return this.page;
  }

  get currentPerPage(): number {
    return this.perPage;
  }

  set(page: number, perPage: number, total: number): void {
    this.total = Number.isFinite(total) ? Math.max(0, total) : 0;
    this.perPage = Math.max(1, perPage);
    const pages = this.pageCount();
    this.page = Math.min(Math.max(1, Math.trunc(page) || 1), pages);
    if (this.perPageSelect.value !== String(this.perPage)) {
      this.perPageSelect.value = String(this.perPage);
    }
    this.render();
  }

  private pageCount(): number {
    return Math.max(1, Math.ceil(this.total / this.perPage));
  }

  private go(page: number): void {
    const target = Math.min(Math.max(1, page), this.pageCount());
    if (target === this.page) return;
    this.options.onChange(target, this.perPage);
  }

  private render(): void {
    const pages = this.pageCount();
    const first = this.total === 0 ? 0 : (this.page - 1) * this.perPage + 1;
    const last = Math.min(this.total, this.page * this.perPage);
    this.summary.textContent =
      this.total === 0 ? "No items" : `${first}–${last} of ${this.total.toLocaleString()}`;

    clear(this.buttons);
    this.buttons.appendChild(this.navButton("chevronLeft", "Previous page", this.page > 1, () => this.go(this.page - 1)));

    for (const entry of pageWindow(this.page, pages)) {
      if (entry === "gap") {
        this.buttons.appendChild(h("span", { class: "pager-gap", ariaLabel: "More pages" }, "…"));
        continue;
      }
      const btn = h(
        "button",
        {
          class: `pager-page${entry === this.page ? " is-active" : ""}`,
          type: "button",
          ariaLabel: `Page ${entry}`,
        },
        String(entry),
      );
      if (entry === this.page) btn.setAttribute("aria-current", "page");
      btn.addEventListener("click", () => this.go(entry));
      this.buttons.appendChild(btn);
    }

    this.buttons.appendChild(this.navButton("chevronRight", "Next page", this.page < pages, () => this.go(this.page + 1)));
  }

  private navButton(icon: string, label: string, enabled: boolean, action: () => void): HTMLButtonElement {
    const btn = h("button", { class: "pager-nav", type: "button", ariaLabel: label });
    btn.appendChild(iconSvg(icon, 14));
    btn.disabled = !enabled;
    btn.addEventListener("click", action);
    return btn;
  }
}

/** Compact page number window: 1 … 4 5 [6] 7 8 … 20 (bounded to 9 slots). */
function pageWindow(page: number, pages: number): Array<number | "gap"> {
  const entries: Array<number | "gap"> = [];
  const add = (value: number | "gap") => entries.push(value);
  if (pages <= 9) {
    for (let i = 1; i <= pages; i += 1) add(i);
    return entries;
  }
  const start = Math.max(2, Math.min(page - 2, pages - 5));
  const end = Math.min(pages - 1, start + 4);
  add(1);
  if (start > 2) add("gap");
  for (let i = start; i <= end; i += 1) add(i);
  if (end < pages - 1) add("gap");
  add(pages);
  return entries;
}
