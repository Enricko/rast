-- Per-app visibility (schema v11).
--
-- Each installed app is visible to every signed-in user by default; an
-- administrator can restrict an app to administrators. Anyone who can see an
-- app opens their own copy (own container, own data volume with its settings
-- and sign-ins); installing, updating, removing, limiting and changing the
-- visibility stay administrator-only.

ALTER TABLE linux_apps ADD COLUMN visibility TEXT NOT NULL DEFAULT 'users'
    CHECK (visibility IN ('users', 'admins'));
