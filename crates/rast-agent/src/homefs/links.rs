//! The `~/Shared/<name>` links of one HOME (ADR 0005, section 1): one symlink
//! per shared folder that the user may open, pointing at its container in
//! the shared root. They are for apps (file dialogs, terminals); File Station
//! never follows them.
//!
//! The agent keeps them, as the user: the server sends the user's current
//! shared folders (the same strictly checked header as every request), and
//! the agent adds the missing links and removes the links that point into
//! the shared root but are no longer wanted. It only ever creates or removes
//! symlinks whose target is a direct child of the shared root, and never
//! touches anything else in `~/Shared`: a file or a foreign link with a
//! share's name is reported as a conflict and left alone.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rustix::fs as rfs;
use serde::Serialize;

use crate::error::{AppError, AppResult};

use super::fsat::{self, fs_error, Kind};
use super::{ShareRoots, SHARED_DIR};

/// What one pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedLinks {
    /// Links created (or pointed at a new container).
    pub linked: Vec<String>,
    /// Links removed: their shared folder is gone or no longer open to the
    /// user.
    pub removed: Vec<String>,
    /// Share names taken by something else in `~/Shared` (left unchanged).
    pub conflicts: Vec<String>,
}

/// Whether `target` names a direct child of `shared_root`.
fn share_target<'a>(target: &'a Path, shared_root: &Path) -> Option<&'a std::ffi::OsStr> {
    (target.parent() == Some(shared_root))
        .then(|| target.file_name())
        .flatten()
}

/// Bring `<home>/Shared` in line with `shares` (keyed by link name).
/// `shared_root` is the canonical shared root, as the links name it.
pub fn sync(
    home: BorrowedFd<'_>,
    shared_root: &Path,
    shares: &ShareRoots,
) -> AppResult<SharedLinks> {
    let wanted: BTreeMap<&str, PathBuf> = shares
        .iter()
        .map(|(name, root)| (name.as_str(), shared_root.join(&root.container)))
        .collect();
    let mut report = SharedLinks::default();
    let dir = match fsat::stat_at(home, SHARED_DIR).map_err(fs_error)? {
        None if wanted.is_empty() => return Ok(report),
        None => {
            fsat::mkdir_at(home, SHARED_DIR).map_err(fs_error)?;
            fsat::open_created_dir(home, SHARED_DIR)?
        }
        Some(info) if info.is_dir() => fsat::open_listing(home, Some(SHARED_DIR), &info)?,
        Some(_) => {
            return Err(AppError::conflict(
                "~/Shared is not a folder; the shared folder links cannot be kept",
            ))
        }
    };
    let mut done = std::collections::BTreeSet::new();
    for (name, info) in fsat::entries(dir.as_fd()).map_err(fs_error)? {
        let wanted_target = wanted.get(name.as_str());
        if info.kind != Kind::Symlink {
            if wanted_target.is_some() {
                report.conflicts.push(name.clone());
                done.insert(name);
            }
            continue;
        }
        let target = match rfs::readlinkat(dir.as_fd(), name.as_str(), Vec::new()) {
            Ok(target) => PathBuf::from(std::ffi::OsString::from_vec(target.into_bytes())),
            Err(_) => continue,
        };
        let ours = share_target(&target, shared_root).is_some();
        match wanted_target {
            Some(wanted) if *wanted == target => {
                done.insert(name);
            }
            Some(_) if !ours => {
                report.conflicts.push(name.clone());
                done.insert(name);
            }
            // A link of ours that points at another container (the share
            // was recreated), or one that is no longer wanted.
            _ if ours => {
                fsat::unlink_at(dir.as_fd(), &name).map_err(fs_error)?;
                if wanted_target.is_none() {
                    report.removed.push(name);
                }
            }
            _ => {}
        }
    }
    for (name, target) in &wanted {
        if done.contains(*name) {
            continue;
        }
        match rfs::symlinkat(target.as_path(), dir.as_fd(), *name) {
            Ok(()) => report.linked.push((*name).to_string()),
            Err(rustix::io::Errno::EXIST) => report.conflicts.push((*name).to_string()),
            Err(error) => return Err(fs_error(error.into())),
        }
    }
    // The folder is read in directory order, which differs between file
    // systems: report names sorted, so the result does not depend on it.
    report.linked.sort();
    report.removed.sort();
    report.conflicts.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::homefs::ShareRoot;
    use std::os::unix::fs::symlink;

    fn shares(list: &[(&str, &str)]) -> ShareRoots {
        let mut out = ShareRoots::default();
        for (index, (name, container)) in list.iter().enumerate() {
            out.insert(
                name,
                ShareRoot {
                    id: format!("00000000-0000-4000-8000-00000000000{index}"),
                    name: name.to_string(),
                    container: container.to_string(),
                },
            )
            .unwrap();
        }
        out
    }

    fn link(path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }

    #[test]
    fn links_follow_the_shared_folders_of_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(shared.join("team")).unwrap();
        let fd = std::fs::File::open(&home).unwrap();

        // No shares and no folder: nothing is created.
        let report = sync(fd.as_fd(), &shared, &ShareRoots::default()).unwrap();
        assert_eq!(report, SharedLinks::default());
        assert!(!home.join("Shared").exists());

        let report = sync(
            fd.as_fd(),
            &shared,
            &shares(&[("Team", "team"), ("Sales", "sales")]),
        )
        .unwrap();
        assert_eq!(report.linked, ["Sales", "Team"]);
        assert_eq!(link(&home.join("Shared/Team")), Some(shared.join("team")));

        // Things of the user's own are never touched.
        std::fs::write(home.join("Shared/notes.txt"), b"mine").unwrap();
        symlink("/etc", home.join("Shared/etc")).unwrap();
        symlink("/elsewhere", home.join("Shared/Docs")).unwrap();
        std::fs::create_dir(home.join("Shared/Ops")).unwrap();
        let report = sync(
            fd.as_fd(),
            &shared,
            &shares(&[("Team", "team-2"), ("Docs", "docs"), ("Ops", "ops")]),
        )
        .unwrap();
        assert_eq!(report.removed, ["Sales"]);
        assert_eq!(report.linked, ["Team"]);
        assert_eq!(report.conflicts, ["Docs", "Ops"]);
        assert_eq!(link(&home.join("Shared/Team")), Some(shared.join("team-2")));
        assert_eq!(
            link(&home.join("Shared/Docs")),
            Some(PathBuf::from("/elsewhere"))
        );
        assert_eq!(link(&home.join("Shared/etc")), Some(PathBuf::from("/etc")));
        assert_eq!(
            std::fs::read(home.join("Shared/notes.txt")).unwrap(),
            b"mine"
        );

        // Access revoked everywhere: only the links of shares go.
        let report = sync(fd.as_fd(), &shared, &ShareRoots::default()).unwrap();
        assert_eq!(report.removed, ["Team"]);
        assert!(home.join("Shared/notes.txt").exists());
        assert!(link(&home.join("Shared/Docs")).is_some());
        assert!(link(&home.join("Shared/etc")).is_some());

        // A link of a share deeper in the shared root is not one of ours.
        symlink(shared.join("team/sub"), home.join("Shared/Deep")).unwrap();
        sync(fd.as_fd(), &shared, &ShareRoots::default()).unwrap();
        assert!(link(&home.join("Shared/Deep")).is_some());
    }

    #[test]
    fn a_shared_entry_that_is_not_a_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, home.join("Shared")).unwrap();
        let fd = std::fs::File::open(&home).unwrap();
        let error = sync(fd.as_fd(), &shared, &shares(&[("Team", "team")])).unwrap_err();
        assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}
