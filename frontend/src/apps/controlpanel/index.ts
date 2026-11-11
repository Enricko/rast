// Control Panel: profile, appearance, users, shared folders, storage, components, settings.
// Every value comes from the live API; unavailable endpoints render an explicit
// "not available yet" state instead of placeholder numbers.

import type { AppContext, AppDefinition } from "../../core/app";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { errorMessage } from "../../core/ui";
import type { TabRenderer } from "./common";
import { renderAppearanceTab } from "./appearance";
import { renderComponentsTab } from "./components";
import { renderProfileTab } from "./profile";
import { renderSettingsTab } from "./settings";
import { renderSharesTab } from "./shares";
import { renderStorageTab } from "./storage";
import { renderUsersTab } from "./users";

interface TabDefinition {
  id: string;
  label: string;
  icon: string;
  adminOnly: boolean;
  render: TabRenderer;
}

const TABS: TabDefinition[] = [
  { id: "profile", label: "Profile", icon: "user", adminOnly: false, render: renderProfileTab },
  { id: "appearance", label: "Appearance", icon: "wallpaper", adminOnly: false, render: renderAppearanceTab },
  { id: "users", label: "Users & Groups", icon: "users", adminOnly: true, render: renderUsersTab },
  { id: "shares", label: "Shared folders", icon: "share", adminOnly: true, render: renderSharesTab },
  { id: "storage", label: "Storage", icon: "disk", adminOnly: true, render: renderStorageTab },
  { id: "components", label: "Components", icon: "activity", adminOnly: true, render: renderComponentsTab },
  { id: "settings", label: "Settings", icon: "settings", adminOnly: true, render: renderSettingsTab },
];

export const controlPanelApp: AppDefinition = {
  id: "controlpanel",
  name: "Control Panel",
  icon: "control",
  description: "Profile, users, shared folders, storage and system settings",
  singleton: true,
  defaultSize: { w: 1060, h: 700 },
  minSize: { w: 420, h: 380 },
  mount(ctx) {
    return mountControlPanel(ctx);
  },
};

async function mountControlPanel(ctx: AppContext): Promise<void> {
  const params = (ctx.params ?? {}) as { tab?: string; section?: string };
  const isAdmin = ctx.store.isAdmin();
  const tabs = TABS.filter((tab) => !tab.adminOnly || isAdmin);
  let active = tabs.find((tab) => tab.id === params.tab)?.id ?? "profile";
  let cleanup: (() => void) | void;
  let renderToken = 0;

  ctx.root.classList.add("app-controlpanel");
  ctx.setTitle("Control Panel");

  const nav = h("nav", { class: "cp-nav", role: "tablist", ariaLabel: "Control Panel sections" });
  const navList = h("div", { class: "cp-nav-list" });
  nav.appendChild(navList);
  if (!isAdmin) {
    nav.appendChild(
      h(
        "div",
        { class: "cp-nav-note small faint" },
        "Administrator sections are hidden for this account.",
      ),
    );
  }

  const headerTitle = h("h2", { class: "cp-header-title" });
  const headerDetail = h("div", { class: "cp-header-detail small muted" });
  const headerActions = h("div", { class: "cp-header-actions" });
  const content = h("div", { class: "cp-content", role: "tabpanel" });
  const header = h("header", { class: "cp-header" }, h("div", { class: "cp-header-text" }, headerTitle, headerDetail), headerActions);
  const main = h("div", { class: "cp-main" }, header, content);
  ctx.root.append(nav, main);

  const renderNav = (): void => {
    navList.replaceChildren();
    for (const [index, tab] of tabs.entries()) {
      const activeTab = tab.id === active;
      const button = h(
        "button",
        {
          class: `cp-nav-item${activeTab ? " is-active" : ""}`,
          type: "button",
          // Tooltip for the narrow-window icon rail (the label stays the accessible name).
          title: tab.label,
          role: "tab",
          ariaSelected: activeTab,
          tabIndex: activeTab ? 0 : -1,
          onClick: () => selectTab(tab.id),
          onKeyDown: (event) => {
            if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
            event.preventDefault();
            const next = tabs[(index + (event.key === "ArrowDown" ? 1 : -1) + tabs.length) % tabs.length];
            if (!next) return;
            selectTab(next.id);
            window.setTimeout(() => {
              navList.children[tabs.indexOf(next)]?.querySelector<HTMLElement>(".cp-nav-item")?.focus();
            }, 0);
          },
        },
        h("span", { class: "cp-nav-icon" }, iconSvg(tab.icon, 17)),
        h("span", { class: "cp-nav-label" }, tab.label),
      );
      navList.appendChild(button);
    }
  };

  const setHeader = (title: string, detail: string): void => {
    headerTitle.textContent = title;
    headerDetail.textContent = detail;
    ctx.setTitle(`Control Panel — ${title}`);
  };

  async function selectTab(tabId: string): Promise<void> {
    const tab = tabs.find((candidate) => candidate.id === tabId);
    if (!tab) return;
    active = tab.id;
    renderNav();
    setHeader(tab.label, tabDescription(tab.id));
    headerActions.replaceChildren();
    content.replaceChildren();
    cleanup?.();
    cleanup = undefined;
    const token = ++renderToken;
    const loading = h("div", { class: "cp-loading", role: "status" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading…"));
    content.appendChild(loading);
    try {
      const result = await tab.render({ ctx, host: content, actions: headerActions, section: params.section });
      if (token !== renderToken) {
        // A newer tab render superseded this one; discard its cleanup.
        if (typeof result === "function") result();
        return;
      }
      cleanup = result;
    } catch (error) {
      if (token !== renderToken) return;
      content.replaceChildren(
        h(
          "div",
          { class: "error-state" },
          h("div", { class: "error-title" }, `Could not load ${tab.label}`),
          h("div", { class: "error-detail" }, errorMessage(error)),
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void selectTab(tab.id) }, "Retry"),
        ),
      );
    }
  }

  renderNav();
  await selectTab(active);

  ctx.onDispose(() => {
    cleanup?.();
    cleanup = undefined;
  });
}

function tabDescription(tabId: string): string {
  switch (tabId) {
    case "profile":
      return "Your account, password, appearance and active sessions";
    case "users":
      return "Accounts, groups, quotas, sign-in activity and access rules";
    case "shares":
      return "Shared folders and access permissions";
    case "storage":
      return "Storage locations, capacity and per-user usage";
    case "components":
      return "System health, host resources and database";
    case "settings":
      return "Server-wide preferences and read-only configuration";
    default:
      return "";
  }
}
