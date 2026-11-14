-- Native Linux apps (schema v12).
--
-- Linux desktop apps no longer run in a Docker image that is built per app:
-- they run natively on the server, in one shared Xpra seamless graphical
-- session per RAST user. This migration records the execution backend, the
-- native install metadata (package ownership, launcher, compatibility), the
-- per-user unprivileged host identities and the running sessions/apps, and
-- keeps the Docker backend available for apps that have not migrated yet.

ALTER TABLE linux_apps ADD COLUMN backend TEXT NOT NULL DEFAULT 'docker'
    CHECK (backend IN ('docker', 'native'));
-- Where a portable or AppImage install lives (managed application directory).
ALTER TABLE linux_apps ADD COLUMN install_path TEXT;
-- Previous portable version directory, kept for rollback.
ALTER TABLE linux_apps ADD COLUMN previous_path TEXT;
-- JSON: the exact launcher (argv, desktop entry metadata, working directory).
ALTER TABLE linux_apps ADD COLUMN launcher TEXT
    CHECK (launcher IS NULL OR json_valid(launcher));
ALTER TABLE linux_apps ADD COLUMN architecture TEXT;
-- `.deb` installs: version read from dpkg after install; 1 when the package
-- was already installed before RAST adopted it (uninstall must not remove it).
ALTER TABLE linux_apps ADD COLUMN package_version TEXT;
ALTER TABLE linux_apps ADD COLUMN package_preexisting INTEGER;
-- JSON array: package names the application declares it needs.
ALTER TABLE linux_apps ADD COLUMN dependencies TEXT
    CHECK (dependencies IS NULL OR json_valid(dependencies));
-- JSON array: packages the install transaction added because of this app.
-- Uninstall removes only these (and only when nothing else needs them).
ALTER TABLE linux_apps ADD COLUMN packages_added TEXT
    CHECK (packages_added IS NULL OR json_valid(packages_added));
-- JSON: install preview / compatibility result {ok, reasons[]}.
ALTER TABLE linux_apps ADD COLUMN compatibility TEXT
    CHECK (compatibility IS NULL OR json_valid(compatibility));
-- How the native files were obtained: file|url|migrated.
ALTER TABLE linux_apps ADD COLUMN install_source TEXT;
-- Docker -> native migration state and the retained recovery data.
-- pending -> prepared -> migrated | failed; rolled_back restores Docker.
ALTER TABLE linux_apps ADD COLUMN migration_state TEXT;
ALTER TABLE linux_apps ADD COLUMN migration_error TEXT;
-- Profile backup directory and/or retained Docker volume name.
ALTER TABLE linux_apps ADD COLUMN migration_backup TEXT;
ALTER TABLE linux_apps ADD COLUMN migrated_at INTEGER;
-- Legacy Docker volume kept while a migrated app can still be rolled back.
ALTER TABLE linux_apps ADD COLUMN docker_volume TEXT;

CREATE INDEX idx_linux_apps_backend ON linux_apps(backend, status);

-- One unprivileged host identity per RAST user.
-- uid = linux_uid_base + user_id (see config), gid = uid, home under
-- <data_dir>/linux-users/<user_id>.
CREATE TABLE linux_identities (
    user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    username   TEXT    NOT NULL UNIQUE,
    uid        INTEGER NOT NULL UNIQUE,
    gid        INTEGER NOT NULL,
    home       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);

-- One shared graphical session per person, started lazily on the first
-- application launch. `key` is the secret part of the session's proxy URL;
-- it is persisted so a restart of RAST can adopt the still-running session.
CREATE TABLE linux_sessions (
    user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    key        TEXT    NOT NULL UNIQUE,
    display    INTEGER NOT NULL,
    port       INTEGER NOT NULL,
    unit       TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('starting', 'ready', 'stopping')),
    started_at INTEGER NOT NULL,
    last_input INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Applications running inside a person's session. Window state lives in the
-- browser; this table tells the server (and App Center) what is running and
-- lets a restarted server reconcile systemd units.
CREATE TABLE linux_runs (
    id         TEXT    PRIMARY KEY,
    app_id     TEXT    NOT NULL REFERENCES linux_apps(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    unit       TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('launching', 'running', 'exited', 'failed')),
    error      TEXT,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER
);
CREATE INDEX idx_linux_runs_user ON linux_runs(user_id, status);
CREATE INDEX idx_linux_runs_app ON linux_runs(app_id, status);
