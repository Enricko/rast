# RAST HTTP API

Base path: `/api`. All bodies are JSON (`Content-Type: application/json`) unless
noted. Timestamps are Unix epoch **milliseconds**.

## Authentication

1. `POST /api/auth/login` with `{"username","password"}` sets the session cookie
   (`rast_session`, HttpOnly) and returns the user plus a CSRF token.
2. Every subsequent request sends the cookie. Every **mutating** request
   (`POST`, `PUT`, `PATCH`, `DELETE`) must also send `X-CSRF-Token` with the
   token from the login response or `GET /api/auth/me`.
3. `POST /api/auth/logout` revokes the session.

Responses use this error shape and never leak internals:

```json
{"error": {"code": "forbidden", "message": "administrator role required"}}
```

| Status | Meaning |
| --- | --- |
| 400 | validation error |
| 401 | not authenticated / session expired |
| 403 | authenticated but not allowed (or CSRF mismatch) |
| 404 | object not found **or** not visible to this user |
| 409 | conflict (name exists, job cancelled) |
| 413 | upload too large |
| 429 | rate limited / account locked (`Retry-After` header) |
| 503 | subsystem unavailable (e.g. Docker worker down) |

Paged responses: `{"items": [...], "page": 1, "perPage": 25, "total": 137}`.
All list endpoints accept `page` and `perPage` (bounded server-side).

## Public endpoints

| Method | Path | Description |
| --- | --- | --- |
| GET | `/healthz` | liveness for proxies (`ok`) |
| GET | `/api/bootstrap` | `{product, description, version, setupRequired, setupTokenRequired, registrationEnabled, maxUploadBytes, sessionTtlHours, serverTime}` |
| GET | `/api/setup/status` | `{setupRequired, tokenRequired, version}` |
| POST | `/api/setup` | `{token?, username, password, displayName?}` — creates the first admin, one-shot, disabled once an admin exists |
| POST | `/api/auth/login` | `{username, password}` → `{user, csrfToken, expiresAt}` |

`user` object (everywhere):

```json
{"id":1,"username":"admin","displayName":"Administrator","role":"admin|user",
 "active":true,"quotaBytes":10737418240,"usedBytes":0,"mustChangePassword":false,
 "createdAt":1700000000000,"updatedAt":1700000000000,"lastLoginAt":null}
```

## Session & profile

| Method | Path | Body / Notes |
| --- | --- | --- |
| GET | `/api/auth/me` | `{user, csrfToken, sessionExpiresAt, features:{docker,apps,backup,monitor,activityLogs,fileStation}, serverTime}` |
| POST | `/api/auth/logout` | clears cookie |
| POST | `/api/auth/password` | `{currentPassword, newPassword}`; revokes other sessions |
| GET | `/api/auth/sessions` | own sessions `{id, ip, userAgent, createdAt, lastSeenAt, expiresAt, current}` |
| DELETE | `/api/auth/sessions/{id}` | revoke one session |
| DELETE | `/api/auth/sessions` | revoke all other sessions |

While `mustChangePassword` is set (administrator reset or temporary password),
every protected endpoint except `/api/auth/me`, `/api/auth/password`,
`/api/auth/logout` and `/api/events` answers `403 password change required
before using RAST`.

## Users (admin)

| Method | Path | Body |
| --- | --- | --- |
| GET | `/api/users?page&perPage&q` | list |
| POST | `/api/users` | `{username, password, role, displayName?, quotaBytes?}` |
| PATCH | `/api/users/{id}` | `{displayName?, role?, active?, quotaBytes?, mustChangePassword?}` |
| DELETE | `/api/users/{id}` | deactivate (data preserved) |
| POST | `/api/users/{id}/password` | `{password, mustChangePassword?}` |
| GET | `/api/users/{id}/sessions` | active sessions |
| DELETE | `/api/users/{id}/sessions` | revoke all |

Rules enforced server-side: cannot demote/deactivate the last active admin,
cannot deactivate yourself.

## File Station

Paths are **logical** paths: `/` is the user's home root. Shared folders are
mounted read-only or read-write under `/shared/<name>` (per ACL). Use
`/api/files/roots` to discover them.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/files?path=/&page=1&perPage=100&sort=name&order=asc&q=` | listing: `{path, breadcrumbs:[{name,path}], items:[entry], page, perPage, total, quota:{quotaBytes,usedBytes,freeBytes}}` |
| GET | `/api/files/roots` | `[{name:"Home", path:"/", kind:"home"}, {name:"team", path:"/shared/team", kind:"share", perms:"read|write|admin"}]` |
| GET | `/api/files/stat?path=` | single entry |
| POST | `/api/files/mkdir` | `{path}` (parent + name) |
| POST | `/api/files/upload?dir=/&name=file.bin&overwrite=false` | **raw stream** body; `Content-Length` required; returns entry |
| POST | `/api/files/upload/init` | chunked upload: `{"dir":"/","name":"file.bin","size":N,"overwrite":false}` → `{uploadId, chunkSize, offset, size, expiresAt}`; quota is reserved at init |
| POST | `/api/files/upload/chunk?uploadId=&offset=` | raw chunk body (max `chunkSize`, normally 64 MiB); offsets must be sequential; returns `{uploadId, received, size, offset}` |
| POST | `/api/files/upload/complete?uploadId=` | finalize chunked upload → file entry |
| DELETE | `/api/files/upload?uploadId=` | cancel chunked upload and release its quota reservation |
| GET | `/api/files/download?path=` | streamed, `Content-Disposition: attachment`, supports `Range` |
| GET | `/api/files/preview?path=` | inline preview for images/PDF/text; sandboxed CSP |
| GET | `/api/files/text?path=&maxBytes=262144` | `{content, truncated, encoding}` |
| POST | `/api/files/rename` | `{path, newName}` |
| POST | `/api/files/copy` | `{paths:[], destDir}` → `{jobId}` |
| POST | `/api/files/move` | `{paths:[], destDir}` → `{jobId}` |
| POST | `/api/files/delete` | `{paths:[]}` → moves to recycle bin, `{jobId}` |
| GET | `/api/files/search?q=&path=&type=&page=` | recursive search (bounded) |
| GET | `/api/files/usage` | `{quotaBytes, usedBytes, trashBytes, byType:{...}}` |
| GET | `/api/files/trash?page=` | `{items:[{id, originalPath, sizeBytes, kind, deletedAt}], total}` |
| POST | `/api/files/trash/restore` | `{ids:[]}` |
| POST | `/api/files/trash/delete` | `{ids:[]}` permanent delete (irreversible) |
| DELETE | `/api/files/trash` | empty the bin |

Entry object:

```json
{"name":"report.pdf","path":"/docs/report.pdf","kind":"file|dir",
 "size":1234,"modifiedAt":1700000000000,"mime":"application/pdf",
 "readable":true,"writable":true,"trashed":false}
```

Shared folders (admin CRUD; ACL enforced on every files endpoint):

| Method | Path | Body |
| --- | --- | --- |
| GET | `/api/shares` | `[{id,name,path,description,members:[{userId,username,perms}],createdAt}]` |
| POST | `/api/shares` | `{name, description?, members:[{userId, perms:"read|write|admin"}]}` |
| PATCH | `/api/shares/{id}` | `{name?, description?, members?}` |
| DELETE | `/api/shares/{id}` | removes the share definition, keeps data |

## Applications (Docker, admin)

Requires `RAST_DOCKER_ENABLED=true` and a running `rast-worker.service`.
When unavailable: `503` with `{"error":{"code":"unavailable","message":"docker worker unavailable"}}`
and `/api/docker/status` reports why.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/docker/status` | `{enabled, available, version, reason}` |
| GET | `/api/apps` | managed apps with status (read-only grant: no `env`/`compose`; `envCount`, `secretCount`, first line of `lastError`) |
| POST | `/api/apps` | `{name, compose, env:[{key,value,secret}]}` → validates compose then `{app, jobId}` |
| GET | `/api/apps/{id}` | detail: status, containers, ports, volumes, resources, lastError |
| POST | `/api/apps/{id}/start` | |
| POST | `/api/apps/{id}/stop` | |
| POST | `/api/apps/{id}/restart` | |
| POST | `/api/apps/{id}/redeploy` | `{compose?, env?}` |
| GET | `/api/apps/{id}/logs?tail=200` | `{lines:[{ts,stream,message}]}` (admin only) |
| GET | `/api/apps/{id}/history` | deployment history (read-only grant: no `detail`) |
| DELETE | `/api/apps/{id}?removeVolumes=false` | confirmation required; volumes kept by default |
| GET | `/api/docker/containers` | discovered containers, flagged `managed:true|false` |
| POST | `/api/docker/import` | `{containerIds:[]}` explicit import |

Env values marked `secret:true` are encrypted at rest and returned masked
(`"••••"`); submit a new value to change them.

## Resource monitor

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/monitor/snapshot` | latest sample: `{ts, host:{cpuPercent, cpuCores, loadAvg:[...], memoryUsed/Total, swapUsed/Total, uptimeSecs}, disks:[{mount, totalBytes, usedBytes}], network:{rxBytesPerSec, txBytesPerSec, rxTotal, txTotal}, containers:[{name, cpuPercent, memoryBytes}], components:[...]}`, `unavailable` + `reason` when sampling failed |
| GET | `/api/monitor/history?metric=cpu&points=120` | ring buffer, bounded retention |
| GET | `/api/monitor/containers` | container resources (empty when Docker unavailable) |

Container inventory (names/ids/resources) is returned only to administrators
and accounts allowed to use Applications; other accounts receive
`containers: []` with `containersRestricted: true` — in REST responses and in
live `monitor` events, re-evaluated per sample. Disks exclude overlay,
container-runtime and virtual filesystems for everyone.

Live updates: `GET /api/events` (SSE). Events: `monitor` (every interval),
`job.queued|start|progress|done|failed|cancelled`, `notice`.
Each event has `event: <kind>`, `data: {...}`; include `id` for `Last-Event-ID`.

## Backup Center (admin; read-only grant sees status metadata)

Archives contain the account database (password/session hashes), the server
master key, configuration and optionally every user's files, so downloading,
the restore guide and every mutation are administrator-only regardless of
group grants. A read-only `backup` grant receives status metadata only:
`fileName`, `sha256` and `note` are empty, `error` is generic and
`downloadable` is `false`.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/backups?page=` | `{items:[{id,fileName,sizeBytes,sha256,scope:[],status,note,createdAt,finishedAt,error,downloadable}], total}` |
| POST | `/api/backups` | `{scope:["db","config","users","apps"], note?}` → `{jobId}` |
| DELETE | `/api/backups/{id}` | delete archive |
| GET | `/api/backups/{id}/download` | streamed archive (admin only, audited as `backup.download`) |
| GET | `/api/backups/schedule` | `{enabled, frequency, timeOfDay, weekday, retentionCount, scopes, lastRunAt, nextRunAt}` |
| PUT | `/api/backups/schedule` | update schedule |
| POST | `/api/backups/{id}/verify` | integrity check |
| GET | `/api/backups/restore-guide?backupId=` | `{steps:[...], warning, rollback}` (admin only); user files are restored into the storage roots, never `/` |

## Activity logs

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/logs?page&perPage&actor&action&result&from&to&q` | admin sees all, users see their own |
| GET | `/api/logs/actions` | filter options |
| GET | `/api/logs/export` | CSV download |

## Jobs

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/jobs?page&perPage&status` | own jobs (admin: all) |
| GET | `/api/jobs/{id}` | `{id, kind, status, progress, total, message, result, error}` |
| POST | `/api/jobs/{id}/cancel` | cooperative cancellation |

## System & settings (admin)

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/system/info` | host, server, counts, database, storage (admin) |
| GET | `/api/system/components` | component health list (admin) |
| GET | `/api/settings` | control panel settings |
| PATCH | `/api/settings` | `{siteName?, loginMessage?, auditRetentionDays?, trashRetentionDays?, defaultQuotaBytes?}` |
| GET | `/api/settings/storage` | storage dirs, usage, shares |

## Chunked uploads (large files behind proxies)

Cloudflare and some reverse proxies cap a single request body (100 MB on the
Cloudflare free plan). Clients split larger files into `chunkSize` pieces:

1. `POST /api/files/upload/init` reserves quota and returns `chunkSize`/`expiresAt`.
2. Send chunks sequentially with `POST /api/files/upload/chunk?uploadId=..&offset=..`
   (raw body, `Content-Length` required; offset must equal bytes received so far).
3. `POST /api/files/upload/complete?uploadId=..` atomically moves the temp file
   into place, commits the quota usage and returns the file entry.
4. `DELETE /api/files/upload?uploadId=..` cancels and releases the reservation.

Sessions expire after 24 hours (stale temp files and reservations are cleaned
up automatically); only the initiating user may write to or complete an upload.


## Appearance (per user)

Every account has its own wallpaper and theme, stored server-side in SQLite.
Uploaded wallpapers are private: only the owner can read or delete them.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/appearance` | `{theme, accent, wallpaper:{kind,builtinId,uploadId,color,fit,position,overlay}, widgets:[{id,x,y}], uploads:[{id,width,height,sizeBytes,mime,createdAt,url}], limits:{maxBytes,maxUploads}}` |
| PUT | `/api/appearance` | `{theme?, accent?, wallpaper:{kind?,builtinId?,uploadId?,color?,fit?,position?,overlay?}, widgets?:[{id,x,y}]}` (partial merge: omitted fields keep their saved value; `widgets`, when present, replaces the whole list; concurrent updates are serialised) |
| GET | `/api/appearance/wallpapers` | owner-only list of uploaded wallpapers |
| POST | `/api/appearance/wallpapers` | raw image body (`image/jpeg`, `image/png` or `image/webp`, max 10 MiB, Content-Length required) -> created upload; validated by magic bytes and dimensions |
| GET | `/api/appearance/wallpapers/{id}` | streamed image, owner only (`404` for every other account, including admins, also when revalidating); `Cache-Control: private, no-cache` + ETag, so a shared browser never reuses a previous account's image without the owner check |
| DELETE | `/api/appearance/wallpapers/{id}` | deletes the file; if it is the active wallpaper the account falls back to the default built-in |
| POST | `/api/appearance/reset` | reset wallpaper to defaults (theme, accent and widgets are kept) |

Allowed values: theme `light|dark|system`, accent `ocean|forest|teal|sage|amber|terracotta|rose|plum|indigo|graphite`, kind `builtin|upload|color`,
fit `cover|contain|center`, position `center|top|bottom|left|right|top-left|top-right|bottom-left|bottom-right`,
overlay `0..60` (percent), built-in ids `alpine-dawn|forest-mist|coastal-calm|desert-dusk`,
colors `#RRGGBB`. Limits: 10 MiB per image, 20 images and 100 MiB total per user.
Widgets: ids `health|cpu-memory|storage|network`, each at most once, at most 4;
`x`/`y` are the card's preferred position as fractions (`0..1`) of the free
desktop area, so the layout scales with the browser and cards stay on screen
(rounded to 4 decimals; unknown ids or out-of-range values are `400`).
Theme, accent, wallpaper and widgets survive restarts, logout/login and browser changes;
the client keeps only a small per-user paint cache that is cleared on sign-out.
Every successful change publishes an `appearance.changed` event to the owner's
other sessions (`{reason, originSessionId, appearance}`).



## Users, groups and access policy (admin)

NAS-style groups with per-group shared-folder and application rules. Precedence:
explicit **No Access** (user or any group) wins; otherwise the strongest grant
wins (`write` includes `read`); when no rule grants anything the legacy
`share_members` ACL decides (there is no implicit owner grant), so upgraded
installations keep their v0.1.x access. Personal homes keep ownership rules; administrators bypass
folder policy, and group permissions never grant administrator capabilities.

Folder rules: `inherit | none | read | write`. Application rules:
`inherit | none | allow` for ids `files`, `applications`, `monitor`, `backup`,
`logs`, `controlpanel` (group `allow` for Applications/Backup is read-only
status metadata: no env values, logs, archive downloads or restore guide;
deploy/config/mutations remain admin-only). Folder rules reference shared
folders by stable id (`shareId`); `shareName` is accepted on input and always
returned with the current name.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/groups?page=&perPage=&q=` | `{items:[{id,name,description,memberCount,folderRuleCount,appRuleCount,createdAt,updatedAt}],page,perPage,total}` |
| POST | `/api/groups` | `{name, description?, members:[userId], folderRules:[{shareId?,shareName?,rule}], appRules:[{appId,rule}]}` → GroupDetail |
| GET | `/api/groups/{id}` | GroupDetail with `members`, `folderRules`, `appRules` |
| PUT | `/api/groups/{id}` | same body; replaces the sets in one save; stable id across rename |
| DELETE | `/api/groups/{id}` | `{deleted,groupId,affectedUsers,affectedFolderRules,affectedAppRules,retained}`; users/folders/files are kept |
| GET | `/api/groups/{id}/members?q=&page=&perPage=` | searchable member page incl. deactivated accounts: `{items:[{userId,username,displayName,role,active}],page,perPage,total,inactiveTotal}` |
| GET | `/api/groups/{id}/impact` | deletion preview per member: `{members,folderRules,appRules,users:[{userId,username,displayName,active,changes:[{kind,resourceId,resource,before,after}]}],evaluated,truncated,retained}` |
| GET | `/api/users?q=&active=all\|active\|inactive` | items additionally expose `groups:[{id,name}]` and `groupIds` |
| GET | `/api/users/{id}/groups` | `{groupIds, groups:[{id,name}]}` |
| PUT | `/api/users/{id}/groups` | `{groupIds}` (atomic replace) |
| GET | `/api/users/{id}/access-rules` | direct user overrides: `{folderRules,appRules}` |
| PUT | `/api/users/{id}/access-rules` | replace direct overrides (explicit deny/grant) |
| GET | `/api/users/{id}/effective-access?q=&level=&page=&perPage=` | evaluator explanation: `{user,groups,folders:[{shareId,shareName,path,level,sources,fallback,conflict?}],foldersTotal,foldersAll,page,perPage,levelCounts,apps:[...]}`; `fallback` is `admin`, `explicit`, `legacy-acl` or `none` |
| GET | `/api/access/me` | caller's own effective access, same query (hidden shares omitted) |

`GET /api/auth/me` includes `permissions: {apps: {<id>:{allowed,readOnly,label}},
groups:[{id,name}]}` used by the launcher. Live updates: `group.changed`
(admin, `{reason,groupId}`) and `access.changed` (user-scoped,
`{reason,groupId?,userId,changedBy}`); SSE events are additionally filtered by
the account's current application access. Audit actions: `group.create`,
`group.update`, `group.members_update`, `group.permissions_update`,
`group.delete`, `user.groups_update`, `user.access_update`.

## Realtime events and live logs

`GET /api/events` is an authenticated SSE stream scoped server-side:

* `hello` first frame: `{seq, ts, userId, admin, sessionId, version}`,
* filtered domain events (see `docs/REALTIME.md` for the catalogue and
  authorization scopes; clients only receive events they may see),
* `ping` every 15 s (staleness detection),
* `resync` if the server-side broadcast buffer lagged; clients then fetch a
  fresh authorized snapshot,
* `session.revoked` followed by the stream closing when the session expires,
  is revoked, the account is disabled or its role changes.

The SSE `id` is a process-wide monotonic sequence number; clients drop
duplicates, detect gaps and reconcile. Credentials stay in the cookie; tokens
never appear in the event URL.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/events` | authenticated event stream (admin and user scope filtering) |
| GET | `/api/apps/{id}/logs/stream?tail=200` | admin-only SSE log stream; bounded ring buffer, `app.logs` events with `{appId, lines:[{ts,stream,message}], reset?}`; the session and active-admin status are revalidated before every poll |

Event kinds include `monitor`, `file.changed`, `file.quota`, `files.resync`,
`job.*`, `app.changed`, `app.logs`, `docker.changed`, `backup.changed`,
`audit.created`, `user.changed`, `settings.changed`, `appearance.changed`.

## Conventions for implementers

* Authorisation is checked **server-side** on every object. Hiding UI is never
  a control.
* Filesystem handlers must resolve paths inside the allowed roots only, reject
  `..`, and open with `O_NOFOLLOW` semantics where applicable.
* Any long operation returns a `jobId`; the UI follows `job.*` SSE events.
* Audit every mutation with `audit::record`.
