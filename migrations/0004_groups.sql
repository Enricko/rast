-- NAS-style user groups and access policy (RAST v0.2.0).
--
-- Purely additive migration: creates new tables only, so existing
-- installations upgrade in place without touching users, shares or ACL rows.
-- The permission evaluator falls back to the legacy `share_members` ACL for
-- entries that have no new rules, which keeps existing effective access
-- identical after the upgrade.

CREATE TABLE groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    description TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX idx_group_members_user ON group_members(user_id);

-- Folder rules reference a shared folder by display name (COLLATE NOCASE).
-- Shares are the only folder namespace governed by the new policy; personal
-- homes keep their ownership rules.
CREATE TABLE group_folder_rules (
    group_id   INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    share_name TEXT    NOT NULL COLLATE NOCASE,
    rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
    PRIMARY KEY (group_id, share_name)
);
CREATE INDEX idx_group_folder_rules_share ON group_folder_rules(share_name);

-- Applications are referenced by their stable ids:
-- files | applications | monitor | backup | logs | controlpanel
CREATE TABLE group_app_rules (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    app_id   TEXT    NOT NULL,
    rule     TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'allow')),
    PRIMARY KEY (group_id, app_id)
);
CREATE INDEX idx_group_app_rules_app ON group_app_rules(app_id);

-- Direct per-user rules (explicit denials/grants) evaluated together with the
-- rules of every group the user belongs to.
CREATE TABLE user_folder_rules (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    share_name TEXT    NOT NULL COLLATE NOCASE,
    rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, share_name)
);
CREATE INDEX idx_user_folder_rules_share ON user_folder_rules(share_name);

CREATE TABLE user_app_rules (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id     TEXT    NOT NULL,
    rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'allow')),
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, app_id)
);
CREATE INDEX idx_user_app_rules_app ON user_app_rules(app_id);
