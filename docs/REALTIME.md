# Realtime synchronization contract (internal)

RAST keeps every open window up to date with one authenticated SSE connection
per browser tab. This document is the contract between the Rust backend, the
shared frontend realtime layer and the individual applications.

## Backend

### Event envelope

`src/events.rs` publishes scoped, globally ordered events:

| Field | Meaning |
| --- | --- |
| `seq` | Process-wide monotonic sequence (SSE `id`), assigned and sent under one lock so subscribers always see ascending ids. Ids skip events the account may not see and restart at 1 with a new process, so they are a per-connection cursor, not a gap detector (see *Sequencing* below). |
| `kind` | Event name (table below). |
| `ts` | Epoch milliseconds. |
| `scope` | `Broadcast`, `Admin`, `User(id)`, `Users(ids)`, `UserAndAdmin(id)`, `Share(shareId)`. Never serialised. |
| `data` | JSON payload. |

Helpers on `AppState.events`:

```rust
publish(kind, data)                       // everyone
publish_user(kind, user_id, data)         // one account
publish_user_and_admin(kind, user_id, data)
publish_users(kind, user_ids, data)       // explicit audience (e.g. share members)
publish_admin(kind, data)                 // administrators
publish_share(kind, share_id, data)       // whoever may read that shared folder now
```

`Share(shareId)` is decided **per delivery** by the folder evaluator
(`access::folder_access_by_id`, cached and generation-aware): administrators
(who may open every shared folder), legacy members, direct and group grants,
minus explicit denials. A grant or revocation applies to the very next event
on an already-open stream, and no audience list is ever put in a payload.

Events are additionally filtered by the account's current **application access**
(group policy): a user who cannot use File Station, Applications, Backup Center or
Activity Logs receives none of that app's events, and `access.changed` is always
delivered so clients can reauthorize.

`GET /api/events` (protected) sends `hello` (seq, identity, `sessionId`),
then filtered events, `ping` heartbeats every 15 s and `resync` when the
broadcast receiver lagged or a gap was detected (the event that revealed the
gap is still delivered after the `resync`). Sessions are revalidated every
20 s; revoked, expired, disabled or role-changed sessions receive
`session.revoked` and the stream closes. Events are published **after** the
underlying database/filesystem change committed. When the server shuts down
(SIGTERM/restart) every event and log stream ends immediately, so a restart
is not held open by connected browsers; clients reconnect and resynchronise.

### Sequencing

* `hello` reports the stream's starting cursor (read *before* subscribing, so
  an event published in between surfaces as a gap instead of being skipped).
* Clients treat every `hello` as a **new cursor**: after a restart ids begin
  at 1 again, and a cursor kept from the previous process would discard every
  new event (this was the cause of missed shared-folder updates).
* Within one connection, frames older than the cursor are dropped. An id jump
  is normal (events for other accounts are skipped) and is **not** a gap;
  missed events are signalled only by the server's `resync` frame and by the
  reconnect itself, both of which trigger an authorized snapshot refresh.

### Event catalogue

| Kind | Scope | Data |
| --- | --- | --- |
| `monitor` | Broadcast | host/container snapshot (already implemented) |
| `job.queued/start/progress/done/failed/cancelled` | UserAndAdmin(owner) | `{id, kind, progress, total, message, userId, result?, error?}` |
| `file.changed` | `User(owner)` for homes; `Share(shareId)` for shared folders; recycle-bin purges `User(actor)` | `{op, path, paths:[parent dirs to refetch], userId (actor, `null` for external), shareId? , entry? (without the actor's `readable`/`writable`), source:"api"|"external"}` — copy/move/delete jobs publish the source and destination entries that changed, also when the job fails or is cancelled part-way |
| `file.quota` | owner | `{usedBytes, quotaBytes, trashBytes, userId}` |
| `files.resync` | Broadcast | `{reason}` when a watcher overflowed |
| `app.changed` | Admin | `{appId, status, containers}` |
| `app.logs` | Admin | `{appId, lines:[{ts,stream,message}]}` (bounded stream) |
| `docker.changed` | Admin | `{available, version, reason}` |
| `backup.changed` | Admin | `{reason, id?}` |
| `audit.created` | UserAndAdmin(actor) / Admin | audit row (already emitted by `audit::record`) |
| `user.changed` | Admin | `{userId, event:"created"|"updated"|"deactivated"}` |
| `settings.changed` | Admin | `{keys:[...]}` |
| `group.changed` | Admin | `{reason, groupId}` (group list/editor refresh) |
| `access.changed` | User | `{reason, groupId?, userId, changedBy}` (launcher/File Station/apps reauthorize) |
| `appearance.changed` | User | `{reason, originSessionId, appearance?}` |
| `session.revoked` | stream-local | `{reason}` (connection closes afterwards) |

Rules:

* Only publish what changed; include the affected identifiers so clients can
  decide whether they care.
* Never include secrets, session tokens, file contents or other users' data in
  an event. Scoping is enforced server-side.
* Long operations publish job progress; the domain event is emitted once, on
  commit.

### External changes

* **Files:** a bounded recursive watcher (`notify`) covers `storage_dir/homes`
  and `storage_dir/shared` only, debounced ~400 ms, bounded to a few thousand
  events/second with coalescing. Overflow emits `files.resync`. Paths are mapped
  to the owning account (homes) or the shared folder's id (shared) before
  publishing; readers of the folder are decided per delivery.
* **Containers:** a low-frequency reconciler compares managed application state
  every 5 s (and after reconnect) and emits `app.changed` when it differs.
  Docker events are used for per-container changes where available.
* **Logs:** `GET /api/apps/{id}/logs/stream` is an admin SSE stream with a
  bounded ring buffer (last ~500 lines) and rotation-safe incremental reads.

## Frontend

`core/sse.ts` owns the single `EventSource` per tab:

* sequencing (per-connection cursor reset by `hello`, older frames dropped,
  `resync` from the server), `hello`/`ping`/`resync`,
* exponential backoff with jitter (1 s → 30 s), stale detection after 45 s,
  `online`/`offline` and visibility/wake handling,
* `onResync` for views that must refetch snapshots,
* `ConnectionState = connected | connecting | reconnecting | offline`.

`core/realtime.ts` is the shared store:

```ts
const realtime = getRealtime(ctx.events);      // one per tab
const off = realtime.on<T>(kind, (data) => …); // auto-unsubscribed on dispose
const offResync = realtime.onResync(() => refreshSnapshot());
const debounced = debounceTask(() => reload(), 250);
realtime.cache.load(key, fetcher, { maxAgeMs }); // dedupe + share
```

Application rules:

1. Subscribe to the kinds that affect you and refetch only the affected,
   authorized data (debounced for bursts). Keep selection, scroll offset,
   filters and expanded state across the refresh; never remount the desktop.
2. Apply the server response of the user's own action first; reconcile with the
   event that follows (de-duplicate, do not regress newer data).
3. On `resync`/reconnect, fetch a fresh snapshot of the active view.
4. Remove routine “Refresh” buttons once realtime works; keep contextual
   Retry/Reconnect for failures.
5. Unsubscribe on window close (`ctx.onDispose`) and when the shell stops the
   stream at sign-out.

## Verification hooks

`docs/REALTIME.md` is covered by tests: event scoping (`src/events.rs`),
SSE authorization/lifecycle, filesystem watcher mapping and API event emission.
