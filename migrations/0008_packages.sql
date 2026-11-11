-- Package Center: managed native runtimes (Python, PHP CLI, Go, Node.js, and
-- third-party runtime packages imported from GitHub releases).
--
-- Installed files live under <data_dir>/packages/runtimes/<package>/<version>/;
-- these tables record what is installed, which version is the RAST default,
-- which consumers pin a version, which GitHub sources an administrator
-- approved, and which jobs belong to which package.

-- One row per installed package (any number of versions).
CREATE TABLE packages (
    id              TEXT    PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    type            TEXT    NOT NULL CHECK (type IN ('runtime')),
    name            TEXT    NOT NULL,
    -- 'official' (signed catalog) or 'github' (approved third-party release).
    source          TEXT    NOT NULL CHECK (source IN ('official', 'github')),
    -- owner/repo for GitHub imports, NULL for the official catalog.
    source_repo     TEXT,
    publisher_id    TEXT    NOT NULL,
    publisher_name  TEXT    NOT NULL,
    default_version TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- Installed versions, side by side. The manifest is the exact (validated)
-- document the version was installed from, so details survive catalog changes.
CREATE TABLE package_versions (
    package_id      TEXT    NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    version         TEXT    NOT NULL,
    revision        INTEGER NOT NULL,
    target          TEXT    NOT NULL,
    manifest_json   TEXT    NOT NULL CHECK (json_valid(manifest_json)),
    artifact_url    TEXT    NOT NULL,
    artifact_sha256 TEXT    NOT NULL,
    -- GitHub release pin for imports (tag), NULL for catalog installs.
    release_tag     TEXT,
    install_path    TEXT    NOT NULL,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    check_output    TEXT    NOT NULL DEFAULT '',
    installed_by    TEXT    NOT NULL DEFAULT '',
    installed_at    INTEGER NOT NULL,
    PRIMARY KEY (package_id, version)
);

-- Consumers that use a runtime: either follow the package default
-- (version NULL) or stay pinned to an explicit version. Pinned versions and
-- the default of a followed package cannot be removed.
CREATE TABLE package_consumers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE COLLATE NOCASE CHECK (length(name) BETWEEN 1 AND 64),
    description TEXT    NOT NULL DEFAULT '',
    package_id  TEXT    NOT NULL REFERENCES packages(id) ON DELETE RESTRICT,
    version     TEXT,
    created_by  TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX idx_package_consumers_package ON package_consumers(package_id);

-- GitHub repositories an administrator approved as package sources.
CREATE TABLE package_sources (
    repo          TEXT    PRIMARY KEY COLLATE NOCASE,
    publisher_id  TEXT    NOT NULL,
    approved_by   TEXT    NOT NULL,
    approved_at   INTEGER NOT NULL
);

-- "Install from GitHub", step 1: the resolved release and validated manifest
-- the administrator previewed. Step 2 installs exactly this (release + digest).
CREATE TABLE package_inspections (
    id              TEXT    PRIMARY KEY,
    input_url       TEXT    NOT NULL,
    repo            TEXT    NOT NULL,
    release_tag     TEXT    NOT NULL,
    release_id      INTEGER NOT NULL,
    manifest_json   TEXT    NOT NULL CHECK (json_valid(manifest_json)),
    manifest_sha256 TEXT    NOT NULL,
    created_by      INTEGER,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL
);

-- Which background job did what to which package (job history in details).
CREATE TABLE package_jobs (
    job_id     TEXT    PRIMARY KEY,
    package_id TEXT    NOT NULL,
    version    TEXT    NOT NULL DEFAULT '',
    action     TEXT    NOT NULL CHECK (action IN ('install', 'update', 'reinstall')),
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_package_jobs_package ON package_jobs(package_id, created_at);
