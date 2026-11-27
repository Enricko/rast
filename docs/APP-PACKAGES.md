# App packages

An `app` package is a desktop app that App Center installs from the official
catalog: a window, and optionally a server program RAST runs next to itself.
AI Agent is the first (package `ai-agent`, app id `agent`), so it updates
through App Center instead of RAST releases.

## Manifest

A package manifest (`docs/rast-package.schema.json`) with `"type": "app"` and
an `app` section; the `runtime` section is optional:

```json
"app": {
  "id": "agent",
  "window": {
    "entry": "ui/app.js",
    "styles": ["ui/app.css"],
    "sdk": 1,
    "singleton": true,
    "defaultSize": {"w": 1080, "h": 720}
  },
  "server": {
    "executable": "bin/rast-ai-agent",
    "hostApi": 1,
    "apiPrefix": "/api/ai",
    "eventPrefix": "ai."
  }
}
```

- `id`: the desktop app id. A package app with a built-in app's id takes its
  place (its window, its `/api/<segment>`, its access rules).
- `window.entry`: an ES module whose default export is `{ mount(ctx, sdk) }`;
  RAST serves the entry's folder at
  `/api/appfiles/<appId>/<version>/<revision>/…` (immutable per version).
- `server.apiPrefix`: `/api/<segment>`, a segment RAST does not use itself
  (`HOST_API_SEGMENTS` in `src/packages/manifest.rs`).
- `server.eventPrefix`: the only live event kinds and audit actions the app
  may publish.

## The server program

RAST starts the default version's `executable` as its own user, with a
cleared environment and restarts (1 s up to 60 s apart) when it stops:

| Variable | Meaning |
| --- | --- |
| `RAST_APP_SOCKET` | where the app listens (HTTP/1.1 over a Unix socket) |
| `RAST_HOST_SOCKET` | RAST's host API |
| `RAST_APP_TOKEN` | this start's secret: RAST sends it as `X-Rast-Token`, the app as `Authorization: Bearer` |
| `RAST_APP_DATA` | the app's own folder (0700), kept across updates |
| `RAST_APP_ID`, `RAST_APP_VERSION`, `RAST_VERSION`, `RAST_HOST_API` | what runs |

Requests below `apiPrefix` reach the app after RAST's sign-in, CSRF and access
checks, without cookies or `Authorization`, with `X-Rast-Token`,
`X-Rast-User` (JSON `{id, username, displayName, role, admin}`) and
`X-Rast-Client-Ip`. Cookies the app sets stay out of the browser.

## Host API v1

Over `host.sock`, each request with the app's token. The routes and their
answers are listed in `src/apphost/host_api.rs`: info (RAST's version, the
app's folders, RAST's private paths), accounts, groups, an account's app
access, live events (`{events:[…]}` keeps their order), audit entries, File
Station operations as an account (list, read, search, write, mkdir; File
Station's checks, quota and live updates), commands (as root through the
Terminal helper, or in an account's sandbox), and AI Agent's one-time data
handover (`src/apphost/migrate.rs`). A version's routes never change meaning;
new routes come with later versions.

## Window SDK v1

Before importing the window module, the desktop sets
`globalThis.__RAST_SDK__` to RAST's own `api`, `components`, `dom`,
`format`, `icons`, `realtime` and `ui` modules (`frontend/src/core/sdk.ts`).
`frontend/src/core/sdk-v1.json` pins what each exports; a test fails when
one disappears.

`frontend/scripts/build-app.mjs <entry.ts> <out dir>` bundles a window
(`app.js` + `app.css`), pointing imports of those core modules at the SDK.

## AI Agent

AI Agent's source lives in the package repository
(<https://github.com/Enricko/rast-packages/tree/main/apps/ai-agent>): a Rust
crate built against a pinned RAST commit (RAST's library gives it accounts,
the audit log, the sandbox and the host API) and a window built with
`frontend/scripts/build-app.mjs` from a RAST checkout. Its workflow there
releases `ai-agent-v<version>`; the catalog takes AI Agent's versions from
`packages.ai-agent.releaseRepos` (newest home first). RAST hands `/api/ai/*`
to the package's server while it runs; on its first start the package takes
the AI Agent data an older RAST kept (`apphost::migrate`).
