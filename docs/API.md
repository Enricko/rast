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
| POST | `/api/users` | `{username, password, role, displayName?, quotaBytes?, active?, mustChangePassword?, groupIds?, folderRules?, appRules?}` (see below) |
| PATCH | `/api/users/{id}` | `{displayName?, role?, active?, quotaBytes?, mustChangePassword?}` |
| DELETE | `/api/users/{id}` | deactivate (data preserved) |
| POST | `/api/users/{id}/password` | `{password, mustChangePassword?}` |
| GET | `/api/users/{id}/sessions` | active sessions |
| DELETE | `/api/users/{id}/sessions` | revoke all |

Rules enforced server-side: cannot demote/deactivate the last active admin,
cannot deactivate yourself.

`POST /api/users` can set up the whole account in one request. `active`
defaults to `true` and `mustChangePassword` to `false`. `groupIds`,
`folderRules` (`[{shareId? | shareName, rule}]`, rule `inherit|none|read|write`)
and `appRules` (`[{appId, rule}]`, rule `inherit|none|allow`) use the same
format as `PUT /api/users/{id}/groups` and `PUT /api/users/{id}/access-rules`.
Everything is validated before anything is written: an unknown group, shared
folder or application, or an invalid rule, answers `400` and creates no
account. The account, its groups and its direct rules are stored in one
transaction; `inherit` rules are not stored. The `user.create` audit entry
lists the groups and rules.

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

## Docker (admin; app id `applications`)

Requires `RAST_DOCKER_ENABLED=true` and a running `rast-worker.service`.
When unavailable: `503` with `{"error":{"code":"unavailable","message":"docker worker unavailable"}}`
and `/api/docker/status` reports why.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/docker/status` | `{enabled, available, version, reason, workerVersion, workerOutdated}`; `workerOutdated` is true when the Docker helper runs another RAST version (it needs `systemctl restart rast-worker.service`; helpers before 0.9.1 report no version) |
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
| GET | `/api/docker/containers` | every container on the host (`docker ps -a`), flagged `managed:true|false` with the app `slug` |
| POST | `/api/docker/import` | `{containerIds:[]}` explicit import |

Env values marked `secret:true` are encrypted at rest and returned masked
(`"••••"`); submit a new value to change them.

### Docker engine (admin only)

Raw containers, images and volumes, as in Docker Desktop. Container ids are the
full id or a unique prefix of at least 12 hex characters. Changes are audited
(`docker.container.*`, `docker.image.*`, `docker.volume.remove`) and announced
with the admin event `docker.inventory`. A refusal by Docker (image in use,
container gone…) is `409 conflict` with Docker's message.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/docker/containers/{id}` | `{container, inspect, managed, slug, appId, terminal:{allowed, reason}}`; `inspect` has image, command, ports, mounts, env, labels, restart policy, privileged, networks, times, exit code, health |
| POST | `/api/docker/containers/{id}/start` | `docker start` |
| POST | `/api/docker/containers/{id}/stop` | `docker stop` |
| POST | `/api/docker/containers/{id}/restart` | `docker restart` |
| DELETE | `/api/docker/containers/{id}?confirm=<name>` | `docker rm --force`, never `-v`: volumes stay. `confirm` must equal the container name. Containers of a managed app → `409` (delete the app) |
| GET (WebSocket) | `/api/docker/containers/{id}/terminal` | Interactive shell in a running container; see below |
| GET | `/api/docker/containers/{id}/logs?tail=300&since=<unix secs>` | `{lines:[{ts, stream:"stdout"|"stderr", message}], tail}`, both streams merged by time; `tail` ≤ 2000 |
| GET | `/api/docker/stats` | `{items:[{id, name, cpuPercent, memoryBytes, memoryLimitBytes, memoryPercent, netRxBytes, netTxBytes, blockReadBytes, blockWriteBytes, pids}], sampledAt}` |
| GET | `/api/docker/images` | `{items:[{id, repository, tag, reference, digest, sizeBytes, size, createdAt, createdSince, containers, usedBy:[names], inUse}], totalBytes}`; untagged images have an empty `reference` |
| GET | `/api/docker/images/inspect?image=<ref>` | `{id, tags, exposedPorts:[{port, protocol}], volumes:[paths], env:[{key, value}], cmd, entrypoint, workingDir, user, os, architecture, sizeBytes, created}`; `404` when the image is not on the server. Used to prefill "Run image", which creates a normal app through `POST /api/apps` |
| POST | `/api/docker/images/pull` | `{image}` → `{jobId, image}`; job kind `docker.pull`, audited when it ends |
| POST | `/api/docker/images/remove` | `{image}` (reference or id); never forced, so images in use stay |
| POST | `/api/docker/images/prune` | removes untagged images no container uses → `{deleted, reclaimed}` |
| GET | `/api/docker/volumes` | `{items:[{name, driver, scope, mountpoint, labels, project, anonymous, inUse, usedBy:[names], app}]}`; `app` is the managed app slug that owns it |
| POST | `/api/docker/volumes/remove` | `{name, confirm}`; `confirm` must equal `name`. Volumes in use or owned by a managed app → `409` |

Image references accept letters, digits and `. _ - / : @`, must start with a
letter or digit and have no empty path segment. Volume names follow Docker's
`[A-Za-z0-9][A-Za-z0-9_.-]*`.

`terminal.allowed` is false for privileged containers, containers sharing the
host's processes, memory (IPC) or network, with devices or extra capabilities
such as `SYS_ADMIN`, and containers that bind-mount `/`, a system folder
(`/etc`, `/root`, `/home`, `/usr`, `/var/lib`, `/proc`, …), a parent of one
(such as `/var`) or the Docker socket.

#### Container terminal

`GET /api/docker/containers/{id}/terminal` upgrades to a WebSocket. The
request must come from the RAST page: its `Origin` must match the host it was
sent to or `public_url` (otherwise `403` before the upgrade). Then:

1. Within 10 seconds the browser sends a text message
   `{"type":"start","csrf":"<csrfToken from /api/auth/me>","cols":N,"rows":N}`.
2. The server answers `{"type":"ready","container":"<name>"}` once the shell
   runs, or `{"type":"error","message":"…"}` and closes (bad token, container
   stopped or refused by the terminal policy, 4 terminals already open, …).
3. Keyboard input goes as binary messages, terminal output comes back as
   binary messages; `{"type":"resize","cols":N,"rows":N}` changes the size
   (cols 10–500, rows 2–200).
4. When the shell ends the server sends `{"type":"exit","code":N}` (`null`
   when unknown) and closes. Closing the WebSocket hangs up the shell.

The session is re-checked every minute; signing out, a revoked session or a
lost admin role sends an `error` message and closes the terminal. A terminal
without keyboard input for 60 minutes is closed. Every opening is audited as
`docker.container.exec` (success or failure, never the typed commands).

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

## App Center (admins and granted groups)

Verified runtimes installed side by side under `<data_dir>/packages/`.
App Center is for administrators and the members of groups granted the
`packages` application (`allow`); everyone else gets 403. Both may check,
install, update, change the default, remove, uninstall, cancel package jobs and
edit consumers (`canManage:true`); approving a new GitHub source stays
administrator-only. Every mutation is audited (`package.install`,
`package.update`, `package.reinstall`, `package.activate`, `package.remove`,
`package.uninstall`, `package.consumer_*`, `package.source_approve`,
`package.catalog_check`, `job.cancel`). Installs and updates run as jobs
(`job.*` events with kind `package.install|update|reinstall`, delivered to
every account that may open App Center); `POST /api/jobs/{id}/cancel`
stops one until its files are moved into place (progress message `Moving into
place`), after which it finishes.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/packages` | `{catalog:{url,name,official,keyId,officialKey,sequence,generatedAt,expiresAt,checkedAt,lastAttemptAt,nextCheckAt,intervalMinutes,checking,stale,expired,lastError}, host:{os,arch,target,libc,glibcVersion,kernel,rastVersion}, shell:{enabled,installed,active,profile,binDir,syncCommand,system:{enabled,linked,pending,displaced,directory,watcher}}, counts:{all,installed,updates,updatesReady}, canManage, apps:[{id,name,description,installed,removable}], packages:[PackageView]}`. `PackageView.system` is the runtime the operating system already has (`{command,path,realPath,version,versionCommand,output,source:"apt"\|"snap"\|"manual",sourcePackage,onPath,displaced,detectedAt}`, `displaced` while a manual install is moved aside for the system-wide link; the version is read by running the command, confined), `terminal` what a login shell runs (`{command,source:"package-center"\|"system",version,path}`); an update with `fromSystem: true` installs a newer version of the system runtime's release line next to it. `installed` counts system runtimes |
| GET | `/api/packages/{id}` | `{package: PackageView, jobs:[{jobId,packageId,version,action,status,progress,total,message,error,username,createdAt,finishedAt}]}`; `message` of a finished job is a summary (`Installed and verified Go 1.26.11 · RAST default`) |
| POST | `/api/packages/check` | fetch and verify the signed catalog now (also looks for new releases of imported packages) → `{catalog}`; a refused catalog keeps the last verified one and reports `lastError`/`stale` |
| POST | `/api/packages/{id}/install` | `{version, makeDefault?}` → `{jobId}`; only versions of the verified catalog that are compatible with this host; 503 while the catalog is expired |
| POST | `/api/packages/{id}/update` | `{version?, makeDefault?}` → `{jobId}`; without `version`: newest compatible version of the default's channel (without a default: of the system runtime's release line). Installs next to the current version, which stays for rollback. Without `makeDefault`, the first App Center version becomes the default unless the system already provides the command |
| PUT | `/api/packages/{id}/default` | `{version}` → `{ok, activation:{shims,conflicts}}`; switches `runtimes/<id>/current` and the `bin/` shims atomically. `{system: true}` goes back to the system's runtime: removes `current` and the shims, keeps the installed versions (400 without a system runtime on PATH, 409 while consumers follow the default) |
| DELETE | `/api/packages/{id}/versions/{version}?confirm={version}` | `confirm` must repeat the version; the default and pinned versions are refused (409) |
| DELETE | `/api/packages/{id}?confirm={id}` | uninstall: removes every installed version, `current` and the `bin/` shims (the system's own runtime, if any, is used again). `confirm` must repeat the id; 409 while any consumer uses the package, 404 when nothing is installed |
| POST | `/api/packages/apps/{id}/install` | install one of RAST's optional apps again (`applications` = Docker, `backup` = Backup Center) → `{ok, changed, apps}`; 400 for built-in apps, 404 for unknown ids |
| DELETE | `/api/packages/apps/{id}?confirm={id}` | uninstall an optional app → `{ok, changed, apps}`: it disappears from every desktop and its API answers 403 for every account, administrators included. Data, containers and backup schedules stay. `confirm` must repeat the id. Both calls publish `access.changed` (`reason: app_installed\|app_uninstalled`, `appId`) and `package.changed` (`reason: app`) and are audited (`package.app_install`, `package.app_uninstall`) |
| POST | `/api/packages/consumers` | `{name, packageId, version\|null, description?}`; `null` follows the default, a version pins it (and protects it from removal) |
| PUT / DELETE | `/api/packages/consumers/{id}` | edit / delete a consumer |
| POST | `/api/packages/github/inspect` | `{url}` (repository or release link) → `{inspectionId, repo, repoUrl, release, manifest, manifestSha256, checks:[{label,status,detail}], official, sourceApproved, sourceApprovedBy, hostTarget, compatible, reason, artifact, alreadyInstalled, expiresAt}`; nothing is installed |
| POST | `/api/packages/github/install` | `{inspectionId, approveSource, makeDefault?}` → `{jobId}`; `approveSource: true` is administrator-only (403); pinned to the inspected release and digests (inspection valid 30 minutes). Ownership is checked again: 409 when the id belongs to another source, repository or publisher, to the official catalog, or the package provides an official command name; 503 when no verified catalog is loaded or the kernel lacks Landlock. The source approval is recorded only once the install is queued |

`PackageView`: `{id, name, description, homepage, license, publisher:{id,name,url}, packageType, official, source, sourceRepo, inCatalog, verification:{label,detail}, installed:[{version,revision,channel,isDefault,status,path,executables:[{name,path}],bundled,target,artifact,sizeBytes,installedAt,installedBy,checkOutput,pinnedBy,releaseTag}], defaultVersion, defaultExecutables, latest, versions:[{version,revision,channel,channelLabel,channelStatus,compatible,reason,prerelease,targets,artifact,bundled,extensions,executables,installedRevision,releaseDate,notes}], unsupported:[{version,reason}], updates:[{from,to,channel,replacesDefault,revision}], detected:[{version,channel,state,detail,detectedAt,source,releaseUrl}], updateState: none|detected|ready, unavailableReason, consumers:[{id,name,packageId,version,followsDefault,effectiveVersion,executables,description,createdBy,updatedAt}], activeJob}`.
`updates` are verified versions ready to install; `detected` are new upstream
versions the catalog announces while its workflow verifies them (not
installable). Manifest format: [`rast-package.schema.json`](rast-package.schema.json).

## Linux apps (app id `applications`; managing them is administrator-only)

Desktop Linux apps installed from a File Station file (`.deb`, AppImage or a
portable `.tar.gz`/`.tar.xz`/`.tar.bz2`/`.tar` archive) or an `https://` link.
Since v0.13 they run **natively** on the server by default: each RAST user gets
one unprivileged Linux identity and one lazily started Xpra seamless graphical
session that every application and window of that user shares. Installed files
live under `<data_dir>/linux-apps` for portable/AppImage apps, or are installed
with the host package manager for `.deb` apps (through the privileged
`rast-native.service` helper, which also starts the session and application
systemd units). Per-user application profiles live in
`<data_dir>/linux-apps/profiles/<app>/<user>` and the person's RAST home is
shared with their app identity through an ACL. Legacy apps keep
`backend:"docker"` and their existing Docker behaviour until an administrator
migrates them; setting `native_enabled = false` keeps the Docker backend for
new installs. Every change is audited (`linuxapp.install`, `linuxapp.update`,
`linuxapp.limits`, `linuxapp.visibility`, `linuxapp.uninstall`,
`linuxapp.migrate`, `linuxapp.rollback`, `linuxapp.cleanup`).

Installed apps are shared with every signed-in user by default; an
administrator can restrict one to administrators. Everyone who may see an app
can launch their own copy, list it and read its icon. Managing apps
(installing, updating, removing, limits, visibility, migration) is
administrator-only. The shared app surface is not governed by the group
policy's `applications` (Docker) rule.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/linux-apps` | any signed-in user: `{apps:[LinuxApp they may see], sessions:[legacy Session] (the caller's), session:NativeSession\|null, limits:{minFreeMemoryBytes, idleMinutes, maxCpus, linux:{...}}, dockerEnabled, nativeEnabled}` |
| POST | `/api/linux-apps/preview` | administrator: `{path}` (File Station path) or `{url}` (an `https://` link) → `{preview:Preview}`. Inspects a file **without changing the system**: `.deb` metadata, dependency transaction (packages to install/upgrade/remove, download size), host compatibility, chosen launcher (or `candidates` when ambiguous), AppImage update information, portable archive contents. Source archives are rejected with an actionable `error`. |
| POST | `/api/linux-apps/install` | administrator: `{path}` (File Station path the caller can read, up to 8 GB) or `{url}` (an `https://` link on port 443 that ends in the file's name; the server downloads it, following redirects; hosts that resolve to a private, loopback or link-local address are refused with 400) or `{path|url, launcher:{argv,desktopEntry}}` for an ambiguous portable archive → legacy Docker backend `{app, jobId}`; native backend `{jobId, installing:true}` (the app row is created during the job; watch `linuxapps.changed`/job events). Job kind `linuxapps.install`. Run `preview` first and show the plan to the administrator |
| GET | `/api/linux-apps/{id}` | any signed-in user: `LinuxApp` they may see (404 otherwise) |
| PATCH | `/api/linux-apps/{id}` | administrator: `{memoryMb?, cpus?}` (512–65536 MB, 1 to the host's CPU count) → `LinuxApp`; native apps apply it to the next application start |
| DELETE | `/api/linux-apps/{id}?removeData=true` | administrator: stops the app's runs, removes managed files and (for native `.deb` apps) only the packages RAST added and nothing else depends on; with `removeData` also removes every person's profile. Legacy apps remove their images/volumes as before. Files in RAST homes are never touched. 409 while installing or updating |
| GET | `/api/linux-apps/{id}/icon` | any signed-in user: PNG or SVG (sandboxed CSP); the URL in `iconUrl` changes with every update and is cached |
| POST | `/api/linux-apps/{id}/check` | administrator: ask the app's official source for a newer version now → `{app, update}` (checks also run daily for legacy Docker apps; native apps are checked on this explicit action and record `latestVersion`/`checkedAt`). A native `.deb` check may take up to a couple of minutes because it refreshes the package policy |
| POST | `/api/linux-apps/{id}/update` | administrator: `{}` (official source), `{path}` or `{url}` (update from a file/link) → `{app, jobId}`; job kind `linuxapps.update`. Portable updates are staged, validated and switched atomically; the previous version stays available for rollback. Legacy apps keep running copies on the old image. 400 when the app has no update source and no file was given |
| POST | `/api/linux-apps/{id}/visibility` | administrator: `{visibility:"users"|"admins"}` → `LinuxApp`. `users` (default) shows the app to every signed-in user; `admins` restricts it to administrators |
| POST | `/api/linux-apps/{id}/open` | any signed-in user: launch into the caller's shared native session (or, for legacy apps, start their own container) → `Session` (legacy) or `NativeSession` (native). 404 when the app is not visible to them |
| POST | `/api/linux-apps/{id}/launch` | any signed-in user: launch a native app into the caller's shared session; idempotent (an already running app returns the existing run) → `{run:Run, session:NativeSession}`. The first launch starts the session lazily |
| GET | `/api/linux-apps/session` | any signed-in user: `{session:NativeSession\|null, limits}`; `limits` reports `idleMinutes`, `sessionMemoryMb/Cpus`, `appMemoryMb/Cpus` |
| POST | `/api/linux-apps/session/close` | any signed-in user: stop the caller's whole native session (all their apps); 404 when none runs |
| POST | `/api/linux-apps/session/runs/{run}/close` | any signed-in user: terminate one native application run (its whole process group); the session and all other apps keep running |
| POST | `/api/linux-apps/{id}/migrate` | administrator: `{}` or `{path|url}` (replacement install file when the original is gone) → `{app, jobId}`; job kind `linuxapps.migrate`. Copies every person's legacy Docker settings into native profiles (the Docker data is retained), keeps the app id and switches the backend after the native install validates |
| POST | `/api/linux-apps/{id}/migration/rollback` | administrator: return a migrated app to the Docker backend (the retained Docker data is still there) → `LinuxApp` |
| POST | `/api/linux-apps/{id}/migration/cleanup` | administrator: `{removeBackup:true}` → `LinuxApp`; deliberately removes the retained legacy Docker volumes/profiles backup after native operation was verified |
| GET | `/api/linux-apps/sessions/{sid}` | legacy apps: one of the caller's own copies → `Session`; 404 once it is gone or for someone else's copy. `sid:"native"` returns the native session |
| POST | `/api/linux-apps/sessions/{sid}/close` | legacy apps: stop their own copy now → `Session` (`sid:"native"` stops the native session) |
| GET | `/api/linux-apps/sessions/{sid}/windows` | legacy images with per-window streaming → `{windows:[Window]}`; 503 when the app's image has no window service (use Reinstall) |
| POST | `/api/linux-apps/sessions/{sid}/windows/{window}/stream` | legacy per-window streaming: start (or find) that window's VNC stream → `{url, password}`; a noVNC client connects its WebSocket to `url` (RFB bytes) and authenticates with `password` |
| DELETE | `/api/linux-apps/sessions/{sid}/windows/{window}/stream` | legacy per-window streaming: stop that window's stream |
| POST | `/api/linux-apps/sessions/{sid}/windows/{window}/close` | legacy per-window streaming: close that window inside the app, as its own close button would |

`LinuxApp`: `{id, desktopId:"linux:<id>", name, format:"deb"|"appimage"|"tarball",
fileName, version, package, description, command:[argv], launchGuess, iconUrl,
status:"installing"|"ready"|"updating"|"failed", error, jobId,
update:{source:"apt"|"github"|"zsync"|null, latestVersion, available, checkedAt,
error}, memoryMb, cpus, sizeBytes, visibility:"users"|"admins", running,
createdAt, updatedAt, backend:"docker"|"native", native:NativeInfo|null}`.
`launchGuess` is true when RAST picked the program of an archive itself.
`latestVersion` is `"newer build"` for an AppImage whose update file changed
without a version number.

`NativeInfo`: `{installPath, previousPath, launcher:{argv,desktopEntry,name,
comment,icon,terminal,workingDir,chromium}, architecture, packageVersion,
preexistingPackage, dependencies:[package], packagesAdded:[package],
compatibility:{ok, reasons:[]}, installSource:"file"|"url"|"migrated",
migration:{state:"pending"|"prepared"|"migrated"|"failed"|"rolled_back",
error, backup, migratedAt, dockerVolume}}`.

`Preview`: `{format, fileName, sizeBytes, sha256, source, app:{name, version,
package, description, icon:{type,data}|null, launchGuess, launcher|null},
candidates:[{name,argv,desktopEntry}], compatibility:{ok,reasons:[]},
deb:{package,version,architecture,description,depends:[],conflicts:[],
installs:[{name,version,oldVersion?}],removals:[],upgrades:[],downloadBytes,
transaction:{installs:[],removals:[],upgrades:[],downloadBytes},
source,candidate,repository,preexisting,installedVersion,sourcesAdded:[]}|null,
update:{kind,...}|null, warnings:[]}`. The `.deb` transaction lists every
package the simulated APT transaction would install, upgrade or remove and the
download size; removals/conflicts are shown highlighted before an administrator
confirms.

Legacy `Session`: `{id, appId, appName, status:"starting"|"ready"|"ended", url,
reason:"closed"|"idle"|"exited"|"error"|"removed"|null, error, startedAt,
viewers, separateWindows}`. `separateWindows` is true when the app's image can
stream each of its windows on its own. `url` (`/linux-apps/<key>/`) is the
frame's address; the key is 128 random bits and is the only credential of that
legacy route, so treat the URL like a password. RAST does not write it to its
logs and it stops working when the copy ends. Legacy responses on
`/linux-apps/` carry `Content-Security-Policy: sandbox …; frame-ancestors
'self'` (the frame has an opaque origin).

`NativeSession`: `{id:"native", status:"starting"|"ready"|"ended",
url:"/linux-apps/session/<key>/", startedAt, viewers, separateWindows:true,
display, reason:"closed"|"idle"|"error"|"removed"|null, error,
apps:[Run]}`. `Run`: `{id, appId, appName, status:
"launching"|"running"|"exited"|"failed", unit, error, startedAt}`. The native
session URL is served **same-origin** by RAST and every request (HTML, assets,
WebSocket) is authorized against the signed-in RAST user that owns the session;
another user's key is refused with 403 and an expired/revoked session is
refused with 401. The Xpra port itself is bound to `127.0.0.1` with Xpra-level
authentication disabled and is never exposed; RAST proxies HTTP and WebSocket
to it and injects its window-integration shim. Closing a RAST window sends the
app's normal close request (the app's own unsaved-changes dialog appears as a
dialog window); terminating an application is the separate `runs/{run}/close`
action.

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
| POST | `/api/jobs/{id}/cancel` | cooperative cancellation → `{ok, cancelRequested}` (`false` when the job already finished); the owner, an administrator, or for package jobs any App Center manager |

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
| GET | `/api/appearance` | `{theme, accent, wallpaper:{kind,builtinId,uploadId,color,fit,position,overlay}, widgets:[{id,x,y}], widgetsCustomized, taskbarPins:[appId], desktopIcons:[{id,col,row}]\|null, uploads:[{id,width,height,sizeBytes,mime,createdAt,url}], limits:{maxBytes,maxUploads}}` |
| PUT | `/api/appearance` | `{theme?, accent?, wallpaper:{kind?,builtinId?,uploadId?,color?,fit?,position?,overlay?}, widgets?:[{id,x,y}], taskbarPins?:[appId], desktopIcons?:[{id,col,row}]\|null}` (partial merge: omitted fields keep their saved value; `widgets`, `taskbarPins` and `desktopIcons`, when present, replace the whole list; concurrent updates are serialised). `desktopIcons` is the account's desktop layout: grid cells from the top left (0–63), at most 64 icons, one per cell and app id; `null` means the default layout (every app the account may open, in order); `[]` is an empty desktop |
| GET | `/api/appearance/wallpapers` | owner-only list of uploaded wallpapers |
| POST | `/api/appearance/wallpapers` | raw image body (`image/jpeg`, `image/png` or `image/webp`, max 10 MiB, Content-Length required) -> created upload; validated by magic bytes and dimensions |
| GET | `/api/appearance/wallpapers/{id}` | streamed image, owner only (`404` for every other account, including admins, also when revalidating); `Cache-Control: private, no-cache` + ETag, so a shared browser never reuses a previous account's image without the owner check |
| DELETE | `/api/appearance/wallpapers/{id}` | deletes the file; if it is the active wallpaper the account falls back to the default built-in |
| POST | `/api/appearance/reset` | reset wallpaper to defaults (theme, accent, widgets, taskbar pins and desktop icons are kept) |

Allowed values: theme `light|dark|system`, accent `ocean|forest|teal|sage|amber|terracotta|rose|plum|indigo|graphite`, kind `builtin|upload|color`,
fit `cover|contain|center`, position `center|top|bottom|left|right|top-left|top-right|bottom-left|bottom-right`,
overlay `0..60` (percent), built-in ids `alpine-dawn|forest-mist|coastal-calm|desert-dusk`,
colors `#RRGGBB`. Limits: 10 MiB per image, 20 images and 100 MiB total per user.
Widgets: ids `health|cpu-memory|storage|network`, each at most once, at most 4;
`x`/`y` are the card's preferred position as fractions (`0..1`) of the free
desktop area, so the layout scales with the browser and cards stay on screen
(rounded to 4 decimals; unknown ids or out-of-range values are `400`).
`widgetsCustomized` is `false` until the account saves a widget list (any
`widgets` value, including `[]`); until then the desktop shows System Health and
CPU & Memory when the account may use Resource Monitor.
Taskbar pins: application ids (`files|applications|monitor|backup|logs|controlpanel`)
in taskbar order, each at most once; `[]` means nothing pinned. Every account
starts with `["files","monitor","controlpanel"]`. Pins for applications the
account cannot open are kept but not shown.
Theme, accent, wallpaper, widgets and taskbar pins survive restarts, logout/login and browser changes;
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
`logs`, `controlpanel`, `packages` (group `allow` for Docker/Backup is read-only
status metadata: no env values, logs, archive downloads or restore guide;
deploy/config/mutations remain admin-only; for App Center it lets the
group manage packages, except approving a new GitHub source). Folder rules reference shared
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

`GET /api/auth/me` includes `permissions: {apps: {<id>:{allowed,readOnly,label,installed}},
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
`audit.created`, `user.changed`, `settings.changed`, `appearance.changed`,
`package.changed`, `package.catalog`.

## Conventions for implementers

* Authorisation is checked **server-side** on every object. Hiding UI is never
  a control.
* Filesystem handlers must resolve paths inside the allowed roots only, reject
  `..`, and open with `O_NOFOLLOW` semantics where applicable.
* Any long operation returns a `jobId`; the UI follows `job.*` SSE events.
* Audit every mutation with `audit::record`.
