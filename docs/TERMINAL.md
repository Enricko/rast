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
agent process (ordinary users: uid 200000+id, gid rast-u<id>;            rast terminal-agent
               administrators: root, HOME=/root — cgroup limits only)
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
* **Permissions**: the app id is `terminal`. Administrators always may use it;
  ordinary accounts are denied until a group grant allows it (Control Panel →
  Groups → application access, or a direct per-user override in Effective
  Access). Selected ordinary users always run as their unprivileged
  `rast-u<id>` identity (no sudo/root, no `docker` group).
* **Administrator root shell**: by default (`terminal_admin_root = true`) a
  **super administrator's** Terminal is a real root shell (administrators
  without that level run as their unprivileged identity, like selected
  users) — `HOME=/root`, full
  filesystem and command access, including package management and system
  services. This is intentional: RAST administrators are server owners and
  already control the host through Docker/App Center. The session still runs
  in its own transient cgroup (so closing the tab ends everything it started),
  but without the resource limits and the unprivileged hardening used for
  ordinary users, so system tools work. `PATH` is what a root login has,
  including `/root/.local/bin` and `/snap/bin`. Tools the server's
  administrators installed for their own Linux account also run: for every
  login account that may already become root (a member of `sudo`, `admin` or
  `wheel`), its `~/.local/bin`, `~/bin`, `~/.cargo/bin`, `~/.bun/bin`,
  `~/.deno/bin`, `~/go/bin`, `~/.opencode/bin` and similar per-user tool
  directories are appended to the root shell's `PATH` (last, so they never
  shadow a system command; a directory another account can write to is
  skipped). Such a tool runs as root with root's own settings (`HOME=/root`),
  so for example `claude` asks root to sign in once. Other directories can be
  added with `terminal_path_extra`. Set `terminal_admin_root = false` to give super administrators the
  mapped unprivileged identity instead. Ordinary users are never granted root
  by this feature.
* **Administrator shell as a login account** (`terminal_admin_account`): set
  this to the Linux account the server is administered with over SSH (a
  member of `sudo`, `admin` or `wheel`; the helper refuses any other account)
  and an administrator's Terminal is a *login shell of that account* instead
  of root. The helper passes only what sshd/PAM would (`HOME`, `USER`,
  `SHELL`, `LANG` from `/etc/default/locale`, `PATH` from `/etc/environment`,
  `XDG_RUNTIME_DIR` when the account has a user session, and the login
  umask); `/etc/profile` and
  the account's `~/.profile`/`~/.bashrc` build the rest. The same commands,
  versions and tool sign-ins as over SSH are therefore available, and `sudo`
  gives root. The Terminal runtime selection and per-tab overrides are not
  applied to these shells. Ordinary users are unaffected.
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
  **One-HOME accounts** (after `rast migrate home`, ADR 0005) get no grant: the
  shell's HOME is their HOME, which they own, and shared folders are reached
  through their Linux groups. "Open in Terminal" accepts a folder in the HOME
  (checked by the account's file agent) or in a shared folder they can open;
  `rast migrate home` removes the grants that earlier terminals left in the
  HOME (`u:rast-u<id>:rwX`/`rX`).
* **Lifecycle**: a terminal stays open until it is closed. Closing a tab (or
  the Terminal window) terminates that process tree (SIGTERM, then SIGKILL
  after a short grace). Reloading the page, closing the browser or losing the
  network only detaches the viewer: the shell and whatever runs in it keep
  going, and Terminal shows every running shell again as a tab when it is
  opened next (the desktop reopens the Terminal window after a reload), with
  the most recent 256 KiB of output replayed. A shell nobody is viewing is
  closed after `terminal_idle_minutes` without output (default 24 hours). One
  window shows a session at a time: opening it elsewhere takes it over.
  Signing out, disabling the account or revoking the Terminal permission
  closes the affected sessions within 30 seconds, viewed or not. Sessions live
  in the server's memory, so restarting or updating RAST ends them.
  Server shutdown stops the transient units through systemd (they are
  `KillMode=control-group`), never touching other users' processes.

## Limits and security

* `terminal_max_sessions` per user, `terminal_memory_mb`, `terminal_cpus`,
  `terminal_tasks` per session (systemd `MemoryMax`, `CPUQuota`, `TasksMax`)
  for ordinary users. Administrator root shells are not limited (builds,
  package managers and tools such as coding agents need more than the default
  1 GiB / 512 tasks), like a root login over SSH.
* Administrator shells are full root shells (see Permissions); everyone else is
  confined to the mapped identity and ACL-granted folders.
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

* `HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM=xterm-256color` (administrator root
  shells use `HOME=/root`, `USER=root`; everyone else gets the identity home),
  `XDG_RUNTIME_DIR` and `TMPDIR` in the session's private directory,
  `XDG_CONFIG_HOME`/`XDG_CACHE_HOME`/`XDG_DATA_HOME`/`XDG_STATE_HOME` under the
  identity's private home.
* `PATH` from the selected managed runtimes (Python, PHP CLI, Go, Node.js,
  Rust, Eclipse Temurin JDK, Bun, Deno, .NET SDK (C#), Clang/LLVM (C/C++),
  Flutter/Dart, Composer, Zig, Kotlin, CMake, Gradle, Apache Maven) followed by
  the standard system directories, plus `JAVA_HOME`, `DOTNET_ROOT`, etc. for
  the selected versions; the account's own `~/.local/bin` and `~/bin` come
  first and `/snap/bin` last when they exist. Composer's `composer` wrapper uses the `php` that is
  first on the shell's `PATH` (managed PHP when selected, otherwise the OS
  PHP).
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
| `terminal_admin_root` | `true` | Super administrators get a root shell; administrators and selected ordinary users always get their unprivileged identity. |
| `terminal_admin_account` | unset | Run super administrators' terminals as this login account (sudo/admin/wheel member), like SSH, instead of root. |
| `terminal_path_extra` | `[]` | Directories appended to an administrator root shell's `PATH` (tools in another account's home). |
| `terminal_max_sessions` | `8` | Maximum concurrent terminals per account. |
| `terminal_grace_seconds` | `30` | Minimum time a terminal without a viewer is kept. |
| `terminal_idle_minutes` | `1440` | Close a terminal nobody is viewing after this many minutes without output (0 = right after the grace period). |
| `terminal_memory_mb` | `1024` | Memory limit per terminal of an ordinary user (systemd `MemoryMax`). |
| `terminal_cpus` | `2` | CPU limit per terminal of an ordinary user (`CPUQuota` = 100× this). |
| `terminal_tasks` | `512` | Process/thread limit per terminal of an ordinary user (`TasksMax`). |

Environment equivalents: `RAST_TERMINAL_ENABLED`, `RAST_TERMINAL_ADMIN_ROOT`, `RAST_TERMINAL_MAX_SESSIONS`,
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
