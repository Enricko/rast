-- Desktop widgets per account (schema v6).
--
-- Additive: one column with a default, so existing rows (and installations
-- upgrading in place) start without widgets. The value is a JSON array of
-- {"id", "x", "y"} objects, validated by the appearance API: known widget ids
-- only, unique, at most four, and x/y are fractions (0..1) of the free desktop
-- area so a widget always stays inside the desktop at any browser size.

ALTER TABLE user_appearance ADD COLUMN widgets TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(widgets) AND json_type(widgets) = 'array');
