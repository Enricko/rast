// Control Panel → Settings: server-wide preferences (editable) and read-only
// configuration that comes from the server config file/environment.

import { apiErrorMessage } from "../../core/api";
import { card } from "../../core/components";
import { getRealtime } from "../../core/realtime";
import { h } from "../../core/dom";
import {
  bytesToGib,
  formField,
  gibToBytes,
  parseGib,
  validateGib,
  validateRequiredText,
  withButtonBusy,
} from "../../core/forms";
import { formatBytes } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { Settings } from "../../core/types";
import { pageIntro, scrollToSection, type TabRenderContext } from "./common";

const READ_ONLY_LABELS: Record<string, string> = {
  bind: "Listen address",
  publicUrl: "Public URL",
  dataDir: "Data directory",
  storageDir: "Storage directory",
  sessionTtlHours: "Session lifetime",
  monitorIntervalSecs: "Monitor interval",
  maxUploadBytes: "Maximum upload size",
  dockerEnabled: "Docker integration",
  secureCookies: "Secure cookies",
  registrationEnabled: "Public registration",
};

function formatConfigValue(key: string, value: unknown): string {
  if (typeof value === "boolean") return value ? "Enabled" : "Disabled";
  if (key === "maxUploadBytes" && typeof value === "number") return formatBytes(value);
  if (key === "sessionTtlHours" && typeof value === "number") return `${value} hours`;
  if (key === "monitorIntervalSecs" && typeof value === "number") return `${value} seconds`;
  if (value === null || value === undefined || value === "") return "not configured";
  return String(value);
}

export async function renderSettingsTab(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  let settings = await ctx.api.get<Settings>("/api/settings");
  host.replaceChildren();
  actions.replaceChildren();
  const realtime = getRealtime(ctx.events);
  let saving = false;

  const intro = pageIntro(
    "Server preferences",
    "Preferences stored in the RAST database. Fields managed by the server configuration file are read-only here.",
  );

  // ------------------------------------------------------------ editable form
  const siteName = h("input", { class: "input", type: "text", value: settings.siteName, autocomplete: "off" });
  const siteNameField = formField({
    label: "Site name",
    control: siteName,
    required: true,
    hint: "Shown in the browser title, start menu and about dialog. 1–64 characters.",
  });
  const loginMessage = h("textarea", { class: "textarea", rows: 2, value: settings.loginMessage, placeholder: "Optional short announcement for users" });
  const loginMessageField = formField({
    label: "Login message",
    control: loginMessage,
    hint: "Stored in server settings (max 200 characters).",
  });
  const auditDays = h("input", {
    class: "input",
    type: "number",
    min: "1",
    max: "3650",
    step: "1",
    value: String(settings.auditRetentionDays),
  });
  const auditField = formField({
    label: "Activity log retention (days)",
    control: auditDays,
    required: true,
    hint: "Older audit entries are pruned automatically. 1–3650 days.",
  });
  const trashDays = h("input", {
    class: "input",
    type: "number",
    min: "1",
    max: "3650",
    step: "1",
    value: String(settings.trashRetentionDays),
  });
  const trashField = formField({
    label: "Recycle bin retention (days)",
    control: trashDays,
    required: true,
    hint: "Deleted files are purged from the recycle bin after this period.",
  });
  const defaultQuota = h("input", {
    class: "input",
    type: "number",
    min: "0",
    step: "1",
    value: String(bytesToGib(settings.defaultQuotaBytes)),
  });
  const defaultQuotaField = formField({
    label: "Default quota for new users (GiB)",
    control: defaultQuota,
    hint: "0 means unlimited.",
  });
  const alert = h("div", { class: "banner banner-error", hidden: true, role: "alert" });
  const savedNote = h("span", { class: "saved-note small", ariaLive: "polite" });
  const saveButton = h("button", { class: "btn btn-primary", type: "button" }, iconSvg("check", 16), "Save settings");
  const resetButton = h("button", { class: "btn btn-ghost", type: "button" }, "Reset changes");
  let snapshot = {
    siteName: settings.siteName,
    loginMessage: settings.loginMessage,
    auditRetentionDays: settings.auditRetentionDays,
    trashRetentionDays: settings.trashRetentionDays,
    defaultQuotaBytes: settings.defaultQuotaBytes,
  };
  const isDirty = (): boolean =>
    siteName.value.trim() !== snapshot.siteName ||
    loginMessage.value.trim() !== snapshot.loginMessage ||
    Number(auditDays.value) !== snapshot.auditRetentionDays ||
    Number(trashDays.value) !== snapshot.trashRetentionDays ||
    gibToBytes(parseGib(defaultQuota.value)) !== snapshot.defaultQuotaBytes;
  const refreshDirty = (): void => {
    saveButton.disabled = !isDirty();
    resetButton.disabled = !isDirty();
  };
  [siteName, loginMessage, auditDays, trashDays, defaultQuota].forEach((input) => {
    input.addEventListener("input", refreshDirty);
    input.addEventListener("change", refreshDirty);
  });
  refreshDirty();

  resetButton.addEventListener("click", () => {
    siteName.value = snapshot.siteName;
    loginMessage.value = snapshot.loginMessage;
    auditDays.value = String(snapshot.auditRetentionDays);
    trashDays.value = String(snapshot.trashRetentionDays);
    defaultQuota.value = String(bytesToGib(snapshot.defaultQuotaBytes));
    alert.hidden = true;
    refreshDirty();
  });

  saveButton.addEventListener("click", () => {
    alert.hidden = true;
    siteNameField.setError(null);
    loginMessageField.setError(null);
    auditField.setError(null);
    trashField.setError(null);
    defaultQuotaField.setError(null);
    let invalid = false;
    const siteNameError = validateRequiredText(siteName.value, "Site name", 64);
    if (siteNameError) {
      siteNameField.setError(siteNameError);
      invalid = true;
    }
    if ([...loginMessage.value].length > 200) {
      loginMessageField.setError("Login message must be at most 200 characters");
      invalid = true;
    }
    const auditValue = Number(auditDays.value);
    if (!Number.isInteger(auditValue) || auditValue < 1 || auditValue > 3650) {
      auditField.setError("Enter a whole number between 1 and 3650");
      invalid = true;
    }
    const trashValue = Number(trashDays.value);
    if (!Number.isInteger(trashValue) || trashValue < 1 || trashValue > 3650) {
      trashField.setError("Enter a whole number between 1 and 3650");
      invalid = true;
    }
    const quotaError = validateGib(defaultQuota.value);
    if (quotaError) {
      defaultQuotaField.setError(quotaError);
      invalid = true;
    }
    if (invalid) return;
    saving = true;
    void withButtonBusy(saveButton, "Saving…", async () => {
      try {
        const updated = await ctx.api.patch<Settings>("/api/settings", {
          siteName: siteName.value.trim(),
          loginMessage: loginMessage.value.trim(),
          auditRetentionDays: auditValue,
          trashRetentionDays: trashValue,
          defaultQuotaBytes: gibToBytes(parseGib(defaultQuota.value)),
        });
        snapshot = {
          siteName: updated.siteName,
          loginMessage: updated.loginMessage,
          auditRetentionDays: updated.auditRetentionDays,
          trashRetentionDays: updated.trashRetentionDays,
          defaultQuotaBytes: updated.defaultQuotaBytes,
        };
        ctx.store.patch({ siteName: updated.siteName, loginMessage: updated.loginMessage });
        savedNote.textContent = "Saved";
        window.setTimeout(() => {
          savedNote.textContent = "";
        }, 2400);
        ctx.toast.success("Server settings saved");
        refreshDirty();
      } catch (error) {
        alert.hidden = false;
        alert.textContent = apiErrorMessage(error);
      } finally {
        saving = false;
      }
    });
  });

  const conflictHost = h("div", { class: "cp-conflict", hidden: true });

  /** Replace the form baseline with the latest server copy. */
  function adoptServerSettings(fresh: Settings): void {
    settings = fresh;
    snapshot = {
      siteName: fresh.siteName,
      loginMessage: fresh.loginMessage,
      auditRetentionDays: fresh.auditRetentionDays,
      trashRetentionDays: fresh.trashRetentionDays,
      defaultQuotaBytes: fresh.defaultQuotaBytes,
    };
    siteName.value = fresh.siteName;
    loginMessage.value = fresh.loginMessage;
    auditDays.value = String(fresh.auditRetentionDays);
    trashDays.value = String(fresh.trashRetentionDays);
    defaultQuota.value = String(bytesToGib(fresh.defaultQuotaBytes));
    ctx.store.patch({ siteName: fresh.siteName, loginMessage: fresh.loginMessage });
    conflictHost.hidden = true;
    alert.hidden = true;
    refreshDirty();
  }

  function showConflict(): void {
    conflictHost.hidden = false;
    conflictHost.replaceChildren(
      h(
        "div",
        { class: "banner banner-warn", role: "status" },
        h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
        h(
          "div",
          { class: "banner-body" },
          h("div", { class: "banner-title" }, "Server settings changed in another session"),
          h(
            "div",
            { class: "banner-message" },
            "Your unsaved edits were kept. Reload to review the latest saved values, or save to overwrite them.",
          ),
          h(
            "button",
            { class: "btn btn-sm", type: "button", onClick: () => void reloadFromServer() },
            "Reload",
          ),
        ),
      ),
    );
  }

  async function reloadFromServer(): Promise<void> {
    try {
      const fresh = await ctx.api.get<Settings>("/api/settings");
      adoptServerSettings(fresh);
      ctx.toast.success("Settings reloaded from the server");
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  const formCard = card({
    title: "Preferences",
    subtitle: "Changes apply immediately to all users",
    children: [
      conflictHost,
      siteNameField.el,
      loginMessageField.el,
      h(
        "div",
        { class: "form-grid-2" },
        auditField.el,
        trashField.el,
      ),
      defaultQuotaField.el,
      alert,
      h("div", { class: "row wrap form-actions" }, saveButton, resetButton, savedNote),
    ],
  });

  // ---------------------------------------------------------------- read-only
  const readOnlyKeys = Array.isArray(settings.config.readOnly) ? settings.config.readOnly : [];
  const configEntries = Object.entries(settings.config).filter(([key]) => key !== "readOnly");
  const readOnlyList = h("dl", { class: "kv compact readonly-list" });
  for (const [key, value] of configEntries) {
    const label = READ_ONLY_LABELS[key] ?? key;
    readOnlyList.appendChild(
      h(
        "div",
        { class: "readonly-row" },
        h("dt", {}, h("span", { class: "lock-icon" }, iconSvg("lock", 13)), label),
        h("dd", {}, formatConfigValue(key, value)),
        h(
          "span",
          { class: "readonly-marker small faint" },
          readOnlyKeys.includes(key) ? "controlled by server configuration" : "",
        ),
      ),
    );
  }
  if (configEntries.length === 0) {
    readOnlyList.appendChild(h("div", { class: "table-empty" }, "No server configuration was returned."));
  }
  const readOnlyCard = card({
    title: "Server configuration",
    subtitle: "Read-only — set through the config file or environment variables",
    children: [
      h(
        "div",
        { class: "banner banner-info" },
        h("span", { class: "banner-icon" }, iconSvg("lock", 16)),
        h(
          "div",
          { class: "banner-body" },
          h(
            "div",
            { class: "banner-message" },
            "These values can only be changed by editing the server configuration and restarting RAST. They are shown here for reference.",
          ),
        ),
      ),
      readOnlyList,
    ],
  });

  host.append(intro, h("div", { class: "cp-grid" }, formCard, readOnlyCard));
  scrollToSection(host, tab.section);

  const unsubscribe = realtime.on<{ keys?: string[] }>("settings.changed", (data) => {
    if (saving) return;
    const keys = Array.isArray(data?.keys) ? data.keys : [];
    const relevant = ["siteName", "loginMessage", "auditRetentionDays", "trashRetentionDays", "defaultQuotaBytes"];
    if (keys.length > 0 && !keys.some((key) => relevant.includes(key))) return;
    void (async () => {
      try {
        const fresh = await ctx.api.get<Settings>("/api/settings");
        if (saving) return;
        if (isDirty()) showConflict();
        else adoptServerSettings(fresh);
      } catch {
        /* the tab keeps its current values; a manual reload is still possible */
      }
    })();
  });
  const unsubscribeResync = realtime.onResync(() => {
    if (!isDirty()) void reloadFromServer();
  });
  return () => {
    unsubscribe();
    unsubscribeResync();
  };
}
