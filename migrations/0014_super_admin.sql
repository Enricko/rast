-- Super administrators (schema v14).
--
-- Three roles: super administrator, administrator and user. A super
-- administrator is an administrator (`role = 'admin'`) with this flag: every
-- existing administrator check keeps working, and only the server-level
-- surfaces (root file system, host terminal, managing administrators) ask
-- for the flag.
--
-- Every administrator that exists when this migration runs becomes a super
-- administrator, so nobody loses access they had.
ALTER TABLE users ADD COLUMN super_admin INTEGER NOT NULL DEFAULT 0
    CHECK (super_admin IN (0, 1));

UPDATE users SET super_admin = 1 WHERE role = 'admin';
