-- Open windows per account (schema v10).
--
-- desktop_sessions: the account's desktop session, saved by the browser as
-- windows open, move and close, so a reload, another tab or another device
-- shows the same windows (apps, places, sizes, minimized/maximized/snapped,
-- stacking order and where each app was). One JSON object per account,
-- validated by the desktop API.

CREATE TABLE desktop_sessions (
    user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    state      TEXT    NOT NULL
                       CHECK (json_valid(state) AND json_type(state) = 'object'),
    updated_at INTEGER NOT NULL
);
