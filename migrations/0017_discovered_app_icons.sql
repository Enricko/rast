-- Discovered applications: icons and optional working directory (schema v17).
--
-- Icons of applications installed outside the App Center cannot always be read
-- by the unprivileged web server: an app installed inside a managed application
-- profile by an external installer (the WordCraft case) keeps its `.desktop`
-- file and icon in a 0700 identity-owned profile tree. The identity-scoped
-- desktop scan already reads those files *as the identity*, so it now carries
-- the icon bytes back (PNG or SVG only, size-capped) and the inventory stores
-- them, exactly like a managed native app's icon. A rescan refreshes them; an
-- entry whose icon cannot be resolved keeps its last known icon and falls back
-- to the generic App Center icon in the UI.
--
-- `working_dir` is the optional `Path=` of a `.desktop` entry (or the working
-- directory given when an existing application is imported by hand). The shared
-- session applies it when it is inside the managed application trees; anything
-- else keeps using the profile HOME (see native_session::managed_cwd).
ALTER TABLE discovered_apps ADD COLUMN working_dir TEXT;
ALTER TABLE discovered_apps ADD COLUMN icon_type TEXT
    CHECK (icon_type IS NULL OR icon_type IN ('png', 'svg'));
ALTER TABLE discovered_apps ADD COLUMN icon_data BLOB;
