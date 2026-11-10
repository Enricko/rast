-- RAST initial schema
-- All timestamps are Unix epoch milliseconds (INTEGER) unless noted.

CREATE TABLE users (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    username             TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    display_name         TEXT    NOT NULL DEFAULT '',
    password_hash        TEXT    NOT NULL,
    role                 TEXT    NOT NULL CHECK (role IN ('admin', 'user')),
    active               INTEGER NOT NULL DEFAULT 1,
    quota_bytes          INTEGER NOT NULL DEFAULT 0,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    failed_attempts      INTEGER NOT NULL DEFAULT 0,
    locked_until         INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    last_login_at        INTEGER
);
CREATE INDEX idx_users_role_active ON users(role, active);

CREATE TABLE sessions (
    id           TEXT    PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   BLOB    NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    ip           TEXT    NOT NULL DEFAULT '',
    user_agent   TEXT    NOT NULL DEFAULT '',
    revoked_at   INTEGER
);
CREATE UNIQUE INDEX idx_sessions_token ON sessions(token_hash);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);

CREATE TABLE login_attempts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    username   TEXT    NOT NULL,
    ip         TEXT    NOT NULL DEFAULT '',
    success    INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_login_attempts_ip ON login_attempts(ip, created_at);
CREATE INDEX idx_login_attempts_user ON login_attempts(username, created_at);

CREATE TABLE audit_logs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at  INTEGER NOT NULL,
    user_id     INTEGER,
    username    TEXT    NOT NULL DEFAULT '',
    action      TEXT    NOT NULL,
    object_type TEXT    NOT NULL DEFAULT '',
    object_id   TEXT    NOT NULL DEFAULT '',
    object_path TEXT    NOT NULL DEFAULT '',
    result      TEXT    NOT NULL,
    detail      TEXT    NOT NULL DEFAULT '',
    ip          TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX idx_audit_created ON audit_logs(created_at DESC);
CREATE INDEX idx_audit_user ON audit_logs(user_id, created_at DESC);
CREATE INDEX idx_audit_action ON audit_logs(action, created_at DESC);

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE shares (
    id             TEXT    PRIMARY KEY,
    name           TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    description    TEXT    NOT NULL DEFAULT '',
    container_path TEXT    NOT NULL,
    created_by     INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE share_members (
    share_id TEXT    NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    perms    TEXT    NOT NULL DEFAULT 'read' CHECK (perms IN ('read', 'write', 'admin')),
    PRIMARY KEY (share_id, user_id)
);
CREATE INDEX idx_share_members_user ON share_members(user_id);

CREATE TABLE storage_usage (
    user_id        INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    used_bytes     INTEGER NOT NULL DEFAULT 0,
    reserved_bytes INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE trash (
    id            TEXT    PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    original_path TEXT    NOT NULL,
    trashed_path  TEXT    NOT NULL,
    size_bytes    INTEGER NOT NULL DEFAULT 0,
    kind          TEXT    NOT NULL DEFAULT 'file',
    deleted_at    INTEGER NOT NULL
);
CREATE INDEX idx_trash_user ON trash(user_id, deleted_at DESC);

CREATE TABLE jobs (
    id          TEXT    PRIMARY KEY,
    kind        TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed', 'cancelled')),
    progress    INTEGER NOT NULL DEFAULT 0,
    total       INTEGER NOT NULL DEFAULT 0,
    message     TEXT    NOT NULL DEFAULT '',
    user_id     INTEGER,
    username    TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    result      TEXT,
    error       TEXT
);
CREATE INDEX idx_jobs_created ON jobs(created_at DESC);
CREATE INDEX idx_jobs_status ON jobs(status);

CREATE TABLE apps (
    id         TEXT    PRIMARY KEY,
    name       TEXT    NOT NULL,
    slug       TEXT    NOT NULL UNIQUE,
    compose    TEXT    NOT NULL,
    env_json   TEXT    NOT NULL DEFAULT '[]',
    status     TEXT    NOT NULL DEFAULT 'stopped',
    last_error TEXT    NOT NULL DEFAULT '',
    created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE app_deployments (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id      TEXT    NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    action      TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    detail      TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX idx_app_deployments_app ON app_deployments(app_id, created_at DESC);

CREATE TABLE backups (
    id          TEXT    PRIMARY KEY,
    file_name   TEXT    NOT NULL,
    path        TEXT    NOT NULL,
    size_bytes  INTEGER NOT NULL DEFAULT 0,
    sha256      TEXT    NOT NULL DEFAULT '',
    scope       TEXT    NOT NULL DEFAULT '[]',
    status      TEXT    NOT NULL DEFAULT 'running',
    note        TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    finished_at INTEGER,
    error       TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX idx_backups_created ON backups(created_at DESC);

CREATE TABLE backup_schedules (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    enabled        INTEGER NOT NULL DEFAULT 1,
    frequency      TEXT    NOT NULL DEFAULT 'daily' CHECK (frequency IN ('hourly', 'daily', 'weekly')),
    time_of_day    TEXT    NOT NULL DEFAULT '03:00',
    weekday        INTEGER NOT NULL DEFAULT 0,
    retention_count INTEGER NOT NULL DEFAULT 7,
    scopes         TEXT    NOT NULL DEFAULT '["db","config"]',
    last_run_at    INTEGER,
    next_run_at    INTEGER NOT NULL DEFAULT 0
);
