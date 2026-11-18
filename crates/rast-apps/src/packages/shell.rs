//! App Center on the shell PATH: `/etc/profile.d/rast-packages.sh` puts
//! `<data_dir>/packages/bin` (the shims of each package's default version)
//! first on every login shell's PATH, so `go`, `node`, `php` or `python3` in
//! a terminal run the version chosen with *Make default*. A package without a
//! App Center default has no shim, and the system's command is used.
//!
//! The profile script is root's to write (`rast packages path sync`, run by
//! the installer and `rast update`). The server only keeps the runtime tree
//! readable by other users while the option is on: the data directory and the
//! package root get the execute (traverse) bit for others, nothing more, and
//! every other file there is created without any access for others
//! (`UMask=0027`), so the database, keys and backups stay private.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::install::Layout;

pub const PROFILE_SCRIPT: &str = "/etc/profile.d/rast-packages.sh";
pub const SYNC_COMMAND: &str = "sudo rast packages path sync";
const MARKER: &str = "# Managed by RAST";

/// The profile script for `bin_dir` (POSIX sh: dash reads profile.d too).
pub fn script(bin_dir: &Path) -> String {
    let bin = bin_dir.display().to_string().replace('\'', "");
    format!(
        "{MARKER} (rast packages path sync); it is rewritten, do not edit.\n\
         # App Center: the version chosen with \"Make default\" comes first.\n\
         # Turn off with packages_shell_path = false in /etc/rast/rast.toml,\n\
         # then run: {SYNC_COMMAND}\n\
         case \":${{PATH}}:\" in\n\
         \x20 *':{bin}:'*) ;;\n\
         \x20 *) PATH='{bin}'\":${{PATH}}\"; export PATH ;;\n\
         esac\n"
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShellPath {
    /// `packages_shell_path` in the configuration.
    pub enabled: bool,
    /// The profile script exists and is RAST's.
    pub installed: bool,
    /// It names this server's `bin` directory (enabled and up to date).
    pub active: bool,
    pub profile: String,
    pub bin_dir: String,
    /// What to run when the script is missing or outdated.
    pub sync_command: String,
    /// The defaults linked into /usr/local/bin (`packages_system_wide`);
    /// filled in by the caller.
    pub system: super::links::SystemLinks,
}

pub fn status(enabled: bool, bin_dir: &Path, profile: &Path) -> ShellPath {
    let content = fs::read_to_string(profile).ok();
    let installed = content.as_deref().is_some_and(|c| c.starts_with(MARKER));
    let active = enabled && content.as_deref() == Some(script(bin_dir).as_str());
    ShellPath {
        enabled,
        installed,
        active,
        profile: profile.display().to_string(),
        bin_dir: bin_dir.display().to_string(),
        sync_command: SYNC_COMMAND.into(),
        system: Default::default(),
    }
}

/// Let other users run the installed runtimes (`shared`), or close the
/// package tree again. Only permission bits for "others" change: the data
/// directory gains the traverse bit (and never keeps write access for
/// others); RAST's own package directories get exactly what they need.
pub fn share(data_dir: &Path, layout: &Layout, shared: bool) -> io::Result<()> {
    if !shared {
        // Closing the package root is enough to cut access to the runtimes.
        return set_other(&layout.root, 0);
    }
    change_other(data_dir, |other| (other & 0o004) | 0o001)?;
    set_other(&layout.root, 0o001)?;
    for dir in [layout.runtimes(), layout.bin_dir()] {
        if dir.is_dir() {
            set_other(&dir, 0o005)?;
        }
    }
    for package in read_dirs(&layout.runtimes()) {
        set_other(&package, 0o005)?;
        for version in read_dirs(&package) {
            set_other(&version, 0o005)?;
        }
    }
    Ok(())
}

/// Open one freshly installed version (its package directory included).
pub fn share_version(layout: &Layout, id: &str, version: &str) -> io::Result<()> {
    set_other(&layout.package_dir(id), 0o005)?;
    set_other(&layout.version_dir(id, version), 0o005)
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

fn set_other(path: &Path, bits: u32) -> io::Result<()> {
    change_other(path, |_| bits)
}

/// Rewrite the "others" bits of a directory (missing paths are skipped).
fn change_other(path: &Path, f: impl Fn(u32) -> u32) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o7777;
    let wanted = (mode & !0o007) | (f(mode & 0o007) & 0o007);
    if wanted != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(wanted))?;
    }
    Ok(())
}

/// Bring the profile script and the permissions in line with the
/// configuration. Needs root for the script. Returns what was done.
pub fn sync(
    enabled: bool,
    data_dir: &Path,
    layout: &Layout,
    profile: &Path,
) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let bin_dir = layout.bin_dir();
    if enabled {
        // Directories are never created here: run as root they would belong
        // to root. The server creates and opens them when it starts.
        let wanted = script(&bin_dir);
        if fs::read_to_string(profile).ok().as_deref() != Some(wanted.as_str()) {
            if let Ok(existing) = fs::read_to_string(profile) {
                if !existing.starts_with(MARKER) {
                    return Err(format!(
                        "{} exists and was not written by RAST; move it away first",
                        profile.display()
                    ));
                }
            }
            let tmp = profile.with_file_name(".rast-packages.sh.tmp");
            fs::write(&tmp, &wanted)
                .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)))
                .and_then(|_| fs::rename(&tmp, profile))
                .map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    format!("cannot write {}: {e}", profile.display())
                })?;
            done.push(format!(
                "wrote {}: login shells put {} first on PATH",
                profile.display(),
                bin_dir.display()
            ));
        }
        share(data_dir, layout, true)
            .map_err(|e| format!("cannot open {} to other users: {e}", layout.root.display()))?;
        done.push(format!(
            "runtimes under {} are readable by all users ({} and {} only allow traversal)",
            layout.runtimes().display(),
            data_dir.display(),
            layout.root.display()
        ));
    } else {
        match fs::read_to_string(profile) {
            Ok(existing) if existing.starts_with(MARKER) => {
                fs::remove_file(profile)
                    .map_err(|e| format!("cannot remove {}: {e}", profile.display()))?;
                done.push(format!("removed {}", profile.display()));
            }
            Ok(_) => done.push(format!(
                "left {} alone: it was not written by RAST",
                profile.display()
            )),
            Err(_) => {}
        }
        if layout.root.is_dir() {
            share(data_dir, layout, false)
                .map_err(|e| format!("cannot make {} private: {e}", layout.root.display()))?;
            done.push(format!(
                "{} is private again (other users cannot run the runtimes)",
                layout.root.display()
            ));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_profile_script_prepends_the_bin_directory_once() {
        let bin = Path::new("/var/lib/rast/packages/bin");
        let text = script(bin);
        assert!(text.starts_with(MARKER));
        for (start, want) in [
            (
                "/usr/local/bin:/usr/bin",
                "/var/lib/rast/packages/bin:/usr/local/bin:/usr/bin",
            ),
            // Sourced twice (login shell inside a login shell): no duplicate.
            (
                "/var/lib/rast/packages/bin:/usr/bin",
                "/var/lib/rast/packages/bin:/usr/bin",
            ),
        ] {
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{text}\nprintf %s \"$PATH\""))
                .env_clear()
                .env("PATH", start)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&out.stdout), want);
        }
    }

    #[test]
    fn sharing_opens_only_the_runtime_tree() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let layout = Layout::new(data.join("packages"));
        layout.ensure().unwrap();
        let version = layout.version_dir("go", "1.27.4");
        fs::create_dir_all(version.join("bin")).unwrap();
        fs::write(data.join("rast.db"), "").unwrap();
        for p in [
            &data,
            &layout.root,
            &layout.runtimes(),
            &layout.bin_dir(),
            &layout.catalog_dir(),
            &layout.package_dir("go"),
            &version,
        ] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o750)).unwrap();
        }
        fs::set_permissions(data.join("rast.db"), fs::Permissions::from_mode(0o640)).unwrap();

        share(&data, &layout, true).unwrap();
        assert_eq!(mode(&data), 0o751);
        assert_eq!(mode(&layout.root), 0o751);
        assert_eq!(mode(&layout.runtimes()), 0o755);
        assert_eq!(mode(&layout.bin_dir()), 0o755);
        assert_eq!(mode(&layout.package_dir("go")), 0o755);
        assert_eq!(mode(&version), 0o755);
        assert_eq!(mode(&layout.catalog_dir()), 0o750, "catalog stays private");
        assert_eq!(mode(&data.join("rast.db")), 0o640, "files are untouched");

        share(&data, &layout, false).unwrap();
        assert_eq!(
            mode(&layout.root),
            0o750,
            "closing the package root cuts access"
        );

        // A data directory the admin made readable keeps that, but never
        // keeps write access for others.
        fs::set_permissions(&data, fs::Permissions::from_mode(0o757)).unwrap();
        share(&data, &layout, true).unwrap();
        assert_eq!(mode(&data), 0o755);
    }

    #[test]
    fn sync_writes_and_removes_only_its_own_script() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let layout = Layout::new(data.join("packages"));
        let profile = dir.path().join("rast-packages.sh");

        let done = sync(true, &data, &layout, &profile).unwrap();
        assert!(
            !layout.root.exists(),
            "sync never creates the package tree (root would own it)"
        );
        assert!(done[0].starts_with("wrote"), "{done:?}");
        assert_eq!(mode(&profile), 0o644);
        let st = status(true, &layout.bin_dir(), &profile);
        assert!(st.installed && st.active, "{st:?}");
        // Idempotent.
        let again = sync(true, &data, &layout, &profile).unwrap();
        assert!(!again.iter().any(|d| d.starts_with("wrote")), "{again:?}");
        // Another bin directory makes it outdated.
        assert!(!status(true, &dir.path().join("other"), &profile).active);

        sync(false, &data, &layout, &profile).unwrap();
        assert!(!profile.exists());
        assert!(!status(false, &layout.bin_dir(), &profile).installed);

        // A file RAST did not write is never replaced or removed.
        fs::write(&profile, "export PATH=/opt/custom:$PATH\n").unwrap();
        assert!(sync(true, &data, &layout, &profile).is_err());
        let kept = sync(false, &data, &layout, &profile).unwrap();
        assert!(kept[0].starts_with("left"), "{kept:?}");
        assert!(profile.exists());
    }
}
