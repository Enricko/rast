# Terminal

The **Terminal** app gives RAST administrators a root shell on the server in
the browser, with a tab per shell. It is **off** until the server owner turns
it on from the server itself:

```sh
sudo rast terminal enable    # install and start rast-terminal.service
sudo rast terminal status    # on/off, who the shells run as, how many are open
sudo rast terminal disable   # stop and remove it (open shells end)
```

`rast doctor` shows the same status. The uninstaller removes the service too.

## Who can use it

Only administrators. Terminal is not one of the applications a group rule can
grant (it is not in `APP_IDS`), so no group, per-user override or App Center
setting can open it to other accounts. When an administrator is changed to a
user or deactivated, their shells are ended.

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

For development, `RAST_TERMINAL_SOCKET` moves the socket and
`RAST_TERMINAL_PEER_USER` names the account allowed to connect (the helper's
shells then run as whoever starts it).
