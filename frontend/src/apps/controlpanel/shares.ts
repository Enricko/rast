// Control Panel → Shared folders: CRUD for /api/shares with per-member ACLs.
// The endpoint may not be implemented by the running backend yet; that state is
// surfaced explicitly instead of showing invented data.

import { ApiError, apiErrorMessage } from "../../core/api";
import { badge, card } from "../../core/components";
import { h } from "../../core/dom";
import { formatDateTime } from "../../core/format";
import { formField, validateRequiredText } from "../../core/forms";
import { iconSvg } from "../../core/icons";
import type { Page, Share, ShareMember, User } from "../../core/types";
import { confirmDialog, modal } from "../../core/ui";
import { notifyError, type TabRenderContext } from "./common";

type Perm = ShareMember["perms"];

interface MemberDraft {
  userId: number;
  perms: Perm;
}

const PERMS: Array<{ value: Perm; label: string }> = [
  { value: "read", label: "Read only" },
  { value: "write", label: "Read & write" },
  { value: "admin", label: "Admin (manage share)" },
];

const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export async function renderSharesTab(tab: TabRenderContext): Promise<void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  const intro = h(
    "div",
    { class: "cp-intro" },
    h("h2", { class: "cp-intro-title" }, "Shared folders"),
    h(
      "p",
      { class: "cp-intro-detail muted" },
      "Shared folders appear in File Station under /shared. Deleting a definition keeps the files on disk.",
    ),
  );
  const container = h("div", { class: "shares-container" });
  host.append(intro, container);

  let users: User[] | null = null;
  let unavailable = false;

  const addButton = h(
    "button",
    { class: "btn btn-primary", type: "button", onClick: () => void openShareModal() },
    iconSvg("plus", 16),
    "New shared folder",
  );
  actions.appendChild(addButton);

  async function ensureUsers(): Promise<User[]> {
    if (users) return users;
    const response = await ctx.api.get<Page<User>>("/api/users", { page: 1, perPage: 100 });
    users = response.items;
    return users;
  }

  async function load(): Promise<void> {
    container.replaceChildren(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading shared folders…")));
    try {
      const shares = await ctx.api.get<Share[]>("/api/shares");
      unavailable = false;
      addButton.disabled = false;
      renderList(shares);
    } catch (error) {
      renderUnavailableState(error);
    }
  }

  function renderUnavailableState(error: unknown): void {
    unavailable = true;
    addButton.disabled = true;
    let title = "Shared folders are not available";
    let detail = apiErrorMessage(error);
    let hint = "Try again in a moment.";
    if (error instanceof ApiError) {
      if (error.isNotImplemented || error.isNotFound) {
        title = "Shared folders are not available yet";
        detail = error.message;
        hint =
          "The /api/shares endpoint is not enabled on this server build. This page will populate automatically once the storage backend enables it — nothing is simulated here.";
      } else if (error.isUnavailable) {
        title = "Storage service unavailable";
        hint = "The storage subsystem is temporarily unavailable. Other Control Panel sections still work.";
      } else if (error.isForbidden) {
        title = "Administrator access required";
        hint = "Shared folder management is restricted to administrator accounts.";
      }
    }
    container.replaceChildren(
      h(
        "div",
        { class: "not-available-state" },
        h("div", { class: "not-available-icon" }, iconSvg("share", 30)),
        h("div", { class: "not-available-title" }, title),
        h("div", { class: "not-available-detail muted" }, detail),
        h("div", { class: "not-available-hint small faint" }, hint),
        h(
          "div",
          { class: "row" },
          h("button", { class: "btn", type: "button", onClick: () => void load() }, iconSvg("refresh", 15), "Retry"),
        ),
      ),
    );
  }

  function renderList(shares: Share[]): void {
    container.replaceChildren();
    const head = h(
      "div",
      { class: "cp-toolbar" },
      h("div", { class: "small muted" }, shares.length === 0 ? "No shared folders yet" : `${shares.length} shared folder${shares.length === 1 ? "" : "s"}`),
      h("div", { class: "cp-toolbar-spacer" }),
      h("button", { class: "btn btn-sm", type: "button", onClick: () => void load() }, iconSvg("refresh", 15), "Refresh"),
    );
    container.appendChild(head);
    if (shares.length === 0) {
      container.appendChild(
        h(
          "div",
          { class: "empty-state" },
          h("div", { class: "empty-title" }, "No shared folders"),
          h("div", { class: "empty-detail" }, "Create a shared folder to give selected users access to a common workspace."),
          h("button", { class: "btn btn-primary", type: "button", onClick: () => void openShareModal() }, iconSvg("plus", 15), "Create the first folder"),
        ),
      );
      return;
    }
    const grid = h("div", { class: "share-grid" });
    for (const share of shares) {
      grid.appendChild(shareCard(share));
    }
    container.appendChild(grid);
  }

  function shareCard(share: Share): HTMLElement {
    const edit = h(
      "button",
      { class: "btn btn-sm", type: "button", ariaLabel: `Edit ${share.name}`, onClick: () => void openShareModal(share) },
      iconSvg("edit", 15),
      "Edit",
    );
    const remove = h(
      "button",
      { class: "btn btn-sm btn-danger", type: "button", ariaLabel: `Delete ${share.name}`, onClick: () => void deleteShare(share) },
      iconSvg("trash", 15),
      "Delete",
    );
    const memberChips = h("div", { class: "share-members" });
    if (share.members.length === 0) {
      memberChips.appendChild(h("span", { class: "small faint" }, "No members — administrators can still access it."));
    }
    for (const member of share.members) {
      memberChips.appendChild(
        h(
          "span",
          { class: "share-member", title: `${member.username}: ${member.perms}` },
          h("span", { class: "share-member-name" }, member.username),
          badge(member.perms, member.perms === "admin" ? "warn" : member.perms === "write" ? "info" : "neutral"),
        ),
      );
    }
    return card({
      class: "share-card",
      title: share.name,
      subtitle: share.path || `/shared/${share.name}`,
      actions: h("div", { class: "row" }, edit, remove),
      children: [
        share.description ? h("p", { class: "share-description muted" }, share.description) : h("p", { class: "share-description faint small" }, "No description"),
        memberChips,
        h(
          "div",
          { class: "share-foot small faint" },
          `${share.members.length} member${share.members.length === 1 ? "" : "s"} · created ${formatDateTime(share.createdAt)}`,
        ),
      ],
    });
  }

  async function deleteShare(share: Share): Promise<void> {
    const ok = await confirmDialog({
      title: "Delete shared folder",
      message: `Remove the “${share.name}” shared folder definition? Files inside ${share.path || `/shared/${share.name}`} are kept on disk, but users lose access through File Station until a folder with the same name is created again.`,
      confirmLabel: "Delete definition",
      danger: true,
      requireTyped: share.name,
    });
    if (!ok) return;
    try {
      await ctx.api.del(`/api/shares/${encodeURIComponent(share.id)}`);
      ctx.toast.success(`Shared folder ${share.name} removed`);
      await load();
    } catch (error) {
      notifyError(ctx, error);
    }
  }

  async function openShareModal(existing?: Share): Promise<void> {
    const nameInput = h("input", {
      class: "input",
      type: "text",
      value: existing?.name ?? "",
      placeholder: "e.g. team-documents",
      autocomplete: "off",
      spellcheck: false,
    });
    const nameField = formField({
      label: "Folder name",
      control: nameInput,
      required: true,
      hint: "Letters, digits, dot, dash and underscore. Files live at /shared/<name>.",
    });
    const descriptionInput = h("input", {
      class: "input",
      type: "text",
      value: existing?.description ?? "",
      placeholder: "Optional description",
      autocomplete: "off",
    });
    const descriptionField = formField({ label: "Description (optional)", control: descriptionInput });

    let drafts: MemberDraft[] = (existing?.members ?? []).map((member) => ({ userId: member.userId, perms: member.perms }));
    const membersHost = h("div", { class: "member-editor" });
    const membersAlert = h("div", { class: "banner banner-warn", hidden: true });
    const addMemberBtn = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("plus", 14), "Add member");
    const alert = h("div", { class: "banner banner-error", hidden: true, role: "alert" });

    let knownUsers: User[] | null = null;
    try {
      knownUsers = await ensureUsers();
    } catch (error) {
      membersAlert.hidden = false;
      membersAlert.textContent = `Could not load user accounts (${apiErrorMessage(error)}). Member permissions can only be edited when the user list is available.`;
      addMemberBtn.disabled = true;
    }

    const renderMembers = (): void => {
      membersHost.replaceChildren();
      if (!knownUsers) return;
      if (drafts.length === 0) {
        membersHost.appendChild(h("div", { class: "small faint" }, "No members yet. Share with at least one user so they can open it in File Station."));
      }
      for (const [index, draft] of drafts.entries()) {
        const userSelect = h(
          "select",
          { class: "select", ariaLabel: "Member" },
          ...knownUsers.map((user) =>
            h(
              "option",
              { value: String(user.id), selected: user.id === draft.userId },
              `${user.displayName || user.username} (@${user.username})`,
            ),
          ),
        );
        userSelect.addEventListener("change", () => {
          const member = drafts[index];
          if (!member) return;
          member.userId = Number(userSelect.value);
        });
        const permSelect = h(
          "select",
          { class: "select", ariaLabel: "Permission" },
          ...PERMS.map((perm) => h("option", { value: perm.value, selected: perm.value === draft.perms }, perm.label)),
        );
        permSelect.addEventListener("change", () => {
          const member = drafts[index];
          if (!member) return;
          member.perms = permSelect.value as Perm;
        });
        const remove = h(
          "button",
          { class: "btn btn-sm btn-subtle btn-icon", type: "button", ariaLabel: "Remove member", title: "Remove member" },
          iconSvg("close", 14),
        );
        remove.addEventListener("click", () => {
          drafts.splice(index, 1);
          renderMembers();
        });
        membersHost.appendChild(
          h(
            "div",
            { class: "member-row" },
            userSelect,
            permSelect,
            remove,
          ),
        );
      }
    };
    addMemberBtn.addEventListener("click", () => {
      if (!knownUsers) return;
      const used = new Set(drafts.map((draft) => draft.userId));
      const candidate = knownUsers.find((user) => !used.has(user.id));
      if (!candidate) {
        membersAlert.hidden = false;
        membersAlert.textContent = "All user accounts have already been added.";
        return;
      }
      drafts.push({ userId: candidate.id, perms: "read" });
      renderMembers();
    });
    renderMembers();

    const membersField = h(
      "div",
      { class: "field" },
      h("span", { class: "field-label" }, "Members"),
      membersHost,
      membersAlert,
      h("div", { class: "row form-actions" }, addMemberBtn),
    );

    const membersSection = knownUsers
      ? membersField
      : h("div", { class: "field" }, h("span", { class: "field-label" }, "Members"), membersAlert);

    const body = h(
      "div",
      { class: "col" },
      nameField.el,
      descriptionField.el,
      membersSection,
      existing ? h("div", { class: "small faint" }, `Files stay on disk at ${existing.path || `/shared/${existing.name}`}.`) : null,
      alert,
    );

    modal({
      title: existing ? `Edit shared folder — ${existing.name}` : "New shared folder",
      body,
      wide: true,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: existing ? "Save changes" : "Create shared folder",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            alert.hidden = true;
            nameField.setError(null);
            descriptionField.setError(null);
            const name = nameInput.value.trim();
            if (!NAME_PATTERN.test(name)) {
              nameField.setError("Use 1–64 characters: letters, digits, dot, dash or underscore; must start with a letter or digit.");
              nameInput.focus();
              return;
            }
            const descriptionError = descriptionInput.value.trim()
              ? validateRequiredText(descriptionInput.value, "Description", 200)
              : null;
            if (descriptionError) {
              descriptionField.setError(descriptionError);
              return;
            }
            const members = knownUsers
              ? drafts.map((draft) => ({ userId: draft.userId, perms: draft.perms }))
              : undefined;
            try {
              if (existing) {
                await ctx.api.patch(`/api/shares/${encodeURIComponent(existing.id)}`, {
                  name,
                  description: descriptionInput.value.trim(),
                  ...(members ? { members } : {}),
                });
                ctx.toast.success(`Shared folder ${name} updated`);
              } else {
                await ctx.api.post("/api/shares", {
                  name,
                  description: descriptionInput.value.trim(),
                  ...(members ? { members } : {}),
                });
                ctx.toast.success(`Shared folder ${name} created`);
              }
              close();
              await load();
            } catch (error) {
              alert.hidden = false;
              alert.textContent = apiErrorMessage(error);
              throw error;
            }
          },
        },
      ],
    });
    window.setTimeout(() => nameInput.focus(), 40);
  }

  void unavailable;
  await load();
}
