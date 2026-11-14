# Terminal (native PTY shells)

RAST's Terminal is a first-class desktop application that opens an interactive
Linux shell **on the server**, directly through a pseudo-terminal. It does not
use Docker, a VM, Xpra, VNC or a desktop image: the browser renders xterm.js
and websockets move bytes to a real PTY owned by a per-user process.

This is separate from the Docker container terminal (Applications → container →
Terminal), which keeps working unchanged.

## What it looks like

* One desktop window with terminal tabs (multiple tabs per window; the
  launcher can open more Terminal windows).
* Toolbar: new tab, clear, copy, paste, search, font size, reconnect status and
  the **runtime environment** panel.
* Visible `connecting → connected → reconnecting → disconnected / exited`
  states; a dropped websocket reconnects to the *same* shell for the configured
  grace period instead of spawning a duplicate process.
* File Station folders get an **Open in Terminal** context-menu entry; the
  shell starts in that folder.

## Architecture

```
browser (xterm.js)
   │  authenticated, same-origin WebSocket + CSRF token
   ▼
RAST server (uid rast)  ── Unix socket (JSON helper protocol) ──▶ rašt-native.service (root)
   │  start/stop a terminal for userId, resolve folder                 │
   │                                                                   ├─ ensure useradd rast-u<id>
   │  connect to the per-session agent socket                          ├─ grant OS ACL from RAST perms
   ▼                                                                   ├─ systemd-run transient unit:
agent process (uid 200000+id, gid rast-u<id>)                            rast terminal-agent
   ├─ fresh PTY (posix_openpt), controlling terminal                     uid=identity, cgroup limits
   ├─ approved shell (bash, sh fallback), HOME of the identity           hardening, no WEA
   ├─ SIGWINCH via TIOCSWINSZ / ioctls
   └─ streams frames to the server; bounded queues = backpressure
```

* **Identity**: reused from the native Linux apps mapping
  (`rast-u<userId>`, uid = `linux_uid_base + userId`, private home under
  `<data_dir>/linux-users/<id>`). The accounts stay provisioned with a
  `nologin` shell and are not usable for external login: the authenticated PTY
  explicitly execs an approved shell. Opening Terminal never starts Xpra and
  does not require `native_enabled` or the Linux apps feature.
* **Permissions**: the app id is `terminal`. Administrators may use it;
  ordinary accounts are denied until a group grant allows it (Control Panel →
  Groups → application access). An administrator gets an unprivileged shell —
  no sudo/root. The identity has no extra groups (`docker` is not reachable).
* **Secrets**: the agent and shell run in a fresh, explicitly constructed
  environment. The RAST data directory, `secret.key`, the helper/worker
  sockets and the Docker socket are not readable by the identity (ownership
  and mode, plus systemd hardening: `NoNewPrivileges`, `ProtectSystem=full`,
  `RestrictSUIDSGID`, …).
* **Folder mapping**: File Station "Open in Terminal" passes a *RAST logical
  folder reference*, never a POSIX path. The server resolves it with the normal
  storage ACLs; the helper then grants the identity an OS ACL on that folder —
  `rX` for read-only RAST access, `rwX` for write/admin access — and makes the
  ancestor directories traversable. A shell therefore cannot read folders the
  account may not open in File Station. Folders that are not opened once remain
  inaccessible from the shell until an administrator changes their RAST access
  or they are opened in a terminal once (the ACL is grant-only and idempotent).
* **Lifecycle**: sessions live in the server's memory. Closing a tab terminates
  that process tree (SIGTERM, then SIGKILL after a short grace). A network
  interruption keeps the shell alive for `terminal_grace_seconds`; reconnecting
  attaches to the existing PTY. Signing out, disabling the account or revoking
  the Terminal permission closes the affected sessions within 30 seconds.
  Server shutdown stops the transient units through systemd (they are
  `KillMode=control-group`), never touching other users' processes.

## Limits and security

* `terminal_max_sessions` per user, `terminal_memory_mb`, `terminal_cpus`,
  `terminal_tasks` per session (systemd `MemoryMax`, `CPUQuota`, `TasksMax`).
* Output is streamed in bounded frames and a slow browser applies TCP
  backpressure to the agent (the shell blocks instead of the server buffering
  unbounded output). A reconnecting browser receives at most 256 KiB of recent
  output.
* WebSocket: same-origin check, session cookie, CSRF token in the first
  message, 1 MiB maximum client frame; the signed-in session and app permission
  are re-checked every 30 seconds.
* Every helper operation re-validates the user id, session id, shell allow-list,
  environment variable allow-list, working directory, limits and unit name.

## Shell environment and runtimes

New tabs receive a composed environment:

* `HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM=xterm-256color`,
  `XDG_RUNTIME_DIR` and `TMPDIR` in the session's private directory,
  `XDG_CONFIG_HOME`/`XDG_CACHE_HOME`/`XDG_DATA_HOME`/`XDG_STATE_HOME` under the
  identity's private home.
* `PATH` from the selected managed runtimes (Rust, Java/Temurin, Bun, Deno,
  .NET, Python, PHP, Go, Node.js) followed by the standard system directories,
  plus `JAVA_HOME`, `DOTNET_ROOT`, etc. for the selected versions.
* Per-user defaults live in the Terminal environment panel (stored in RAST's
  settings, not in a shared file). A tab can override a version for its own
  shell. Shared runtime trees are read-only; caches (`CARGO_HOME`,
  `NUGET_PACKAGES`, `DENO_DIR`, `BUN_INSTALL`, `DOTNET_CLI_HOME`, …) are set to
  the identity's private home.
* Existing tabs do not change underneath the user: after changing a default or
  override, open a new tab (the panel says so). Nothing is injected into a
  running shell.
* The shell/agent process is not hardened with `MemoryDenyWriteExecute` and
  does not inherit the `NODE_OPTIONS=--jitless` workaround used by App Center's
  package checks, so Java/Bun/Deno/.NET JITs run normally. Service-wide
  hardening is unchanged.

### Rust and rustup

Managed Rust prefixes are standalone toolchains (not rustup shims). If
`~/.cargo/bin` (rustup) is on the user's `PATH`, the managed `bin` directory is
prepended, so the selected managed `rustc`/`cargo` win. `rust-toolchain.toml`
is a rustup feature and is inert for a standalone toolchain: a project that
needs `rust-toolchain.toml` handling should be built with rustup (or a managed
toolchain version matching the file). `CARGO_HOME` stays in the private home,
so registry/download caches are per user. A missing C linker (`cc`/`gcc`/
`clang`) is reported in the environment panel.

### Java, .NET, Bun, Deno

* Java: `JAVA_HOME` points at the selected Temurin JDK root; `PATH` contains
  its `bin` directory (`java`, `javac`, `jar`, `jshell` where the distribution
  ships them).
* .NET: `DOTNET_ROOT` points at the selected SDK root; `dotnet --version`,
  `--list-sdks` and `--list-runtimes` work. Missing `libicu`/`libssl` are shown
  in the panel.
* Bun/Deno: single-binary archives; the directory containing the binary is on
  `PATH`, module caches are per-user.

## Requirements

* Linux host with the privileged native helper installed and running
  (`rast-native.service`). The helper is also used by native Linux apps, but
  Terminal only needs its identity/ACL/unit operations: Docker, Xpra, the
  `nftables` display-port firewall and the graphical session are *not* used.
* `systemd-run`, `useradd`/`groupadd`, `setfacl` (package `acl`), bash or
  dash/sh.
* If you disabled native Linux apps with `linux_apps_enabled = false`, that is
  fine; keep the helper unit installed. Do not set `terminal_enabled = false`
  unless you want to remove the Terminal entirely.

## Configuration

```toml
[terminal]
```

Settings are flat file keys (see `deploy/rast.toml.example`):

| Key | Default | Meaning |
|---|---|---|
| `terminal_enabled` | `true` | Offer the Terminal feature (permission rules still apply). |
| `terminal_max_sessions` | `8` | Maximum concurrent terminals per account. |
| `terminal_grace_seconds` | `30` | How long a disconnected tab may reconnect to the same shell. |
| `terminal_idle_minutes` | `240` | Close sessions with no viewer after this idle time (0 = only the grace period). |
| `terminal_memory_mb` | `1024` | Memory limit per terminal (systemd `MemoryMax`). |
| `terminal_cpus` | `2` | CPU limit per terminal (`CPUQuota` = 100× this). |
| `terminal_tasks` | `512` | Process/thread limit per terminal (`TasksMax`). |

Environment equivalents: `RAST_TERMINAL_ENABLED`, `RAST_TERMINAL_MAX_SESSIONS`,
`RAST_TERMINAL_GRACE_SECONDS`, `RAST_TERMINAL_IDLE_MINUTES`,
`RAST_TERMINAL_MEMORY_MB`, `RAST_TERMINAL_CPUS`, `RAST_TERMINAL_TASKS`.

## Tests

* Rust unit tests: frame protocol, environment composition, version ordering,
  config parsing, helper validators.
* Host integration: `cargo test` spawns a real PTY agent as the current user
  and exercises input, output, resize, signals, TUI output and cleanup.
* The browser flow (login → Terminal → command → resize → reconnect → File
  Station open-in-terminal) is validated manually on a disposable instance
  (see the repository report) because it needs the root helper and a browser:
  `tests/e2e/terminal-native.mjs` (20 checks + screenshots) and
  `tests/e2e/terminal-bench.mjs` (start-to-ready, input round-trip and bulk
  output measurements).
