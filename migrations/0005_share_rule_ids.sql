-- Stable share ids for folder rules (hardening 0005).
--
-- Before this migration folder rules referenced shared folders by display
-- name, so renaming a share had to rewrite every rule (and could silently drop
-- conflicting rows). Rules now reference `shares.id`, which is stable across
-- renames and cascades on delete.
--
-- Rows whose share name no longer matches a share are dropped (they had no
-- effect) but counted in `settings` for the release handoff:
--   migration.0005.orphaned_rules  -- name-keyed rows dropped (no matching share)
--   migration.0005.conflicts       -- matched rows dropped as duplicates
--
-- SQLite cannot alter a primary key in place, so the tables are rebuilt.

CREATE TABLE group_folder_rules_v2 (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    share_id TEXT    NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
    rule     TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
    PRIMARY KEY (group_id, share_id)
);

CREATE TABLE user_folder_rules_v2 (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    share_id   TEXT    NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
    rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, share_id)
);

-- Map name-keyed rules to the owning share. shares.name is COLLATE NOCASE and
-- unique, so a name maps to at most one share; INSERT OR IGNORE also guards
-- against duplicate collisions (counted below).
INSERT OR IGNORE INTO group_folder_rules_v2 (group_id, share_id, rule)
SELECT r.group_id, s.id, r.rule
FROM group_folder_rules r
JOIN shares s ON s.name = r.share_name COLLATE NOCASE;

INSERT OR IGNORE INTO user_folder_rules_v2 (user_id, share_id, rule, updated_at)
SELECT r.user_id, s.id, r.rule, r.updated_at
FROM user_folder_rules r
JOIN shares s ON s.name = r.share_name COLLATE NOCASE;

-- Record migration statistics before the old tables are dropped.
-- orphaned = old rows whose name matched no share.
INSERT OR REPLACE INTO settings (key, value, updated_at)
SELECT
    'migration.0005.orphaned_rules',
    CAST(
        ((SELECT COUNT(*) FROM group_folder_rules) + (SELECT COUNT(*) FROM user_folder_rules))
        - (
            (SELECT COUNT(*) FROM group_folder_rules r
             JOIN shares s ON s.name = r.share_name COLLATE NOCASE)
            + (SELECT COUNT(*) FROM user_folder_rules r
               JOIN shares s ON s.name = r.share_name COLLATE NOCASE)
        )
        AS TEXT
    ),
    (CAST(strftime('%s', 'now') AS INTEGER) * 1000);

-- conflicts = matched rows that collided on (owner, share_id) and were dropped.
INSERT OR REPLACE INTO settings (key, value, updated_at)
SELECT
    'migration.0005.conflicts',
    CAST(
        (
            (SELECT COUNT(*) FROM group_folder_rules r
             JOIN shares s ON s.name = r.share_name COLLATE NOCASE)
            + (SELECT COUNT(*) FROM user_folder_rules r
               JOIN shares s ON s.name = r.share_name COLLATE NOCASE)
        )
        - (
            (SELECT COUNT(*) FROM group_folder_rules_v2)
            + (SELECT COUNT(*) FROM user_folder_rules_v2)
        )
        AS TEXT
    ),
    (CAST(strftime('%s', 'now') AS INTEGER) * 1000);

DROP TABLE group_folder_rules;
DROP TABLE user_folder_rules;

ALTER TABLE group_folder_rules_v2 RENAME TO group_folder_rules;
ALTER TABLE user_folder_rules_v2 RENAME TO user_folder_rules;

CREATE INDEX idx_group_folder_rules_share ON group_folder_rules(share_id);
CREATE INDEX idx_user_folder_rules_share ON user_folder_rules(share_id);
