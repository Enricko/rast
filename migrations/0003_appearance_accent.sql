-- Accent colour presets for the per-user appearance settings (RAST v0.1.6).
--
-- Additive migration: one nullable-free column with a default, so existing rows
-- (and existing installations upgrading in place) keep a valid accent.

ALTER TABLE user_appearance ADD COLUMN accent TEXT NOT NULL DEFAULT 'ocean'
    CHECK (accent IN (
        'ocean', 'forest', 'teal', 'sage', 'amber',
        'terracotta', 'rose', 'plum', 'indigo', 'graphite'
    ));
