-- Persist the display backend and its verified acceleration state with the
-- session row, so a RAST restart that adopts a still-running graphical session
-- reports the real backend (and stops the privileged display on session end).
-- Software sessions keep the defaults; the columns are additive.
ALTER TABLE linux_sessions ADD COLUMN display_backend TEXT NOT NULL DEFAULT 'xvfb';
ALTER TABLE linux_sessions ADD COLUMN gpu_verified INTEGER NOT NULL DEFAULT 0;
ALTER TABLE linux_sessions ADD COLUMN renderer TEXT;
