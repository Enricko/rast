# User groups & access policy — implementation contract (internal)

This is the shared contract for the NAS-style **Users & Groups** feature. It is
read by the Rust backend, the Control Panel UI and the application windows.

## 1. Data model (migration `0004_groups.sql`, purely additive)

```sql
CREATE TABLE groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT, -- stable id; renames keep it
    name        TEXT NOT NULL COLLATE NOCASE UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);

CREATE TABLE group_folder_rules (
    group_id   INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    share_name TEXT NOT NULL COLLATE NOCASE,           -- logical resource
    rule       TEXT NOT NULL CHECK (rule IN ('inherit','none','read','write')),
    PRIMARY KEY (group_id, share_name)
);

CREATE TABLE group_app_rules (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    app_id   TEXT NOT NULL,
    rule     TEXT NOT NULL CHECK (rule IN ('inherit','none','allow')),
    PRIMARY KEY (group_id, app_id)
);

CREATE TABLE user_folder_rules ( -- direct user rules (explicit denials/grants)
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    share_name TEXT NOT NULL COLLATE NOCASE,
    rule       TEXT NOT NULL CHECK (rule IN ('inherit','none','read','write')),
    PRIMARY KEY (user_id, share_name)
);

CREATE TABLE user_app_rules (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id  TEXT NOT NULL,
    rule    TEXT NOT NULL CHECK (rule IN ('inherit','none','allow')),
    PRIMARY KEY (user_id, app_id)
);
-- indexes: group_members(user_id), group_folder_rules(share_name), group_app_rules(app_id)
```

Existing tables (`users`, `shares`, `share_members`) are untouched. Rules reference
shared folders by display **name**; share rename/delete must rewrite/delete the
rules in the same transaction. Group ids are stable across renames.

## 2. Apps (fixed ids, enforceable capabilities)

| id | label | non-admin default | group `allow` means |
| --- | --- | --- | --- |
| `files` | File Station | allowed (home ownership / share ACL) | — (deny hides + blocks) |
| `monitor` | Resource Monitor | allowed | — |
| `logs` | Activity Logs | allowed (own events; admins see all) | — |
| `controlpanel` | Control Panel | allowed (own profile/appearance only; admin tabs stay role-gated) | — |
| `applications` | Applications | **denied** (admin-only today) | read-only list/detail/logs/history; deploy/start/stop/delete remain admin-only |
| `backup` | Backup Center | **denied** (admin-only today) | read-only list/download/guide; create/delete/verify/schedule-write remain admin-only |

Explicit `none` always wins and blocks the API prefix + live events + launcher.

## 3. Precedence (one evaluator used by enforcement *and* Effective Access)

Folder (`/shared/<name>`) decision for a non-admin user:

1. Collect direct (`user_folder_rules`) + group (`group_folder_rules` of all
   groups the user belongs to) rules for the share name.
2. Any `none` → **No Access**.
3. Else strongest grant: `write` > `read` > (`inherit` ignored).
4. If no grant is present → **fallback to the pre-groups behaviour**:
   * share owner (`shares.created_by`) → full/write,
   * legacy `share_members.perms` (`read`/`write`/`admin`) → that level,
   * otherwise No Access.
5. Admins bypass everything (full access). Personal homes keep ownership rules
   (owner full, admins full, others hidden).
6. "New restricted resources require an explicit grant": a new share with no
   member rows and no rules is admin-only.

Application decision:

1. Admin → all apps allowed (existing role checks still gate privileged routes).
2. Collect direct + group app rules.
3. Any `none` → denied.
4. Any `allow` → allowed (`readOnly = !is_admin`; application/backup surface
   read-only).
5. No rule → existing default (table above) with `fallback:"default"`.

Decisions expose **sources** so the admin UI can explain them:
`[{type:"direct"|"group", groupId?, groupName?, rule}]` plus `fallback` label.

Caching: evaluator results are cached in-process keyed by `(user_id, resource)`
and invalidated by a global generation counter that is bumped by every
group/membership/rule/share change (all in the same process after commit).

## 4. Rust evaluator (`src/access/mod.rs`)

```rust
pub const APP_IDS: &[&str] = &["files","applications","monitor","backup","logs","controlpanel"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum FolderRule { Inherit, None, Read, Write }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum AppRule { Inherit, None, Allow }

pub struct RuleSource { pub kind: RuleSourceKind, pub group_id: Option<i64>, pub group_name: Option<String>, pub rule: String }

pub struct FolderAccess {
    pub share_id: String, pub share_name: String,
    /// None = No Access, otherwise the granted level (read/write/admin)
    pub level: Option<Perm>,
    pub sources: Vec<RuleSource>, pub fallback: Option<&'static str>,
}

pub struct AppAccess { pub app_id: String, pub allowed: bool, pub read_only: bool, pub sources: Vec<RuleSource>, pub fallback: &'static str }

pub async fn folder_access_by_name(pool, user: &AuthUser, share_name: &str) -> AppResult<FolderAccess>;
pub async fn folder_access_by_id(pool, user: &AuthUser, share_id: &str) -> AppResult<FolderAccess>;
pub async fn app_access(pool, user: &AuthUser, app_id: &str) -> AppResult<AppAccess>;
pub fn app_allowed_for_events(pool, user, kind) -> bool;   // async cached variant ok
pub async fn effective_access(pool, user: &AuthUser) -> AppResult<EffectiveAccessDto>;

pub fn bump_generation();          // call after any committed policy/membership/share change
pub fn invalidate_user(user_id);   // optional targeted invalidation
```

Enforcement helpers:
```rust
pub async fn require_folder(pool, user, share_name, need: Perm) -> AppResult<()>; // hidden => NotFound
pub async fn app_surface(pool, state, user, app_id) -> AppResult<AppAccess>;       // 403 when denied
```
Storage maps `FolderAccess.level` → `Perm` and keeps returning 404 for hidden
resources (do not leak existence).

## 5. REST API (admin-only unless noted)

### Groups
| Method | Path | Body / response |
| --- | --- | --- |
| GET | `/api/groups?page=1&perPage=25&q=` | `{items:[GroupSummary],page,perPage,total}` |
| POST | `/api/groups` | `{name,description?,members?:[userId],folderRules?:[{shareName,rule}],appRules?:[{appId,rule}]}` → GroupDetail (201) |
| GET | `/api/groups/{id}` | GroupDetail |
| PUT | `/api/groups/{id}` | same body as POST (sets replace, single save) → GroupDetail |
| DELETE | `/api/groups/{id}` | `{deleted:true,affectedUsers,affectedFolderRules,affectedAppRules}` |

`GroupSummary`: `{id,name,description,memberCount,folderRuleCount,appRuleCount,createdAt,updatedAt}`
`GroupDetail`: summary fields + `members:[{userId,username,displayName}]`,
`folderRules:[{shareName,rule}]`, `appRules:[{appId,rule}]`.

### Users & access
| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/users` | now each user also has `groups:[{id,name}]` and `groupIds:[..]` |
| GET | `/api/users/{id}/groups` | `{groupIds,groups:[{id,name}]}` |
| PUT | `/api/users/{id}/groups` | `{groupIds:[..]}` (atomic replace, audit `user.groups_update`) |
| GET | `/api/users/{id}/access-rules` | `{folderRules:[{shareName,rule}],appRules:[{appId,rule}]}` (direct rules) |
| PUT | `/api/users/{id}/access-rules` | same shape; replaces direct rules, audit `user.access_update` |
| GET | `/api/users/{id}/effective-access` | `{user,groups,folders:[{shareId,shareName,path,level,sources,fallback}],apps:[{appId,label,allowed,readOnly,sources,fallback}]}` |
| GET | `/api/access/me` | same shape for the caller (for launcher/effective view) |

`GET /api/auth/me` additionally returns
`permissions: { apps: { "<appId>": {allowed, readOnly} }, groups: [{id,name}] }`.

### Shares
Existing endpoints unchanged; `GET /api/shares` is the source of share names for
the group editor. Rename/delete rewrites/deletes rules referenced by name.

## 6. Enforcement

* Files (`src/storage/**`, `src/api/files.rs`): `resolve_root`/`resolve` use
  `folder_access_by_id`; listing `/`, roots, search, preview, download, upload,
  rename, copy/move (read source + write destination; move write both), delete,
  trash restore/delete must require the evaluated level. Copy/move/delete jobs
  re-evaluate permissions at job start (job returns a clear `forbidden` error if
  revoked). Share create/update/delete rewrite group/user rules atomically and
  `bump_generation()`.
* API middleware (`src/access/middleware.rs`): after authentication, non-admin
  requests are mapped to an app prefix (`/api/files`→files, `/api/monitor`→monitor,
  `/api/logs`→logs, `/api/users|/api/shares|/api/settings`→controlpanel,
  `/api/apps|/api/docker`→applications, `/api/backups`→backup) and denied with
  `403 {"error":{"code":"forbidden","message":"…access denied by group policy"}}`
  when the app is denied. Read-only app grants allow `GET/HEAD/OPTIONS` only for
  applications/backup; writes still hit the existing `AdminUser` checks.
* Applications/Backup read handlers: accept admins **or** users with a read-only
  grant (`access::app_surface`), returning the same payload (secrets stay masked).
  Mutations keep `AdminUser`.
* Activity Logs: non-admins keep own-events behaviour; `logs` denial blocks the
  app/API.
* Control Panel: profile/appearance remain available to allowed users; every
  admin tab/endpoint keeps `AdminUser`/role checks — group rules never grant
  administrator privileges.
* Last-active-admin: `users::update`/`deactivate` must count active admins and
  update/deactivate inside one `BEGIN IMMEDIATE` transaction so a concurrent
  change cannot remove the last admin.

## 7. Live updates

Events published after commit via `AppState.events`:

| kind | scope | data |
| --- | --- | --- |
| `group.changed` | Admin | `{reason:"created"|"updated"|"deleted",groupId}` (editors refresh lists/detail) |
| `access.changed` | User (per affected user) | `{reason, groupId?, userId, changedBy}` (sessions refetch `/api/auth/me`, launcher, effective views) |
| `audit.created` | existing | new actions below |

New audit actions: `group.create`, `group.update`, `group.delete`,
`group.members_update`, `group.permissions_update`, `user.groups_update`,
`user.access_update`, `access.denied` (denials worth auditing).

SSE (`/api/events`) filters event kinds by app access for the connected user
(files→file.*/files.resync, monitor→monitor, applications→app.*/docker.changed,
backup→backup.*, logs→audit.created, controlpanel→user/settings/group.changed;
appearance.changed is always delivered to its owner). Access changes take effect
for deliveries immediately (generation cache) and the API emits
`access.changed` so clients reauthorize open windows.

## 8. Frontend integration

* `MeResponse.permissions` drives `appsForPermissions(permissions, isAdmin)`
  (fallback to current role behaviour when absent).
* On `access.changed`: refetch `/api/auth/me`, update the store, rebuild the
  launcher/start menu/pinned apps, re-evaluate the active window (close with an
  access message when revoked), toast “Your access was updated”.
* Control Panel tab becomes **Users & Groups** (`users` id, label changed) with
  two inner tabs (Users / Groups) using the shared segmented control.
  Groups tab: search + table + create; editor modal with General, Members,
  Shared Folders, Applications sections and one Save action (existing
  dirty/saving/saved/error FSM), delete confirmation showing affected
  users/rules and noting users/files are retained.
  Users tab: search, group membership chips, edit modal with Group Membership,
  Direct Overrides and Effective Access (level + provenance + conflict note).
* File Station: on `access.changed` revalidate the current directory; on
  `403/404` clear inaccessible rows/selections and show an access message;
  events are already filtered server-side.
* Applications/Backup: when `readOnly` hide/disable mutation affordances and show
  a read-only banner; direct API denials stay surfaced.

## 9. Required tests

* evaluator precedence: direct deny vs group grant (both orders), strongest
  grant, RW includes read, inherit fallback to legacy `share_members`, owner,
  admins, hidden shares; app defaults/deny/allow/read-only.
* groups API: CRUD, validation, membership replace, rules replace, delete counts,
  audit rows, `group.changed`/`access.changed` scopes, last-admin untouched.
* enforcement: listing/search/preview/download, viewer read/deny write, editor
  write, copy source-read/dest-write, move write-both, job start recheck after
  revocation, direct URL for denied app (403), allowed app read-only vs write,
  SSE event filtering.
* migration: existing share membership keeps exactly the same effective access
  with no rules (upgrade behaviour), fresh install schema v4.
