//! Package Center system-wide (`packages_system_wide`): the commands of each
//! default version are linked into `/usr/local/bin`, so `sudo`, systemd
//! services and scripts run them too, not only login shells (see `shell`).
//!
//! `/usr/local/bin` comes before `/usr/bin` in sudo's `secure_path` and in the
//! PATH systemd gives services, so files installed by apt are never changed:
//! they are only shadowed. A command someone installed there by hand (for
//! example a manual Node.js) is renamed in place to `.rast-displaced.<name>`
//! (relative links keep working) and put back when Package Center no longer
//! provides that command.
//!
//! Only root can write `/usr/local/bin`. `rast packages path sync` installs a
//! systemd path unit that watches `<data_dir>/packages/bin` and runs
//! `rast packages path links` as root after every change, so the server itself
//! never needs root. The root side trusts nothing it reads there beyond this:
//! a command is linked only when its name is on a fixed list, and its shim
//! points into the `current` tree of the official package that provides that
//! command. A third-party package cannot put a command on root's PATH.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub const SYSTEM_BIN: &str = "/usr/local/bin";
pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const PATH_UNIT: &str = "rast-packages-links.path";
pub const SERVICE_UNIT: &str = "rast-packages-links.service";
/// Prefix of a command moved aside in `/usr/local/bin`.
pub const DISPLACED_PREFIX: &str = ".rast-displaced.";
const MARKER: &str = "# Managed by RAST";

/// The official package that must provide `name` for it to be linked.
pub fn owner_of(name: &str) -> Option<&'static str> {
    match name {
        "php" => Some("php"),
        "node" | "npm" | "npx" | "corepack" => Some("nodejs"),
        "go" | "gofmt" => Some("go"),
        "python3" | "pip3" => Some("python"),
        n if versioned(n, "python3.") || versioned(n, "pip3.") => Some("python"),
        _ => None,
    }
}

/// `python3.14`: the prefix and then one or two digits.
fn versioned(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|rest| {
        (1..=2).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit())
    })
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Commands in `bin_dir` that may be linked: listed names whose shim points
/// into the owning package's `current` tree (no `..` after that).
pub fn eligible(bin_dir: &Path) -> Vec<String> {
    names(bin_dir)
        .into_iter()
        .filter(|name| {
            let Some(id) = owner_of(name) else {
                return false;
            };
            let Ok(target) = fs::read_link(bin_dir.join(name)) else {
                return false;
            };
            let target = target.to_string_lossy().into_owned();
            let prefix = format!("../runtimes/{id}/current/");
            target.strip_prefix(&prefix).is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .split('/')
                        .all(|c| !c.is_empty() && c != "." && c != "..")
            })
        })
        .collect()
}

/// Whether `system_bin/name` is RAST's link to the shim.
fn ours(system_bin: &Path, bin_dir: &Path, name: &str) -> bool {
    fs::read_link(system_bin.join(name)).is_ok_and(|t| t == bin_dir.join(name))
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemLinks {
    /// `packages_system_wide` (with `packages_shell_path`).
    pub enabled: bool,
    /// Commands linked into the system directory now.
    pub linked: Vec<String>,
    /// Commands that should be linked but are not (yet).
    pub pending: Vec<String>,
    /// Commands installed by hand that were moved aside.
    pub displaced: Vec<String>,
    pub directory: String,
    /// The watcher (systemd path unit) is installed.
    pub watcher: bool,
}

pub fn status(enabled: bool, bin_dir: &Path, system_bin: &Path, unit_dir: &Path) -> SystemLinks {
    let mut linked = Vec::new();
    let mut displaced = Vec::new();
    for name in names(system_bin) {
        if let Some(original) = name.strip_prefix(DISPLACED_PREFIX) {
            if owner_of(original).is_some() {
                displaced.push(original.to_string());
            }
        } else if owner_of(&name).is_some() && ours(system_bin, bin_dir, &name) {
            linked.push(name);
        }
    }
    let pending = if enabled {
        eligible(bin_dir)
            .into_iter()
            .filter(|n| !linked.contains(n))
            .collect()
    } else {
        Vec::new()
    };
    SystemLinks {
        enabled,
        linked,
        pending,
        displaced,
        directory: system_bin.display().to_string(),
        watcher: unit_dir.join(PATH_UNIT).is_file(),
    }
}

/// Link the eligible commands into `system_bin`; remove RAST's links that are
/// no longer wanted (all of them when `enabled` is false) and put back what
/// was moved aside. Never replaces or removes anything else. Returns what was
/// done.
pub fn sync(enabled: bool, bin_dir: &Path, system_bin: &Path) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let wanted: BTreeSet<String> = if enabled {
        eligible(bin_dir).into_iter().collect()
    } else {
        BTreeSet::new()
    };
    for name in &wanted {
        if ours(system_bin, bin_dir, name) {
            continue;
        }
        let link = system_bin.join(name);
        if fs::symlink_metadata(&link).is_ok() {
            let aside = system_bin.join(format!("{DISPLACED_PREFIX}{name}"));
            if fs::symlink_metadata(&aside).is_ok() {
                done.push(format!(
                    "left {} alone: {} already holds a copy moved aside earlier",
                    link.display(),
                    aside.display()
                ));
                continue;
            }
            fs::rename(&link, &aside)
                .map_err(|e| format!("cannot move {} aside: {e}", link.display()))?;
            done.push(format!(
                "moved {} aside to {}",
                link.display(),
                aside.display()
            ));
        }
        let target = bin_dir.join(name);
        let tmp = system_bin.join(format!(".rast-link-{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)
            .and_then(|_| fs::rename(&tmp, &link))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                format!("cannot link {}: {e}", link.display())
            })?;
        done.push(format!("linked {} -> {}", link.display(), target.display()));
    }
    for name in names(system_bin) {
        if owner_of(&name).is_some() && !wanted.contains(&name) && ours(system_bin, bin_dir, &name)
        {
            let link = system_bin.join(&name);
            fs::remove_file(&link).map_err(|e| format!("cannot remove {}: {e}", link.display()))?;
            done.push(format!("removed {}", link.display()));
        }
    }
    // Put back what was moved aside once RAST no longer links that name.
    for name in names(system_bin) {
        let Some(original) = name.strip_prefix(DISPLACED_PREFIX) else {
            continue;
        };
        if owner_of(original).is_none() || wanted.contains(original) {
            continue;
        }
        let aside = system_bin.join(&name);
        let link = system_bin.join(original);
        if fs::symlink_metadata(&link).is_ok() {
            done.push(format!(
                "kept {}: {} exists again",
                aside.display(),
                link.display()
            ));
            continue;
        }
        fs::rename(&aside, &link)
            .map_err(|e| format!("cannot put {} back: {e}", link.display()))?;
        done.push(format!("put {} back", link.display()));
    }
    Ok(done)
}

/// The watcher units for `bin_dir`. `config` is passed on when the server
/// was started with a non-default configuration file.
pub fn units(
    rast_bin: &str,
    bin_dir: &Path,
    data_dir: &Path,
    config: Option<&Path>,
) -> [(String, String); 2] {
    let config = config
        .map(|c| format!(" --config '{}'", c.display().to_string().replace('\'', "")))
        .unwrap_or_default();
    let header = format!("{MARKER} (rast packages path sync); it is rewritten, do not edit.\n");
    let path = format!(
        "{header}[Unit]\n\
         Description=RAST Package Center: watch the default commands\n\
         \n\
         [Path]\n\
         PathChanged={bin}\n\
         Unit={SERVICE_UNIT}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = bin_dir.display()
    );
    let service = format!(
        "{header}[Unit]\n\
         Description=RAST Package Center: link the default commands into {SYSTEM_BIN}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         EnvironmentFile=-/etc/rast/rast.env\n\
         ExecStart={rast_bin}{config} packages path links\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         PrivateNetwork=yes\n\
         ProtectHome=read-only\n\
         ProtectSystem=strict\n\
         ReadWritePaths={SYSTEM_BIN}\n\
         ReadOnlyPaths={data}\n",
        data = data_dir.display()
    );
    [
        (PATH_UNIT.to_string(), path),
        (SERVICE_UNIT.to_string(), service),
    ]
}

/// Install (or remove) the watcher units in `unit_dir`. `systemctl` runs
/// `systemctl <args>`; a unit file RAST did not write is never replaced.
pub fn sync_units(
    enabled: bool,
    unit_dir: &Path,
    units: &[(String, String); 2],
    systemctl: &dyn Fn(&[&str]) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    if enabled {
        for (name, _) in units {
            let path = unit_dir.join(name);
            if fs::read_to_string(&path).is_ok_and(|t| !t.starts_with(MARKER)) {
                return Err(format!(
                    "{} exists and was not written by RAST; move it away first",
                    path.display()
                ));
            }
        }
        let mut changed = false;
        for (name, text) in units {
            let path = unit_dir.join(name);
            if fs::read_to_string(&path).is_ok_and(|t| t == *text) {
                continue;
            }
            write_file(&path, text)?;
            changed = true;
            done.push(format!("wrote {}", path.display()));
        }
        if changed {
            systemctl(&["daemon-reload"])?;
        }
        systemctl(&["enable", "--now", PATH_UNIT])?;
    } else {
        let present: Vec<PathBuf> = units
            .iter()
            .map(|(name, _)| unit_dir.join(name))
            .filter(|p| fs::read_to_string(p).is_ok_and(|t| t.starts_with(MARKER)))
            .collect();
        if !present.is_empty() {
            let _ = systemctl(&["disable", "--now", PATH_UNIT]);
            for path in &present {
                fs::remove_file(path)
                    .map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
                done.push(format!("removed {}", path.display()));
            }
            systemctl(&["daemon-reload"])?;
        }
    }
    Ok(done)
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    fs::write(&tmp, text)
        .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)))
        .and_then(|_| fs::rename(&tmp, path))
        .map_err(|e: io::Error| {
            let _ = fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("packages/bin");
        let system = dir.path().join("usr/local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&system).unwrap();
        (dir, bin, system)
    }

    #[test]
    fn only_listed_commands_of_the_owning_package_are_eligible() {
        let (_d, bin, _s) = tree();
        symlink("../runtimes/php/current/bin/php", bin.join("php")).unwrap();
        symlink("../runtimes/go/current/bin/gofmt", bin.join("gofmt")).unwrap();
        symlink(
            "../runtimes/python/current/bin/python3.14",
            bin.join("python3.14"),
        )
        .unwrap();
        // A third-party package providing `node`, a name not on the list, a
        // shim escaping its tree and a plain file are never linked.
        symlink("../runtimes/evil/current/bin/node", bin.join("node")).unwrap();
        symlink("../runtimes/lua/current/bin/lua", bin.join("lua")).unwrap();
        symlink("../runtimes/go/current/../../evil/go", bin.join("go")).unwrap();
        fs::write(bin.join("npm"), "#!/bin/sh\n").unwrap();
        assert_eq!(eligible(&bin), ["gofmt", "php", "python3.14"]);
        assert_eq!(owner_of("python3.123"), None);
        assert_eq!(owner_of("pip3.9"), Some("python"));
        assert_eq!(owner_of("sudo"), None);
    }

    #[test]
    fn links_move_a_manual_command_aside_and_put_it_back() {
        let (dir, bin, system) = tree();
        symlink("../runtimes/php/current/bin/php", bin.join("php")).unwrap();
        symlink("../runtimes/nodejs/current/bin/node", bin.join("node")).unwrap();
        // A manual Node.js with a relative link, and an unrelated command.
        fs::create_dir_all(dir.path().join("usr/local/lib/nodejs/bin")).unwrap();
        fs::write(dir.path().join("usr/local/lib/nodejs/bin/node"), "manual").unwrap();
        symlink("../lib/nodejs/bin/node", system.join("node")).unwrap();
        fs::write(system.join("layar-off"), "keep").unwrap();

        let done = sync(true, &bin, &system).unwrap();
        assert!(done.iter().any(|d| d.starts_with("moved")), "{done:?}");
        assert_eq!(fs::read_link(system.join("php")).unwrap(), bin.join("php"));
        assert_eq!(
            fs::read_link(system.join("node")).unwrap(),
            bin.join("node")
        );
        let aside = system.join(".rast-displaced.node");
        assert_eq!(
            fs::read_to_string(&aside).unwrap(),
            "manual",
            "relative link still resolves"
        );
        let st = status(true, &bin, &system, dir.path());
        assert_eq!(st.linked, ["node", "php"]);
        assert_eq!(st.displaced, ["node"]);
        assert!(st.pending.is_empty() && !st.watcher);
        // Idempotent.
        assert!(sync(true, &bin, &system).unwrap().is_empty());

        // Node goes back to the system version: its link goes, the manual
        // copy comes back; PHP stays.
        fs::remove_file(bin.join("node")).unwrap();
        let done = sync(true, &bin, &system).unwrap();
        assert!(done.iter().any(|d| d.starts_with("put")), "{done:?}");
        assert_eq!(fs::read_to_string(system.join("node")).unwrap(), "manual");
        assert!(!aside.exists());
        assert!(system.join("php").is_symlink());

        // Turned off: every RAST link goes; other files are untouched.
        sync(false, &bin, &system).unwrap();
        assert!(!system.join("php").exists() && !system.join("php").is_symlink());
        assert_eq!(
            fs::read_to_string(system.join("layar-off")).unwrap(),
            "keep"
        );
        assert_eq!(
            status(false, &bin, &system, dir.path()),
            SystemLinks {
                enabled: false,
                directory: system.display().to_string(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_name_taken_again_while_moved_aside_is_never_overwritten() {
        let (_d, bin, system) = tree();
        symlink("../runtimes/go/current/bin/go", bin.join("go")).unwrap();
        fs::write(system.join("go"), "manual go").unwrap();
        sync(true, &bin, &system).unwrap();
        // Someone installs go by hand again, then RAST stops providing it.
        fs::remove_file(system.join("go")).unwrap();
        fs::write(system.join("go"), "newer manual go").unwrap();
        fs::remove_file(bin.join("go")).unwrap();
        let done = sync(true, &bin, &system).unwrap();
        assert!(done.iter().any(|d| d.starts_with("kept")), "{done:?}");
        assert_eq!(
            fs::read_to_string(system.join("go")).unwrap(),
            "newer manual go"
        );
        assert_eq!(
            fs::read_to_string(system.join(".rast-displaced.go")).unwrap(),
            "manual go"
        );
    }

    #[test]
    fn watcher_units_are_installed_and_removed_with_systemctl() {
        let dir = tempfile::tempdir().unwrap();
        let calls = RefCell::new(Vec::new());
        let systemctl = |args: &[&str]| {
            calls.borrow_mut().push(args.join(" "));
            Ok(())
        };
        let units = units(
            "/usr/local/bin/rast",
            Path::new("/var/lib/rast/packages/bin"),
            Path::new("/var/lib/rast"),
            None,
        );
        assert!(units[0]
            .1
            .contains("PathChanged=/var/lib/rast/packages/bin"));
        assert!(units[1]
            .1
            .contains("ExecStart=/usr/local/bin/rast packages path links"));
        assert!(units[1]
            .1
            .contains("ReadWritePaths=/usr/local/bin\nReadOnlyPaths=/var/lib/rast\n"));
        let custom = super::units(
            "/usr/local/bin/rast",
            Path::new("/b"),
            Path::new("/d"),
            Some(Path::new("/etc/rast/x.toml")),
        );
        assert!(custom[1].1.contains(
            "ExecStart=/usr/local/bin/rast --config '/etc/rast/x.toml' packages path links"
        ));

        let done = sync_units(true, dir.path(), &units, &systemctl).unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(
            *calls.borrow(),
            ["daemon-reload", "enable --now rast-packages-links.path"]
        );
        calls.borrow_mut().clear();
        assert!(sync_units(true, dir.path(), &units, &systemctl)
            .unwrap()
            .is_empty());
        assert_eq!(
            *calls.borrow(),
            ["enable --now rast-packages-links.path"],
            "no reload when unchanged"
        );

        calls.borrow_mut().clear();
        sync_units(false, dir.path(), &units, &systemctl).unwrap();
        assert!(!dir.path().join(PATH_UNIT).exists());
        assert_eq!(
            *calls.borrow(),
            ["disable --now rast-packages-links.path", "daemon-reload"]
        );

        // A unit file RAST did not write is never replaced, and nothing is
        // written next to it.
        fs::write(dir.path().join(SERVICE_UNIT), "[Service]\n").unwrap();
        assert!(sync_units(true, dir.path(), &units, &systemctl).is_err());
        assert!(!dir.path().join(PATH_UNIT).exists());
    }
}
