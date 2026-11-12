-- Desktop icon layout per account (schema v9).
--
-- desktop_icons: NULL until the account arranges its desktop; the desktop
-- then shows every app it may open in the default order. Otherwise a JSON
-- array of {"id", "col", "row"}: the apps placed on the desktop and their grid
-- cells (validated by the appearance API). Apps not listed stay reachable in
-- Applications and the start menu.

ALTER TABLE user_appearance ADD COLUMN desktop_icons TEXT
    CHECK (desktop_icons IS NULL OR (json_valid(desktop_icons) AND json_type(desktop_icons) = 'array'));
