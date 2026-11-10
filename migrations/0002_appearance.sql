-- Per-user appearance settings and uploaded wallpapers (RAST v0.1.4).
--
-- Additive migration: creates new tables only, so existing installations can
-- upgrade in place without touching user data.

CREATE TABLE user_appearance (
    user_id        INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    theme          TEXT    NOT NULL DEFAULT 'light'
                           CHECK (theme IN ('light', 'dark', 'system')),
    wallpaper_kind TEXT    NOT NULL DEFAULT 'builtin'
                           CHECK (wallpaper_kind IN ('builtin', 'upload', 'color')),
    builtin_id     TEXT    NOT NULL DEFAULT 'alpine-dawn',
    upload_id      TEXT,
    color          TEXT    NOT NULL DEFAULT '#1B2430',
    fit            TEXT    NOT NULL DEFAULT 'cover'
                           CHECK (fit IN ('cover', 'contain', 'center')),
    position       TEXT    NOT NULL DEFAULT 'center',
    overlay        INTEGER NOT NULL DEFAULT 20
                           CHECK (overlay BETWEEN 0 AND 60),
    updated_at     INTEGER NOT NULL
);

CREATE TABLE appearance_uploads (
    id         TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mime       TEXT    NOT NULL,
    width      INTEGER NOT NULL,
    height     INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    file_name  TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_appearance_uploads_user ON appearance_uploads(user_id, created_at DESC);
