-- no-transaction
-- App Center app packages: allow type = 'app' next to 'runtime'.
--
-- SQLite cannot alter a CHECK constraint, so the table is rebuilt. Foreign
-- keys are disabled for the copy (the child tables reference packages(id);
-- the final schema and rows are unchanged apart from the allowed type).
PRAGMA foreign_keys = OFF;
BEGIN;
CREATE TABLE packages_new (
    id              TEXT    PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    type            TEXT    NOT NULL CHECK (type IN ('runtime', 'app')),
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
INSERT INTO packages_new (id, type, name, source, source_repo, publisher_id,
                          publisher_name, default_version, created_at, updated_at)
    SELECT id, type, name, source, source_repo, publisher_id, publisher_name,
           default_version, created_at, updated_at FROM packages;
DROP TABLE packages;
ALTER TABLE packages_new RENAME TO packages;
COMMIT;
PRAGMA foreign_keys = ON;
