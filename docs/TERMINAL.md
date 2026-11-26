# Terminal

The **Terminal** app gives shells in the browser, with a tab per shell:

- **administrators** get a **root shell on the server** (below);
- **other accounts** an administrator grants the app get shells in **their own
  sandbox container** ([Sandboxes for other accounts](#sandboxes-for-other-accounts)).

Root shells are **off** until the server owner turns them on from the server
itself:

```sh
sudo rast terminal enable    # install and start rast-terminal.service
sudo rast terminal status    # on/off, who the shells run as, how many are open
sudo rast terminal disable   # stop and remove it (open shells end)
```

`rast doctor` shows the same status. The uninstaller removes the service too.

## Who can use it

Root shells: only administrators. A group or per-account rule that allows
Terminal never gives a shell on the server, only a sandbox. When an
administrator is changed to a user or deactivated, their root shells end
(and the server checks every minute that every open root shell still belongs
to an active administrator).

A root shell controls the whole server, including RAST's own database and
secrets: anyone who can sign in as an administrator can do anything on the
machine. Use strong passwords for administrator accounts and keep their
number small. Every shell that is opened or closed is recorded in Activity
Logs (`terminal.open`, `terminal.close`); what is typed in it is not.

## Shells keep running

Closing the window, reloading the page or losing the network only
*detaches*: the shell keeps running on the server, and opening Terminal again
shows it with its recent output (256 KiB per shell). A shell opened in a
second browser window moves there; the first window says so and can take it
back. A shell ends when:

- it exits (`exit`, Ctrl+D);
- its tab is closed (× or middle-click; asks first when a program is still
  running in it);
- nobody has looked at it and it printed nothing for 24 hours;
- its owner stops being an administrator;
- `rast-terminal.service` stops (`rast terminal disable`, or a restart).

At most 8 shells can be open on the server at once.

Updating RAST does not restart the helper, so open shells survive updates.
To run a newer helper after an update, `sudo systemctl restart
rast-terminal.service` (this ends the open shells).

## How it works

The web server runs as the unprivileged `rast` account and cannot start a root
shell by itself. `rast-terminal.service` runs `rast terminal helper` as root,
unconfined (the shells are the point of it), and listens on
`/run/rast-terminal/helper.sock`:

- the directory and the socket belong to the `rast` group (`0750`/`0660`), and
  every connection's peer uid must be the `rast` account's (or root's) — the
  Docker worker account, also in that group, is refused;
- the web server decides who is an administrator (and re-checks every
  minute while a shell is shown); the helper keeps each shell's owner and
  lets only that account list, reattach or end it;
- the browser connects with a same-origin WebSocket whose first message
  carries the session's CSRF token, like container terminals.

Each shell is a login shell (`bash -l`) of root on its own pseudo terminal,
with a clean environment (`HOME=/root`, a standard `PATH`, `TERM`,
`RAST_USER=<who opened it>`), so `/etc/profile`, `/etc/profile.d` (including
App Center's runtimes) and root's own profile apply.

The AI Agent's `run_command` tool uses the same helper (and the same sandboxes
for other accounts) through a one-off `run` request: one command in a fresh
login shell without a terminal (`TERM=dumb`, no pager, empty input), stopped
after its time limit with everything it started, answered with the exit code
and up to 64 KB of each output stream. Each call is allowed by the chat's
owner first. A helper started before this existed refuses `run`; restart it
once (`sudo systemctl restart rast-terminal.service`, which ends open
shells).

For development, `RAST_TERMINAL_SOCKET` moves the socket and
`RAST_TERMINAL_PEER_USER` names the account allowed to connect (the helper's
shells then run as whoever starts it).

## Sandboxes for other accounts

Administrators can give other accounts a terminal too: allow **Terminal** in a
group's (or one account's) application rules in Control Panel. It is denied by
default. Those accounts never get a shell on the server: each gets **its own
Docker container**, and every shell it opens runs inside it.

| | |
| --- | --- |
| Files | only the account's home folder, at `/home/<name>` (what it creates there shows up in File Station). No shared folders, nothing else of the server |
| Tools | Ubuntu 24.04 with bash, git, curl, python3 (venv, pip), the C toolchain (gcc, make, pkg-config, libc headers), nano, vim, tmux, htop, jq, sqlite3, zip and more; App Center's runtimes (Python, Node.js, Go, PHP, Rust, Java, Clang, Flutter, …) read-only and first on `PATH` |
| Network | **none.** A container on Docker's network could reach services on the server and on its private networks (LAN, VPN), so sandboxes have no network at all |
| Privileges | runs as the account that owns the home folders (not root), no capabilities, `no-new-privileges`, read-only system files, `/tmp` in memory (512 MB; programs may run from it, as from the home folder) |
| Limits | 1 GB memory, 2 CPUs, 512 processes per account; 4 shells per account, 16 on the server |

It needs Docker (`docker_enabled = true` and `rast-worker.service` running):
the Docker worker builds the image, starts the containers and runs the shells
(`docker exec`), serving them on `/run/rast/shells.sock` to the web server's
account only.

**The first time** anyone opens a sandbox, the worker builds the image
(`rast-shell:<hash>`, about 700 MB, from `src/sandbox/Dockerfile`); this
downloads Ubuntu packages and takes a few minutes. Terminal shows "Setting up
Terminal" meanwhile. A RAST update that changes the recipe builds a new image
the next time a sandbox opens.

Sandbox shells keep running across reloads like root shells. One container
per account holds all its shells (programs started in the background keep
running while any shell is open); it is removed after 15 minutes without a
shell, when the account loses access to Terminal (checked every minute), is
deactivated or becomes an administrator. Restarting or updating RAST's worker
ends the open sandbox shells. The home folder always stays.

**Quota.** RAST cannot limit what a shell writes while it runs. Opening a
shell counts the home folder again and refuses when it is over its quota.

**Safety notes.**

- Files a shell creates belong to the RAST service account on the server.
  Because a shell can set the set-user-id bit on its own files, the first
  sandbox also closes the storage folders (`<storage>/homes`,
  `<storage>/shared`) to other accounts on the server (mode `0750`).
- File Station refuses symbolic links and special files (pipes, sockets) a
  shell may create, so they cannot be used to read outside the home folder or
  to hang a download.
- The containers share the server's kernel, as all containers do. Grant
  Terminal to people you trust with code running on your server.

Opening and closing sandbox shells is recorded in Activity Logs like root
shells (with `sandbox` as the detail).
