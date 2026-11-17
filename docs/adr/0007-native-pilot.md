# 0007. A native pilot for test accounts and test shared folders

- Status: accepted (coordinator, under the maintainer's session rule: the key
  only ever reduces access)
- Date: 2026-11-17

## Context

Production is also the test host (CLAUDE.md, "Production safety"). It runs
with `native_enabled = false`, the native backend quarantined, because
turning it on changes things for every account at once:

- the shared folder syncer creates `rast-s<h>-rw`/`-ro` groups for every
  shared folder, sets their members for every account with a Linux identity,
  restarts sessions and sends banners; the repair pass and the create and
  delete hooks call the helper for every shared folder (`share_dir_release`
  even on a legacy container);
- quota sync sets kernel limits for every share group and every one-HOME
  account, and measures every shared folder's recycle bins;
- Linux apps are visible by app (`users`/`admins`), not by account, and
  every account can start an Xpra session;
- `rast migrate home` and `rast migrate shares` can apply to any account or
  shared folder.

Phase 1 must be tested on production with test accounts (`t-…`) and test
shared folders (`t-…`) only, while real accounts keep the 0.18.x behaviour.
Alternatives considered: a second host or VM (ruled out by the maintainer),
and a per-account database flag (needs a migration, which is owned by the
project lead, and an admin UI). A name prefix needs neither, matches the
existing test naming rule, and is easy to audit from the configuration.

## Decision

A new optional configuration key, `native_pilot_prefixes` (env
`RAST_NATIVE_PILOT_PREFIXES`, comma separated). Absent: no restriction,
today's behaviour. Present: the native features apply only to

- accounts whose username starts with a prefix (usernames are stored in
  lower case; the check lower-cases too), and
- shared folders whose name **and** container start with a prefix.

`[]` (or `","` in the environment) means nobody. Prefixes are 1–32
characters of the username charset (`a-z`, `0-9`, `.`, `_`, `-`), start with
a letter or digit, are lower case, at most 16; anything else stops the
configuration from loading.

Outside the pilot, administrators included:

- **Linux apps, any backend.** A route layer on every `/api/linux-apps*`
  route: the list answers without apps or sessions and with
  `linuxAppsEnabled`, `nativeEnabled` and `dockerEnabled` false (the UI hides
  the feature), the management routes answer 403 with the reason, every
  other route 404 like an invisible app (new routes default to 404). The
  service layer (`LinuxApps::list`, `get`, `icon`, `open`, `native_launch`,
  `native_session_json`), `NativeSessions::launch` and `ensure_session`, and
  the Xpra display proxy refuse as well, as backstops.
- **One HOME.** `HomeFs::is_one_home` is false: File Station, Terminal and
  apps keep the legacy layout, recycle bin and quota.
- **Shared folder groups.** The plan holds only pilot shared folders,
  accounts and identities, so no helper call (`share_group_ensure`,
  `share_members_set`, `share_session_groups`, restarts, links, banners)
  names anything outside the pilot; helper replies that name an account
  outside it (a group filled before the pilot was set) are dropped with a
  warning. The repair pass and the create and delete hooks skip shared
  folders outside the pilot.
- **Quota.** Kernel user limits only for pilot one-HOME accounts, group
  limits and recycle bin measurements only for pilot shared folders.
- **CLI.** `rast migrate shares` and `rast migrate home`: `--dry-run`
  reports everything and marks entries outside the pilot. Apply and
  `--rollback` refuse a `--share` or `--user` outside it, by the typed name
  before the server lock and again by the stored name and container, with
  exit code 1 and nothing changed; `--all` covers only the pilot and lists
  the rest as skipped. A pilot account that can open a shared folder outside
  the pilot is not moved to one HOME (its apps would lose that folder), and
  recycle bin rows of such folders stay in the legacy bin.

Terminal is not part of the pilot. `rast doctor` and the server's start log
show the pilot.

## Consequences

- **Rename across the boundary.** A shared folder's container is its name at
  creation and never changes. Renaming a pilot folder out of the prefix, or
  renaming a real folder into it, makes it leave (or never join) the pilot:
  name and container must both match. RAST then does nothing to its
  container or its groups (least access, data kept): the groups keep the
  pilot members they had, nobody is added or removed, and no repair or
  delete hook runs. Create test shared folders with their final `t-` name.
- **Upgrade order.** The configuration rejects unknown keys, so a release
  that does not know `native_pilot_prefixes` refuses to start with it. Add
  the key to `/etc/rast/rast.toml` only **after** updating to a binary that
  knows it, and remove it before a rollback to an older one.
- **Ending the pilot.** Removing the key gives every account the native
  features at the next start, so real accounts must be migrated (or the key
  kept) first. Leftover state of real accounts (an Xpra unit adopted at
  start, an old group membership) stays harmless while the pilot is on: the
  proxy and the routes refuse them, and nothing is changed.
- No migration, no new dependency, no API break: the API change is additive
  (flags false and 403/404 only when the key is set). Tests cover the
  configuration, every gated path and the CLI binary.
