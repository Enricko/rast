-- Discovered (externally installed) desktop applications (schema v15).
--
-- Applications installed outside the RAST App Center — a `.desktop` entry in
-- an XDG directory, an AppImage in the person's `~/Applications`, a portable
-- binary, or an app an external installer (for example ArtCraft Launcher)
-- dropped inside a managed application profile — are one *inventory*, not
-- managed installs: RAST never builds, updates or uninstalls them. Each row
-- records what was found and whether the owner (or an administrator, for
-- system-wide entries) approved it. Missing files never delete a row: the row
-- becomes `unavailable` so the owner can still see what was there.
--
-- `id` is a stable server-derived hash of (scope, owner, desktop path), so a
-- rescan updates the same row instead of duplicating it.
--
-- `exec_json` is a JSON argv array, never a shell string. Everything RAST
-- starts is executed with an explicit argv and no shell.
CREATE TABLE discovered_apps (
    id               TEXT    PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    name             TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    generic_name     TEXT,
    comment          TEXT,
    -- JSON array: the parsed Exec line (field codes stripped).
    exec_json        TEXT    NOT NULL CHECK (json_valid(exec_json)),
    -- Optional TryExec program (plain path/name, validated by the scan).
    try_exec         TEXT,
    icon             TEXT,
    -- JSON array of desktop categories (e.g. ["Office","WordProcessor"]).
    categories       TEXT    CHECK (categories IS NULL OR json_valid(categories)),
    terminal         INTEGER NOT NULL DEFAULT 0,
    startup_wm_class TEXT,
    -- Absolute path of the source: the `.desktop` file or the program itself.
    desktop_path     TEXT    NOT NULL,
    -- XDG desktop id (file name for `.desktop` entries; derived for others).
    desktop_id       TEXT,
    -- NULL = the entry is offered system-wide; otherwise only this account
    -- and administrators can see it.
    owner_user_id    INTEGER REFERENCES users(id) ON DELETE CASCADE,
    -- user | system | managed-profile
    scope            TEXT    NOT NULL CHECK (scope IN ('user', 'system', 'managed-profile')),
    -- xdg | appimage | portable | manual | external-installer
    source           TEXT    NOT NULL CHECK (source IN ('xdg', 'appimage', 'portable', 'manual', 'external-installer')),
    install_dir      TEXT,
    version          TEXT,
    -- discovered | pending_approval | ready | unavailable | failed
    status           TEXT    NOT NULL CHECK (status IN ('discovered', 'pending_approval', 'ready', 'unavailable', 'failed')),
    -- 1 once the entry may be launched by the people allowed to see it.
    approved         INTEGER NOT NULL DEFAULT 0,
    -- 1 when an administrator hid the entry from ordinary users.
    hidden           INTEGER NOT NULL DEFAULT 0,
    last_seen_ms     INTEGER NOT NULL,
    last_launched_ms INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE INDEX idx_discovered_apps_owner ON discovered_apps(owner_user_id, status);
CREATE INDEX idx_discovered_apps_scope ON discovered_apps(scope, status);
CREATE INDEX idx_discovered_apps_path ON discovered_apps(desktop_path);

-- Native runs no longer have to belong to a managed `linux_apps` row: a
-- person can start a discovered application in their shared session, and that
-- run must be persisted (and adopted after a restart) exactly like a managed
-- app's run. Everything else about `linux_runs` is unchanged; the app id is
-- still the stable identifier the session/window matching uses.
CREATE TABLE linux_runs_new (
    id         TEXT    PRIMARY KEY,
    app_id     TEXT    NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    unit       TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('launching', 'running', 'exited', 'failed')),
    error      TEXT,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER
);
INSERT INTO linux_runs_new (id, app_id, user_id, unit, status, error, started_at, ended_at)
    SELECT id, app_id, user_id, unit, status, error, started_at, ended_at FROM linux_runs;
DROP TABLE linux_runs;
ALTER TABLE linux_runs_new RENAME TO linux_runs;
CREATE INDEX idx_linux_runs_user ON linux_runs(user_id, status);
CREATE INDEX idx_linux_runs_app ON linux_runs(app_id, status);
