-- Linux desktop apps (schema v10).
--
-- An administrator installs a Linux app from a .deb, an AppImage or a portable
-- archive; the Docker helper builds it into the image rast-linux/<id>:<build>
-- and runs one container per person when it is opened. This table records the
-- app, what its image said about itself and where its updates come from.
-- Running copies are not stored: they end when RAST restarts.

CREATE TABLE linux_apps (
    id             TEXT    PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 40),
    name           TEXT    NOT NULL,
    format         TEXT    NOT NULL CHECK (format IN ('deb', 'appimage', 'tarball')),
    -- Name of the uploaded file (shown in details; versions are read from it
    -- when the app does not say).
    file_name      TEXT    NOT NULL,
    version        TEXT,
    -- Debian package name (.deb apps), used for apt update checks.
    package        TEXT,
    description    TEXT,
    -- JSON array: the command the desktop session runs.
    command        TEXT    CHECK (command IS NULL OR json_valid(command)),
    -- 1 when the program to start was guessed from an archive's contents.
    launch_guess   INTEGER NOT NULL DEFAULT 0,
    icon_type      TEXT    CHECK (icon_type IS NULL OR icon_type IN ('png', 'svg')),
    icon           BLOB,
    -- Current image build; 0 until the first build succeeds.
    build          INTEGER NOT NULL DEFAULT 0,
    status         TEXT    NOT NULL CHECK (status IN ('installing', 'ready', 'updating', 'failed')),
    error          TEXT,
    job_id         TEXT,
    -- Official update source: 'apt' (the package's own apt repository),
    -- 'github' or 'zsync' (AppImage update information); NULL when unknown.
    update_kind    TEXT    CHECK (update_kind IS NULL OR update_kind IN ('apt', 'github', 'zsync')),
    update_ref     TEXT    CHECK (update_ref IS NULL OR json_valid(update_ref)),
    -- SHA-1 of the installed AppImage (zsync update checks compare it).
    file_sha1      TEXT,
    latest_version TEXT,
    checked_at     INTEGER,
    check_error    TEXT,
    memory_mb      INTEGER NOT NULL DEFAULT 4096 CHECK (memory_mb BETWEEN 512 AND 65536),
    cpus           INTEGER NOT NULL DEFAULT 6 CHECK (cpus BETWEEN 1 AND 64),
    size_bytes     INTEGER,
    created_by     INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
