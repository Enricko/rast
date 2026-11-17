//! `rast migrate home` (ADR 0005, section 8): move one legacy account into
//! one-HOME mode, and back.
//!
//! A legacy account keeps its files in a HOME `<homes>/<username>` that the
//! `rast` service owns, and its apps keep theirs in the identity home
//! `<data_dir>/linux-users/<userId>` and in one profile per app
//! (`<data_dir>/linux-apps/profiles/<app>/<userId>/{home,config,data,state,cache,tmp}`).
//! The migration of one account:
//!
//! 1. stops the account's units and refuses to go on while any process of
//!    its identity runs;
//! 2. records every entry of the HOME as it is (owner, group, mode, ACLs)
//!    and the plan of folders to create and entries to move in a journal
//!    below `<data_dir>/migrations/home/<userId>/<stamp>/`, written and
//!    synced before anything changes;
//! 3. gives the identity every entry of the service in the HOME and removes
//!    the ACL entries of RAST identities (the read grant of 0.18.3,
//!    `u:rast-u<id>:rx` and `d:u:rast-u<id>:rx`, and terminal grants);
//! 4. merges the identity home and the app profiles into the HOME, per file
//!    (task 1.5 decision): an entry that the HOME does not have moves in, a
//!    folder that both have is merged, anything else is a conflict that
//!    stays where it is and is reported;
//! 5. sets the passwd home to the HOME and gives the HOME itself to the
//!    identity (mode 0700, no ACLs): the account is now in one-HOME mode;
//! 6. moves, in batches, the account's legacy recycle bin into XDG Trash
//!    (`home_migrate_trash`).
//!
//! The rollback walks the same steps back from the journal. Every tree is
//! walked through descriptors (`openat` with `O_NOFOLLOW`, checked inode,
//! device and mount): no link is ever followed, no entry of another mount
//! is changed, and a hard-linked file is never given away.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, OsString};
use std::io::{self, BufRead};
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native::acl::{self, Acl, Entry, Tag};
use crate::native::daemon::{running_user_units, stop_unit};
use crate::native::homes::{
    c_name, fchmod, fchown, fstat, is_dir, is_symlink, open_dir, stop_files_agent, DIR_FLAGS,
};
use crate::native::identity::{check_existing_identity, getent_passwd, Identity, PasswdEntry};
use crate::native::protocol::{
    validate_app_id, validate_home_username, validate_share_container, HomeMigrateOp, HomeTrashOp,
    LegacyTrashEntry, NativeError, MAX_TRASH_BATCH, MAX_USER_ID,
};
use crate::native::shares::{
    blocking, dev_of, dir_names, fd_mount_id, identity_uid_range, ino_of, is_regular,
    journal_owner, lstat_at, mount_id, newest_journal, open_at, open_journal_dir,
    open_journal_file, parse_proc_status, proc_dir, read_proc_file, record_of, root_uid,
    same_inode, service_uid, show, valid_relative, write_synced, MigrationLock, Record, Sample,
    FILE_FLAGS, JOURNAL_FILE, MAX_DEPTH, MAX_TREE_ENTRIES, ROLLED_BACK_SUFFIX,
};
use crate::native::{run_command, NativeOptions};

/// Owners given in tests, by device and inode.
pub type OwnerMap = Arc<Mutex<HashMap<(u64, u64), (u32, u32)>>>;

/// The plan of folders to create and entries to move, next to the journal.
pub const PLAN_FILE: &str = "plan.jsonl";
/// Written once the HOME is in one-HOME mode.
pub const APPLIED_FILE: &str = "applied";
/// Prefix of the journals of recycle bin batches (`trash-0001.jsonl`).
const TRASH_PREFIX: &str = "trash-";
/// Batches of one migration at most.
const MAX_TRASH_BATCHES: usize = 9_999;
/// Processes read from `/proc` at most.
const MAX_PROCESSES: u64 = 1 << 20;
/// Apps whose profiles are merged at most.
const MAX_APPS: usize = 4_096;
/// Names tried for a recycle bin entry (`name`, `name.2`, …).
const MAX_TRASH_NAMES: usize = 1_000;
/// Timeout of `usermod`.
const USERMOD_TIMEOUT: Duration = Duration::from_secs(45);
/// Names of the HOME's `Shared` folder listed by the scan at most.
const SHARED_LISTED: usize = 200;

/// The parts of an app profile and where they go in the HOME. `tmp` is not
/// merged: it only holds what a running app needed.
pub const PROFILE_PARTS: [(&str, &str); 5] = [
    ("home", ""),
    ("config", ".config"),
    ("data", ".local/share"),
    ("state", ".local/state"),
    ("cache", ".cache"),
];

// ---------------------------------------------------------------------------
// Owners
// ---------------------------------------------------------------------------

/// Who is who during a migration.
#[derive(Clone)]
struct Owners {
    root: u32,
    /// The `rast` service, which owns a legacy HOME.
    rast: u32,
    /// The account's identity.
    uid: u32,
    gid: u32,
    /// Every RAST identity uid.
    identities: RangeInclusive<u32>,
    test_mode: bool,
    #[cfg(test)]
    overlay: Option<OwnerMap>,
}

impl Owners {
    fn of(options: &NativeOptions, identity: &Identity) -> Result<Self, NativeError> {
        Ok(Self {
            root: root_uid(options),
            rast: service_uid(options)?,
            uid: identity.uid,
            gid: identity.gid,
            identities: identity_uid_range(options.uid_base),
            test_mode: options.is_test_mode(),
            #[cfg(test)]
            overlay: options.test_owners.clone(),
        })
    }

    /// The owner and group of an entry.
    fn owner(&self, stat: &libc::stat) -> (u32, u32) {
        #[cfg(test)]
        if let Some(map) = &self.overlay {
            if let Some(owner) = map
                .lock()
                .unwrap()
                .get(&(dev_of(stat), ino_of(stat)))
                .copied()
            {
                return owner;
            }
        }
        (stat.st_uid, stat.st_gid)
    }

    /// Change the owner of the entry open as `fd`.
    fn chown(&self, fd: RawFd, stat: &libc::stat, uid: u32, gid: u32) -> io::Result<()> {
        #[cfg(test)]
        if let Some(map) = &self.overlay {
            map.lock()
                .unwrap()
                .insert((dev_of(stat), ino_of(stat)), (uid, gid));
            return Ok(());
        }
        let _ = stat;
        if self.test_mode {
            return Ok(());
        }
        fchown(fd, uid, gid)
    }

    fn is_identity(&self, uid: u32) -> bool {
        self.identities.contains(&uid)
    }

    /// An entry the migration may change: the service's or the account's.
    fn managed(&self, uid: u32) -> bool {
        uid == self.rast || uid == self.uid
    }
}

// ---------------------------------------------------------------------------
// Descriptor helpers
// ---------------------------------------------------------------------------

fn cname(name: &str) -> Result<CString, NativeError> {
    c_name(name).map_err(|_| NativeError::invalid("a name contains a NUL byte"))
}

fn io_failed(what: &str, path: &str, error: io::Error) -> NativeError {
    NativeError::command_failed(format!("cannot {what} {}: {error}", show(path)))
}

/// `a/b` from `a` and `b` (either may be empty).
fn join(parent: &str, name: &str) -> String {
    match (parent.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (_, true) => parent.to_string(),
        _ => format!("{parent}/{name}"),
    }
}

/// The parent and the last name of a relative path.
fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// Open the folder `rel` below `root`, one name at a time, never following
/// a link (`""`: `root` itself).
fn open_rel(root: &OwnedFd, rel: &str) -> io::Result<OwnedFd> {
    let mut dir = open_at(root.as_raw_fd(), c".", DIR_FLAGS)?;
    if rel.is_empty() {
        return Ok(dir);
    }
    for part in rel.split('/') {
        let name = c_name(part)?;
        dir = open_at(dir.as_raw_fd(), &name, DIR_FLAGS)?;
    }
    Ok(dir)
}

fn rename_noreplace(from: RawFd, from_name: &CStr, to: RawFd, to_name: &CStr) -> io::Result<()> {
    // SAFETY: valid fds and NUL-terminated names.
    if unsafe {
        libc::renameat2(
            from,
            from_name.as_ptr(),
            to,
            to_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(dir: RawFd, name: &CStr, folder: bool) -> io::Result<()> {
    let flags = if folder { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mkdir_at(dir: RawFd, name: &CStr) -> io::Result<()> {
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn is_enoent(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOENT)
}

/// Open a folder made (or found) by the migration and give it to the
/// identity, mode 0700. In a shared folder (`keep_group`) it keeps the
/// group and set-gid bit it inherited from the container and gets no ACL,
/// as the agent makes its trash folders there.
fn ensure_dir(
    parent: &OwnedFd,
    name: &str,
    owners: &Owners,
    path: &str,
) -> Result<(OwnedFd, bool), NativeError> {
    ensure_dir_in(parent, name, owners, path, false)
}

fn ensure_dir_in(
    parent: &OwnedFd,
    name: &str,
    owners: &Owners,
    path: &str,
    keep_group: bool,
) -> Result<(OwnedFd, bool), NativeError> {
    let c = cname(name)?;
    let created = match mkdir_at(parent.as_raw_fd(), &c) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => false,
        Err(error) => return Err(io_failed("create", path, error)),
    };
    let fd = open_at(parent.as_raw_fd(), &c, DIR_FLAGS)
        .map_err(|error| io_failed("open", path, error))?;
    let stat = fstat(fd.as_raw_fd()).map_err(|error| io_failed("inspect", path, error))?;
    if created {
        let (gid, mode) = if keep_group {
            (owners.owner(&stat).1, 0o700 | (stat.st_mode & 0o2000))
        } else {
            (owners.gid, 0o700)
        };
        owners
            .chown(fd.as_raw_fd(), &stat, owners.uid, gid)
            .map_err(|error| io_failed("chown", path, error))?;
        if keep_group {
            // Default ACLs of the container were inherited.
            acl::remove(fd.as_raw_fd(), acl::ACCESS_XATTR)
                .and_then(|()| acl::remove(fd.as_raw_fd(), acl::DEFAULT_XATTR))
                .map_err(|error| io_failed("change the ACL of", path, error))?;
        }
        fchmod(fd.as_raw_fd(), mode as libc::mode_t)
            .map_err(|error| io_failed("chmod", path, error))?;
    } else if owners.owner(&stat).0 != owners.uid {
        return Err(NativeError::forbidden(format!(
            "{} exists and does not belong to the account",
            show(path)
        )));
    }
    Ok((fd, created))
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// What a walk of the HOME found (and, for apply and rollback, did).
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeTree {
    pub dirs: u64,
    pub files: u64,
    /// Apparent size of the regular files.
    pub bytes: u64,
    /// Space they take on disk.
    pub allocated_bytes: u64,
    /// Entries of the `rast` service that become the identity's.
    pub change_owner: Sample,
    /// Entries that already belong to the identity (made in a terminal).
    pub identity_owned: u64,
    /// Entries with ACL entries of RAST identities (the read grant of
    /// 0.18.3, `u:rast-u<id>:rx` and `d:u:rast-u<id>:rx`, and terminal
    /// grants), which are removed.
    pub identity_acl_entries: Sample,
    /// Entries with other named ACL entries, which stay: `path: entries`.
    pub foreign_acl_entries: Sample,
    pub root_owned: Sample,
    /// Entries of another account (left as they are).
    pub other_owner: Sample,
    /// Regular files with more than one link: never given away.
    pub hard_links: Sample,
    pub symlinks: Sample,
    pub special: Sample,
    /// Entries on another file system or mount (not entered).
    pub other_device: Sample,
    pub non_utf8: u64,
    pub changed_during_walk: Sample,
}

/// What merging one source (the identity home or one app) does.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceReport {
    /// `identity` (the legacy identity home) or the app id.
    pub source: String,
    pub path: String,
    pub dirs: u64,
    pub files: u64,
    pub bytes: u64,
    /// Entries that move into the HOME (a folder moves as a whole).
    pub moves: u64,
    /// Entries that stay because the HOME has something else there.
    pub conflicts: u64,
    /// Entries that stay because they are not the identity's, are mount
    /// points or have names that are not UTF-8.
    pub skipped: u64,
    /// On another file system than the HOME: nothing of it is moved.
    pub other_file_system: bool,
}

/// What merging the identity home and the app profiles does.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeReport {
    pub sources: Vec<SourceReport>,
    pub moves: u64,
    pub folders_created: u64,
    /// `<source>: <path>` of the conflicts.
    pub conflicts: Sample,
    /// `<source>: <path> (<why>)` of the skipped entries.
    pub skipped: Sample,
    /// Entries left because the session makes them again (`.Xauthority`).
    pub regenerated: Vec<String>,
}

// ---------------------------------------------------------------------------
// Walking a tree (the HOME, a recycle bin payload)
// ---------------------------------------------------------------------------

/// One entry the walk hands to its visitor: a folder (opened and checked)
/// or a regular file with a single link, of the service or the identity,
/// on the tree's mount.
struct Visit<'a> {
    parent: RawFd,
    name: &'a CStr,
    path: &'a str,
    stat: &'a libc::stat,
    owner: (u32, u32),
    dir: Option<&'a OwnedFd>,
}

impl Visit<'_> {
    /// The descriptor of the entry: the open folder, or the file opened
    /// and checked to be the very file the walk saw.
    fn open(&self) -> Option<(OwnedFd, libc::stat)> {
        if let Some(dir) = self.dir {
            let dup = open_at(dir.as_raw_fd(), c".", DIR_FLAGS).ok()?;
            return Some((dup, *self.stat));
        }
        let fd = open_at(self.parent, self.name, FILE_FLAGS).ok()?;
        let stat = fstat(fd.as_raw_fd()).ok()?;
        (is_regular(&stat) && stat.st_nlink == 1 && same_inode(&stat, self.stat))
            .then_some((fd, stat))
    }
}

/// Walk the tree below `root` depth first; entries not handed to `visit`
/// are counted in `tree`.
fn walk_tree(
    root: &OwnedFd,
    owners: &Owners,
    tree: &mut HomeTree,
    mut visit: impl FnMut(&Visit<'_>, &mut HomeTree) -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    let walk_error =
        |error: io::Error| NativeError::command_failed(format!("cannot walk a folder: {error}"));
    let device = fstat(root.as_raw_fd()).map_err(walk_error)?.st_dev;
    let mount = fd_mount_id(root.as_raw_fd()).map_err(|error| {
        if error.kind() == io::ErrorKind::Unsupported {
            NativeError::unavailable(error.to_string())
        } else {
            walk_error(error)
        }
    })?;
    struct Frame {
        dir: OwnedFd,
        path: String,
        names: std::vec::IntoIter<CString>,
    }
    let mut read = 0u64;
    let mut names = |fd: &OwnedFd| {
        let names = dir_names(fd.as_raw_fd(), MAX_TREE_ENTRIES.saturating_sub(read))
            .map_err(walk_error)?
            .ok_or_else(too_many)?;
        read += names.len() as u64;
        Ok::<_, NativeError>(names)
    };
    let first = open_at(root.as_raw_fd(), c".", DIR_FLAGS).map_err(walk_error)?;
    let mut stack = vec![Frame {
        names: names(&first)?.into_iter(),
        dir: first,
        path: String::new(),
    }];
    while let Some(frame) = stack.last_mut() {
        let Some(name) = frame.names.next() else {
            stack.pop();
            continue;
        };
        let Ok(text) = name.to_str() else {
            tree.non_utf8 += 1;
            continue;
        };
        let path = join(&frame.path, text);
        let parent = frame.dir.as_raw_fd();
        let Ok(stat) = lstat_at(parent, &name) else {
            tree.changed_during_walk.add(&path);
            continue;
        };
        if is_symlink(&stat) {
            tree.symlinks.add(&path);
            continue;
        }
        if stat.st_dev != device {
            tree.other_device.add(&path);
            continue;
        }
        match mount_id(parent, &name) {
            Ok(id) if id == mount => {}
            Ok(_) => {
                tree.other_device.add(&path);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return Err(NativeError::unavailable(error.to_string()));
            }
            Err(_) => {
                tree.changed_during_walk.add(&path);
                continue;
            }
        }
        let owner = owners.owner(&stat);
        if is_dir(&stat) {
            let Ok(dir) = open_at(parent, &name, DIR_FLAGS) else {
                tree.changed_during_walk.add(&path);
                continue;
            };
            match (fstat(dir.as_raw_fd()), fd_mount_id(dir.as_raw_fd())) {
                (Ok(opened), Ok(id)) if same_inode(&opened, &stat) && id == mount => {}
                _ => {
                    tree.changed_during_walk.add(&path);
                    continue;
                }
            }
            tree.dirs += 1;
            if owner.0 == owners.root {
                tree.root_owned.add(&path);
            } else if !owners.managed(owner.0) {
                tree.other_owner.add(&path);
            } else {
                visit(
                    &Visit {
                        parent,
                        name: &name,
                        path: &path,
                        stat: &stat,
                        owner,
                        dir: Some(&dir),
                    },
                    tree,
                )?;
            }
            if stack.len() >= MAX_DEPTH {
                return Err(NativeError::forbidden(format!(
                    "the folder is nested deeper than {MAX_DEPTH} levels"
                )));
            }
            let entries = names(&dir)?;
            stack.push(Frame {
                dir,
                path,
                names: entries.into_iter(),
            });
            continue;
        }
        if !is_regular(&stat) {
            tree.special.add(&path);
            continue;
        }
        tree.files += 1;
        tree.bytes = tree.bytes.saturating_add(stat.st_size.max(0) as u64);
        tree.allocated_bytes = tree
            .allocated_bytes
            .saturating_add((stat.st_blocks.max(0) as u64).saturating_mul(512));
        if stat.st_nlink > 1 {
            tree.hard_links.add(&path);
        } else if owner.0 == owners.root {
            tree.root_owned.add(&path);
        } else if !owners.managed(owner.0) {
            tree.other_owner.add(&path);
        } else {
            visit(
                &Visit {
                    parent,
                    name: &name,
                    path: &path,
                    stat: &stat,
                    owner,
                    dir: None,
                },
                tree,
            )?;
        }
    }
    Ok(())
}

fn too_many() -> NativeError {
    NativeError::forbidden(format!(
        "the folder holds more than {MAX_TREE_ENTRIES} entries"
    ))
}

// ---------------------------------------------------------------------------
// Giving an entry to the identity
// ---------------------------------------------------------------------------

/// What to do with one ACL xattr.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AclAction {
    Keep,
    /// No named entry is left: remove the ACL; the group bits of the mode
    /// become the owning group's entry.
    Remove {
        group: u16,
    },
    Write(Acl),
}

/// An ACL without the named-user entries of RAST identities.
struct Stripped {
    /// It had such entries.
    identity: bool,
    /// Other named entries, which stay (`getfacl` notation).
    foreign: Option<String>,
    action: AclAction,
}

fn strip(raw: Option<&[u8]>, owners: &Owners) -> Stripped {
    let keep = Stripped {
        identity: false,
        foreign: None,
        action: AclAction::Keep,
    };
    let Some(acl) = raw.and_then(Acl::decode) else {
        return keep;
    };
    let is_identity =
        |entry: &Entry| matches!(entry.tag, Tag::User(uid) if owners.is_identity(uid));
    let kept: Vec<Entry> = acl.0.iter().filter(|e| !is_identity(e)).copied().collect();
    let named: Vec<Entry> = kept
        .iter()
        .filter(|entry| matches!(entry.tag, Tag::User(_) | Tag::Group(_)))
        .copied()
        .collect();
    let foreign = (!named.is_empty()).then(|| Acl(named.clone()).describe());
    if kept.len() == acl.0.len() {
        return Stripped { foreign, ..keep };
    }
    let group = kept
        .iter()
        .find(|entry| entry.tag == Tag::GroupObj)
        .map_or(0, |entry| entry.perm);
    let action = if named.is_empty() {
        AclAction::Remove { group }
    } else {
        // The mask covers what the group class may still do.
        let mask = named.iter().fold(group, |mask, entry| mask | entry.perm);
        let mut entries: Vec<Entry> = kept
            .into_iter()
            .filter(|entry| entry.tag != Tag::Mask)
            .collect();
        entries.push(Entry {
            tag: Tag::Mask,
            perm: mask,
        });
        AclAction::Write(Acl::new(entries))
    };
    Stripped {
        identity: true,
        foreign,
        action,
    }
}

/// Report (and with `apply`, make) the change of one entry of the HOME or
/// of a recycle bin payload: the identity's, without identity ACL entries.
fn convert(
    fd: RawFd,
    stat: &libc::stat,
    owner: (u32, u32),
    path: &str,
    owners: &Owners,
    tree: &mut HomeTree,
    apply: bool,
) -> Result<(), NativeError> {
    convert_to(fd, stat, owner, path, owners, owners.gid, tree, apply)
}

/// [`convert`] with the group `group` (a shared folder's for an entry of
/// its recycle bin).
#[allow(clippy::too_many_arguments)]
fn convert_to(
    fd: RawFd,
    stat: &libc::stat,
    owner: (u32, u32),
    path: &str,
    owners: &Owners,
    group: u32,
    tree: &mut HomeTree,
    apply: bool,
) -> Result<(), NativeError> {
    let dir = is_dir(stat);
    let shown = show(path);
    if owner.0 == owners.rast {
        tree.change_owner.add(shown);
    } else {
        tree.identity_owned += 1;
    }
    let failed = |what: &str, error: io::Error| io_failed(what, path, error);
    let access = acl::read_raw(fd, acl::ACCESS_XATTR).map_err(|e| failed("read the ACL of", e))?;
    let default = if dir {
        acl::read_raw(fd, acl::DEFAULT_XATTR).map_err(|e| failed("read the ACL of", e))?
    } else {
        None
    };
    let access = strip(access.as_deref(), owners);
    let default = strip(default.as_deref(), owners);
    if access.identity || default.identity {
        tree.identity_acl_entries.add(shown);
    }
    let foreign: Vec<String> = [
        access.foreign.clone(),
        default.foreign.map(|text| format!("default {text}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !foreign.is_empty() {
        tree.foreign_acl_entries
            .add(&format!("{shown}: {}", foreign.join(", ")));
    }
    if !apply {
        return Ok(());
    }
    if owner != (owners.uid, group) {
        owners
            .chown(fd, stat, owners.uid, group)
            .map_err(|e| failed("chown", e))?;
    }
    match &access.action {
        AclAction::Keep => {}
        AclAction::Remove { group } => {
            acl::remove(fd, acl::ACCESS_XATTR).map_err(|e| failed("change the ACL of", e))?;
            // Never set-uid, and set-gid only on folders (`chown` cleared
            // both on files already).
            let keep = if dir { 0o3707 } else { 0o1707 };
            let mode = (stat.st_mode & keep) | (u32::from(*group & 0o7) << 3);
            fchmod(fd, mode as libc::mode_t).map_err(|e| failed("chmod", e))?;
        }
        AclAction::Write(acl) => {
            acl::write(fd, acl::ACCESS_XATTR, acl).map_err(|e| failed("change the ACL of", e))?
        }
    }
    match &default.action {
        AclAction::Keep => {}
        AclAction::Remove { .. } => {
            acl::remove(fd, acl::DEFAULT_XATTR).map_err(|e| failed("change the ACL of", e))?
        }
        AclAction::Write(acl) => {
            acl::write(fd, acl::DEFAULT_XATTR, acl).map_err(|e| failed("change the ACL of", e))?
        }
    }
    Ok(())
}

/// Restore one recorded entry through `fd`: owner, group, ACLs, mode.
fn restore(fd: RawFd, stat: &libc::stat, record: &Record, owners: &Owners) -> io::Result<()> {
    owners.chown(fd, stat, record.uid, record.gid)?;
    match &record.acl {
        Some(value) => acl::write_raw(
            fd,
            acl::ACCESS_XATTR,
            &hex::decode(value).unwrap_or_default(),
        )?,
        None => acl::remove(fd, acl::ACCESS_XATTR)?,
    }
    if record.d {
        match &record.dacl {
            Some(value) => acl::write_raw(
                fd,
                acl::DEFAULT_XATTR,
                &hex::decode(value).unwrap_or_default(),
            )?,
            None => acl::remove(fd, acl::DEFAULT_XATTR)?,
        }
    }
    // The mode last: setting an ACL rewrites the group bits.
    fchmod(fd, record.restored_mode() as libc::mode_t)
}

/// The record of an entry, with the owner the migration sees.
fn record(path: &str, fd: RawFd, stat: &libc::stat, owners: &Owners) -> io::Result<Record> {
    let mut record = record_of(path, fd, stat)?;
    let (uid, gid) = owners.owner(stat);
    record.uid = uid;
    record.gid = gid;
    Ok(record)
}

/// A journal record is read back as untrusted input: only what the
/// migration could have recorded is restored.
fn validate_record(record: &Record, owners: &Owners) -> Result<(), String> {
    if !valid_relative(&record.p) || record.p.len() > 64 * 1024 {
        return Err("an invalid path".into());
    }
    if !owners.managed(record.uid) || record.gid == 0 {
        return Err(format!("{}: an unexpected owner", show(&record.p)));
    }
    if record.mode & !0o7777 != 0 {
        return Err(format!("{}: an invalid mode", show(&record.p)));
    }
    if !record.d && record.dacl.is_some() {
        return Err(format!("{}: a default ACL on a file", show(&record.p)));
    }
    for value in [&record.acl, &record.dacl].into_iter().flatten() {
        let raw = hex::decode(value).map_err(|_| format!("{}: an invalid ACL", show(&record.p)))?;
        if Acl::decode(&raw).is_none() {
            return Err(format!("{}: an invalid ACL", show(&record.p)));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The account
// ---------------------------------------------------------------------------

/// The account being migrated: its identity and paths.
#[derive(Clone)]
struct Account {
    identity: Identity,
    /// The RAST username: the HOME's name.
    username: String,
    /// `<homes>/<username>`.
    home: PathBuf,
}

impl Account {
    fn of(options: &NativeOptions, user_id: i64, username: &str) -> Result<Self, NativeError> {
        if !(1..=MAX_USER_ID).contains(&user_id) {
            return Err(NativeError::invalid(format!(
                "userId must be between 1 and {MAX_USER_ID}"
            )));
        }
        validate_home_username(username)?;
        let identity = Identity::derive(options, user_id)?;
        Ok(Self {
            home: options.homes_dir.join(username),
            username: username.to_string(),
            identity,
        })
    }

    /// The identity's passwd entry; it must exist and name the legacy
    /// identity home or the HOME.
    async fn passwd(&self, options: &NativeOptions) -> Result<PasswdEntry, NativeError> {
        let entry = getent_passwd(options, &self.identity.username)
            .await?
            .ok_or_else(|| {
                NativeError::not_found(format!(
                    "{} does not exist yet; it is made when the account first uses a native app",
                    self.identity.username
                ))
            })?;
        check_existing_identity(&self.identity, Some(&entry), None)?;
        if entry.home != self.identity.home && entry.home != self.home {
            return Err(NativeError::identity_conflict(format!(
                "the home of {} is {}, neither {} nor {}",
                self.identity.username,
                entry.home.display(),
                self.identity.home.display(),
                self.home.display()
            )));
        }
        Ok(entry)
    }

    fn journal_parts(&self) -> [String; 3] {
        [
            "migrations".into(),
            "home".into(),
            self.identity.user_id.to_string(),
        ]
    }

    fn journal_root(
        &self,
        options: &NativeOptions,
        create: bool,
    ) -> Result<Option<OwnedFd>, NativeError> {
        let parts = self.journal_parts();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        open_journal_dir(options, &parts, create)
    }

    fn lock(&self, root: &OwnedFd) -> Result<MigrationLock, NativeError> {
        MigrationLock::acquire_in(
            root,
            &format!(
                "a migration or rollback of the HOME of user {} is still running",
                self.identity.user_id
            ),
        )
    }

    /// The homes root and the HOME (no link followed).
    fn open_home(&self, options: &NativeOptions) -> Result<(OwnedFd, OwnedFd), NativeError> {
        let homes = open_dir(&options.homes_dir).map_err(|error| {
            NativeError::command_failed(format!(
                "cannot open {}: {error}",
                options.homes_dir.display()
            ))
        })?;
        let name = cname(&self.username)?;
        let home = open_at(homes.as_raw_fd(), &name, DIR_FLAGS).map_err(|error| {
            if is_enoent(&error) {
                NativeError::not_found(format!("{} does not exist", self.home.display()))
            } else {
                NativeError::forbidden(format!("{} is not a folder: {error}", self.home.display()))
            }
        })?;
        Ok((homes, home))
    }
}

/// How many processes run as the identity (real or effective uid).
fn identity_processes(options: &NativeOptions, uid: u32) -> Result<u64, NativeError> {
    let proc = proc_dir(options);
    let failed = |error: io::Error| {
        NativeError::command_failed(format!("cannot list {}: {error}", proc.display()))
    };
    let proc_fd = open_dir(&proc).map_err(failed)?;
    let names = dir_names(proc_fd.as_raw_fd(), MAX_PROCESSES)
        .map_err(failed)?
        .unwrap_or_default();
    let mut count = 0;
    for name in names {
        if name.as_bytes().is_empty() || !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let Ok(dir) = open_at(proc_fd.as_raw_fd(), &name, DIR_FLAGS) else {
            continue;
        };
        let Some((real, effective, _)) =
            read_proc_file(&dir, c"status").and_then(|text| parse_proc_status(&text))
        else {
            continue;
        };
        if real == uid || effective == uid {
            count += 1;
        }
    }
    Ok(count)
}

fn refuse_running(options: &NativeOptions, identity: &Identity) -> Result<(), NativeError> {
    let running = identity_processes(options, identity.uid)?;
    if running > 0 {
        return Err(NativeError::identity_in_use(format!(
            "{running} process(es) of {} still run; stop them (for example with \
             `sudo pkill -u {}`) and run the command again",
            identity.username, identity.username
        )));
    }
    Ok(())
}

/// Stop the account's units: its graphical session, apps, terminals and
/// file agent. Fails when one of them still runs afterwards.
async fn stop_user_units(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<Vec<String>, NativeError> {
    let units = running_user_units(options, identity.user_id, identity.uid).await?;
    for unit in &units {
        let _ = stop_unit(options, &format!("{unit}.service")).await;
    }
    stop_files_agent(options, identity.uid).await;
    let left = running_user_units(options, identity.user_id, identity.uid).await?;
    if !left.is_empty() {
        return Err(NativeError::identity_in_use(format!(
            "these units of {} still run: {}",
            identity.username,
            left.join(", ")
        )));
    }
    Ok(units)
}

async fn set_passwd_home(
    options: &NativeOptions,
    identity: &Identity,
    home: &Path,
) -> Result<(), NativeError> {
    let output = run_command(
        &options.usermod_bin,
        vec![
            OsString::from("--home"),
            home.as_os_str().to_owned(),
            OsString::from(&identity.username),
        ],
        &[],
        USERMOD_TIMEOUT,
        64 * 1024,
        64 * 1024,
    )
    .await?;
    if !output.success {
        return Err(NativeError::command_failed(format!(
            "cannot set the home of {}: {}",
            identity.username,
            crate::native::summarize_command(&output)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sources and the merge plan
// ---------------------------------------------------------------------------

/// Where entries come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Root {
    /// The legacy identity home `<data_dir>/linux-users/<userId>`.
    Identity,
    /// `<data_dir>/linux-apps/profiles/<app>/<userId>/<part>`.
    Profile { app: String, part: &'static str },
}

impl Root {
    fn key(&self) -> String {
        match self {
            Self::Identity => "identity".into(),
            Self::Profile { app, part } => format!("{app}/{part}"),
        }
    }

    fn parse(key: &str) -> Option<Self> {
        if key == "identity" {
            return Some(Self::Identity);
        }
        let (app, part) = key.split_once('/')?;
        validate_app_id(app).ok()?;
        let part = PROFILE_PARTS.iter().find(|(name, _)| *name == part)?.0;
        Some(Self::Profile {
            app: app.to_string(),
            part,
        })
    }

    fn source(&self) -> String {
        match self {
            Self::Identity => "identity".into(),
            Self::Profile { app, .. } => app.clone(),
        }
    }

    /// Where its entries go, relative to the HOME.
    fn target(&self) -> &'static str {
        match self {
            Self::Identity => "",
            Self::Profile { part, .. } => PROFILE_PARTS
                .iter()
                .find(|(name, _)| name == part)
                .map_or("", |(_, target)| target),
        }
    }
}

/// A source: the folder that holds it and, when it exists, the folder
/// itself (a part that moved into the HOME as a whole is gone; a rollback
/// moves it back into `parent`).
struct Source {
    root: Root,
    path: PathBuf,
    parent: OwnedFd,
    name: CString,
    open: Option<(OwnedFd, libc::stat)>,
}

/// What a source folder is.
enum Opened {
    /// A real folder of the identity.
    Owned(OwnedFd, libc::stat),
    Missing,
    /// Anything else (a link, a file, a folder of someone else).
    Other,
}

/// Open the folder `parent/name` when it is a real folder of the identity.
fn open_owned(parent: &OwnedFd, name: &str, owners: &Owners) -> Result<Opened, NativeError> {
    let c = cname(name)?;
    let fd = match open_at(parent.as_raw_fd(), &c, DIR_FLAGS) {
        Ok(fd) => fd,
        Err(error) if is_enoent(&error) => return Ok(Opened::Missing),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOTDIR) | Some(libc::ELOOP)
            ) =>
        {
            return Ok(Opened::Other)
        }
        Err(error) => {
            return Err(NativeError::command_failed(format!(
                "cannot open {name}: {error}"
            )))
        }
    };
    let stat = fstat(fd.as_raw_fd())
        .map_err(|error| NativeError::command_failed(format!("cannot inspect {name}: {error}")))?;
    Ok(if owners.owner(&stat).0 == owners.uid {
        Opened::Owned(fd, stat)
    } else {
        Opened::Other
    })
}

/// The identity home and the parts of every app profile of the account,
/// identity home first, then the apps in name order.
fn open_sources(
    options: &NativeOptions,
    account: &Account,
    owners: &Owners,
    merge: &mut MergeReport,
) -> Result<Vec<Source>, NativeError> {
    let user = account.identity.user_id.to_string();
    let data = open_dir(&options.data_dir).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot open {}: {error}",
            options.data_dir.display()
        ))
    })?;
    let mut sources = Vec::new();
    let dir_at = |parent: &OwnedFd, name: &str| -> Option<OwnedFd> {
        open_at(parent.as_raw_fd(), &c_name(name).ok()?, DIR_FLAGS).ok()
    };
    if let Some(users) = dir_at(&data, "linux-users") {
        if let Opened::Owned(fd, stat) = open_owned(&users, &user, owners)? {
            sources.push(Source {
                root: Root::Identity,
                path: options.linux_users_dir().join(&user),
                name: cname(&user)?,
                parent: users,
                open: Some((fd, stat)),
            });
        }
    }
    let Some(profiles) = dir_at(&data, "linux-apps").and_then(|apps| dir_at(&apps, "profiles"))
    else {
        return Ok(sources);
    };
    let mut apps: Vec<String> = dir_names(profiles.as_raw_fd(), MAX_APPS as u64)
        .map_err(|error| NativeError::command_failed(format!("cannot list the profiles: {error}")))?
        .ok_or_else(|| NativeError::forbidden("too many app profiles"))?
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| validate_app_id(name).is_ok())
        .collect();
    apps.sort();
    for app in apps {
        let Some(app_dir) = dir_at(&profiles, &app) else {
            continue;
        };
        // The person's profile folder must be their identity's: a folder
        // of another identity renamed into place is never merged.
        let profile = match open_owned(&app_dir, &user, owners)? {
            Opened::Owned(profile, _) => profile,
            Opened::Missing => continue,
            Opened::Other => {
                merge.skipped.add(&format!(
                    "{app}: the profile does not belong to the account"
                ));
                continue;
            }
        };
        for (part, _) in PROFILE_PARTS {
            let open = match open_owned(&profile, part, owners)? {
                Opened::Owned(fd, stat) => Some((fd, stat)),
                Opened::Missing => None,
                Opened::Other => {
                    merge
                        .skipped
                        .add(&format!("{app}: {part} (not a folder of the account)"));
                    continue;
                }
            };
            sources.push(Source {
                root: Root::Profile {
                    app: app.clone(),
                    part,
                },
                path: options.profiles_dir().join(&app).join(&user).join(part),
                name: cname(part)?,
                parent: open_at(profile.as_raw_fd(), c".", DIR_FLAGS).map_err(|error| {
                    NativeError::command_failed(format!("cannot open a profile: {error}"))
                })?,
                open,
            });
        }
    }
    Ok(sources)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StepKind {
    Mkdir,
    Move,
}

/// One step of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    op: StepKind,
    /// The target, below the HOME.
    dst: String,
    /// Move: the source root ([`Root::key`]) and the path below it (`""`:
    /// the source folder itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    src: String,
    #[serde(default)]
    d: bool,
    #[serde(default)]
    dev: u64,
    #[serde(default)]
    ino: u64,
}

impl Step {
    fn validate(&self) -> Result<Root, String> {
        if self.dst.is_empty() || !valid_relative(&self.dst) || self.dst.len() > 64 * 1024 {
            return Err("an invalid target".into());
        }
        match self.op {
            StepKind::Mkdir => {
                if self.root.is_some() || !self.src.is_empty() {
                    return Err("a folder to create with a source".into());
                }
                Ok(Root::Identity)
            }
            StepKind::Move => {
                let root = self
                    .root
                    .as_deref()
                    .and_then(Root::parse)
                    .ok_or("an invalid source")?;
                if !valid_relative(&self.src) || self.src.len() > 64 * 1024 {
                    return Err("an invalid source path".into());
                }
                if self.src.is_empty() && root == Root::Identity {
                    return Err("the identity home itself cannot move".into());
                }
                Ok(root)
            }
        }
    }
}

/// What will be at a target once the earlier steps ran.
#[derive(Debug, Clone)]
enum Planned {
    /// A folder created by the plan (empty).
    New,
    /// A folder that moves in from `sources[index]` at `src`.
    Moved { index: usize, src: String },
    /// Anything else that moves in.
    Other,
}

/// The folder a merge goes into.
enum Into {
    /// A real folder (of the HOME, or a source folder that moves in).
    Real(OwnedFd),
    /// A folder the plan creates.
    New,
}

enum Found {
    Missing,
    Dir(Into),
    Other,
}

struct Planner<'a> {
    home: &'a OwnedFd,
    home_dev: u64,
    home_mount: u64,
    owners: &'a Owners,
    sources: &'a [Source],
    planned: HashMap<String, Planned>,
    steps: Vec<Step>,
    merge: MergeReport,
    read: u64,
}

impl Planner<'_> {
    /// Open the folder `src` of source `index`.
    fn open_source(&self, index: usize, src: &str) -> Option<OwnedFd> {
        open_rel(&self.sources[index].open.as_ref()?.0, src).ok()
    }

    /// What the HOME has (or will have) at `rel`, the entry `name` of
    /// `into`.
    fn find(&self, into: &Into, rel: &str, name: &str) -> Result<Found, NativeError> {
        if let Some(planned) = self.planned.get(rel) {
            return Ok(match planned {
                Planned::New => Found::Dir(Into::New),
                Planned::Moved { index, src } => match self.open_source(*index, src) {
                    Some(fd) => Found::Dir(Into::Real(fd)),
                    None => Found::Other,
                },
                Planned::Other => Found::Other,
            });
        }
        let Into::Real(dir) = into else {
            return Ok(Found::Missing);
        };
        let c = cname(name)?;
        let stat = match lstat_at(dir.as_raw_fd(), &c) {
            Ok(stat) => stat,
            Err(error) if is_enoent(&error) => return Ok(Found::Missing),
            Err(_) => return Ok(Found::Other),
        };
        let owner = self.owners.owner(&stat).0;
        if !is_dir(&stat) || stat.st_dev != self.home_dev || !self.owners.managed(owner) {
            return Ok(Found::Other);
        }
        let Ok(fd) = open_at(dir.as_raw_fd(), &c, DIR_FLAGS) else {
            return Ok(Found::Other);
        };
        match (fstat(fd.as_raw_fd()), fd_mount_id(fd.as_raw_fd())) {
            (Ok(opened), Ok(mount)) if same_inode(&opened, &stat) && mount == self.home_mount => {
                Ok(Found::Dir(Into::Real(fd)))
            }
            _ => Ok(Found::Other),
        }
    }

    fn conflict(&mut self, index: usize, src: &str) {
        let source = self.sources[index].root.source();
        let shown = join(
            match &self.sources[index].root {
                Root::Identity => "",
                Root::Profile { part, .. } => part,
            },
            src,
        );
        self.merge
            .conflicts
            .add(&format!("{source}: {}", show(&shown)));
        if let Some(report) = self.report(index) {
            report.conflicts += 1;
        }
    }

    fn skip(&mut self, index: usize, src: &str, why: &str) {
        let source = self.sources[index].root.source();
        self.merge
            .skipped
            .add(&format!("{source}: {} ({why})", show(src)));
        if let Some(report) = self.report(index) {
            report.skipped += 1;
        }
    }

    fn report(&mut self, index: usize) -> Option<&mut SourceReport> {
        let source = self.sources[index].root.source();
        self.merge
            .sources
            .iter_mut()
            .find(|report| report.source == source)
    }

    fn mv(&mut self, index: usize, src: &str, dst: &str, stat: &libc::stat) {
        let dir = is_dir(stat);
        self.steps.push(Step {
            op: StepKind::Move,
            dst: dst.to_string(),
            root: Some(self.sources[index].root.key()),
            src: src.to_string(),
            d: dir,
            dev: dev_of(stat),
            ino: ino_of(stat),
        });
        self.planned.insert(
            dst.to_string(),
            if dir {
                Planned::Moved {
                    index,
                    src: src.to_string(),
                }
            } else {
                Planned::Other
            },
        );
        self.merge.moves += 1;
        if let Some(report) = self.report(index) {
            report.moves += 1;
        }
    }

    /// Plan source `index`: find (or plan) its target folder, then merge.
    fn plan_source(&mut self, index: usize) -> Result<(), NativeError> {
        let source = &self.sources[index];
        let target = source.root.target();
        let Some((source_fd, stat)) = &source.open else {
            return Ok(());
        };
        let stat = *stat;
        if stat.st_dev != self.home_dev
            || fd_mount_id(source_fd.as_raw_fd()).ok() != Some(self.home_mount)
        {
            if let Some(report) = self.report(index) {
                report.other_file_system = true;
            }
            return Ok(());
        }
        let mut into = Into::Real(open_at(self.home.as_raw_fd(), c".", DIR_FLAGS).map_err(
            |error| NativeError::command_failed(format!("cannot open the HOME: {error}")),
        )?);
        let mut rel = String::new();
        let parts: Vec<&str> = target.split('/').filter(|part| !part.is_empty()).collect();
        for (position, part) in parts.iter().enumerate() {
            let next = join(&rel, part);
            match self.find(&into, &next, part)? {
                Found::Dir(found) => into = found,
                Found::Missing if position + 1 == parts.len() => {
                    // The whole part moves in as the folder.
                    self.mv(index, "", &next, &stat);
                    return Ok(());
                }
                Found::Missing => {
                    self.steps.push(Step {
                        op: StepKind::Mkdir,
                        dst: next.clone(),
                        root: None,
                        src: String::new(),
                        d: true,
                        dev: 0,
                        ino: 0,
                    });
                    self.planned.insert(next.clone(), Planned::New);
                    self.merge.folders_created += 1;
                    into = Into::New;
                }
                Found::Other => {
                    self.conflict(index, "");
                    return Ok(());
                }
            }
            rel = next;
        }
        let fd = open_at(source_fd.as_raw_fd(), c".", DIR_FLAGS).map_err(|error| {
            NativeError::command_failed(format!("cannot open a source: {error}"))
        })?;
        self.merge_dir(index, fd, String::new(), into, rel)
    }

    /// Merge the source folder `dir` (`src` below its root) into `into`
    /// (`dst` below the HOME).
    fn merge_dir(
        &mut self,
        index: usize,
        dir: OwnedFd,
        src: String,
        into: Into,
        dst: String,
    ) -> Result<(), NativeError> {
        struct Frame {
            dir: OwnedFd,
            src: String,
            into: Into,
            dst: String,
            names: std::vec::IntoIter<CString>,
        }
        let mount = self.home_mount;
        let read_names = |planner: &mut Self, fd: &OwnedFd| -> Result<Vec<CString>, NativeError> {
            let mut names = dir_names(
                fd.as_raw_fd(),
                MAX_TREE_ENTRIES.saturating_sub(planner.read),
            )
            .map_err(|error| NativeError::command_failed(format!("cannot read a folder: {error}")))?
            .ok_or_else(too_many)?;
            planner.read += names.len() as u64;
            names.sort();
            Ok(names)
        };
        let names = read_names(self, &dir)?;
        let mut stack = vec![Frame {
            dir,
            src,
            into,
            dst,
            names: names.into_iter(),
        }];
        while let Some(frame) = stack.last_mut() {
            let Some(name) = frame.names.next() else {
                stack.pop();
                continue;
            };
            let Ok(text) = name.to_str().map(str::to_string) else {
                let src = frame.src.clone();
                self.skip(index, &src, "a name that is not UTF-8");
                continue;
            };
            let src = join(&frame.src, &text);
            let dst = join(&frame.dst, &text);
            if self.sources[index].root == Root::Identity
                && frame.src.is_empty()
                && text.starts_with(".Xauthority")
            {
                self.merge.regenerated.push(text);
                continue;
            }
            let parent = frame.dir.as_raw_fd();
            let Ok(stat) = lstat_at(parent, &name) else {
                continue;
            };
            if self.owners.owner(&stat).0 != self.owners.uid {
                self.skip(index, &src, "not the account's");
                continue;
            }
            if stat.st_dev != self.home_dev {
                self.skip(index, &src, "another file system");
                continue;
            }
            if is_dir(&stat) && mount_id(parent, &name).ok() != Some(mount) {
                self.skip(index, &src, "a mount point");
                continue;
            }
            let found = {
                let frame = stack.last().expect("frame");
                self.find(&frame.into, &dst, &text)?
            };
            match found {
                Found::Missing => self.mv(index, &src, &dst, &stat),
                Found::Dir(into) if is_dir(&stat) => {
                    let parent = stack.last().expect("frame").dir.as_raw_fd();
                    let Ok(child) = open_at(parent, &name, DIR_FLAGS) else {
                        continue;
                    };
                    if !fstat(child.as_raw_fd()).is_ok_and(|opened| same_inode(&opened, &stat)) {
                        continue;
                    }
                    if stack.len() >= MAX_DEPTH {
                        return Err(NativeError::forbidden(format!(
                            "a profile is nested deeper than {MAX_DEPTH} levels"
                        )));
                    }
                    let names = read_names(self, &child)?;
                    stack.push(Frame {
                        dir: child,
                        src,
                        into,
                        dst,
                        names: names.into_iter(),
                    });
                }
                _ => self.conflict(index, &src),
            }
        }
        Ok(())
    }
}

/// Size of a source tree: folders, files and bytes (no link followed).
fn source_size(fd: &OwnedFd, report: &mut SourceReport) -> Result<(), NativeError> {
    let mut stack = vec![open_at(fd.as_raw_fd(), c".", DIR_FLAGS)
        .map_err(|error| NativeError::command_failed(format!("cannot open a source: {error}")))?];
    let mut read = 0u64;
    while let Some(dir) = stack.pop() {
        let names = dir_names(dir.as_raw_fd(), MAX_TREE_ENTRIES.saturating_sub(read))
            .map_err(|error| NativeError::command_failed(format!("cannot read a folder: {error}")))?
            .ok_or_else(too_many)?;
        read += names.len() as u64;
        for name in names {
            let Ok(stat) = lstat_at(dir.as_raw_fd(), &name) else {
                continue;
            };
            if is_dir(&stat) {
                report.dirs += 1;
                if stack.len() < MAX_DEPTH {
                    if let Ok(child) = open_at(dir.as_raw_fd(), &name, DIR_FLAGS) {
                        stack.push(child);
                    }
                }
            } else if is_regular(&stat) {
                report.files += 1;
                report.bytes = report.bytes.saturating_add(stat.st_size.max(0) as u64);
            }
        }
    }
    Ok(())
}

/// Plan the merge of every source into the HOME.
fn plan(
    home: &OwnedFd,
    owners: &Owners,
    sources: &[Source],
    mut merge: MergeReport,
    sizes: bool,
) -> Result<(Vec<Step>, MergeReport), NativeError> {
    let home_stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let home_mount = fd_mount_id(home.as_raw_fd())
        .map_err(|error| NativeError::unavailable(error.to_string()))?;
    for source in sources {
        let name = source.root.source();
        if merge.sources.iter().any(|report| report.source == name) {
            continue;
        }
        let mut report = SourceReport {
            source: name,
            path: match &source.root {
                Root::Identity => source.path.display().to_string(),
                Root::Profile { .. } => source
                    .path
                    .parent()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            },
            ..Default::default()
        };
        if sizes {
            let name = report.source.clone();
            for other in sources.iter().filter(|other| other.root.source() == name) {
                if let Some((fd, _)) = &other.open {
                    source_size(fd, &mut report)?;
                }
            }
        }
        merge.sources.push(report);
    }
    let mut planner = Planner {
        home,
        home_dev: home_stat.st_dev,
        home_mount,
        owners,
        sources,
        planned: HashMap::new(),
        steps: Vec::new(),
        merge,
        read: 0,
    };
    for index in 0..sources.len() {
        planner.plan_source(index)?;
    }
    Ok((planner.steps, planner.merge))
}

/// The source of a step, open: its folder and, for the folder itself, the
/// folder that holds it.
fn step_source<'a>(sources: &'a [Source], key: &str) -> Option<&'a Source> {
    sources.iter().find(|source| source.root.key() == key)
}

/// The folder that holds `rel` below `root` (or the source's own parent
/// for the source itself), and its name.
fn holder(source: &Source, rel: &str) -> io::Result<(OwnedFd, CString)> {
    if rel.is_empty() {
        let parent = open_at(source.parent.as_raw_fd(), c".", DIR_FLAGS)?;
        return Ok((parent, source.name.clone()));
    }
    let (parent, name) = split(rel);
    let Some((fd, _)) = &source.open else {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    };
    Ok((open_rel(fd, parent)?, c_name(name)?))
}

/// What running (or rolling back) the plan did.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepsReport {
    pub moved: u64,
    pub already: u64,
    pub created: u64,
    /// Steps whose entry is no longer where the plan found it.
    pub missing: Sample,
    /// Steps whose target is taken (left where they are).
    pub conflicts: Sample,
}

fn run_steps(
    home: &OwnedFd,
    owners: &Owners,
    sources: &[Source],
    steps: &[Step],
) -> Result<StepsReport, NativeError> {
    let mut report = StepsReport::default();
    for step in steps {
        let (parent_rel, name) = split(&step.dst);
        let Ok(parent) = open_rel(home, parent_rel) else {
            report.missing.add(&step.dst);
            continue;
        };
        match step.op {
            StepKind::Mkdir => {
                let (_, created) = ensure_dir(&parent, name, owners, &step.dst)?;
                if created {
                    report.created += 1;
                }
            }
            StepKind::Move => {
                let dst = cname(name)?;
                let recorded = |stat: &libc::stat| {
                    dev_of(stat) == step.dev && ino_of(stat) == step.ino && is_dir(stat) == step.d
                };
                let source = step
                    .root
                    .as_deref()
                    .and_then(|key| step_source(sources, key));
                let from = source.and_then(|source| holder(source, &step.src).ok());
                let here = from
                    .as_ref()
                    .and_then(|(dir, name)| lstat_at(dir.as_raw_fd(), name).ok())
                    .filter(recorded);
                match (here, from) {
                    (Some(_), Some((dir, src))) => {
                        match rename_noreplace(dir.as_raw_fd(), &src, parent.as_raw_fd(), &dst) {
                            Ok(()) => report.moved += 1,
                            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                                report.conflicts.add(&step.dst)
                            }
                            Err(error) => return Err(io_failed("move into", &step.dst, error)),
                        }
                    }
                    _ => {
                        // Already moved by a run that stopped half way?
                        if lstat_at(parent.as_raw_fd(), &dst).is_ok_and(|stat| recorded(&stat)) {
                            report.already += 1;
                        } else {
                            report.missing.add(&step.dst);
                        }
                    }
                }
            }
        }
    }
    Ok(report)
}

fn undo_steps(
    home: &OwnedFd,
    sources: &[Source],
    steps: &[Step],
) -> Result<StepsReport, NativeError> {
    let mut report = StepsReport::default();
    for step in steps.iter().rev() {
        let (parent_rel, name) = split(&step.dst);
        let Ok(parent) = open_rel(home, parent_rel) else {
            report.missing.add(&step.dst);
            continue;
        };
        let dst = cname(name)?;
        match step.op {
            StepKind::Mkdir => match unlink_at(parent.as_raw_fd(), &dst, true) {
                Ok(()) => report.created += 1,
                Err(error) if is_enoent(&error) => {}
                // Not empty: what is in it now stays in the HOME.
                Err(_) => report.conflicts.add(&step.dst),
            },
            StepKind::Move => {
                let here = lstat_at(parent.as_raw_fd(), &dst).ok().filter(|stat| {
                    dev_of(stat) == step.dev && ino_of(stat) == step.ino && is_dir(stat) == step.d
                });
                let source = step
                    .root
                    .as_deref()
                    .and_then(|key| step_source(sources, key));
                let to = source.and_then(|source| holder(source, &step.src).ok());
                match (here, to) {
                    (Some(_), Some((dir, src))) => {
                        match rename_noreplace(parent.as_raw_fd(), &dst, dir.as_raw_fd(), &src) {
                            Ok(()) => report.moved += 1,
                            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                                report.conflicts.add(&step.dst)
                            }
                            Err(error) => return Err(io_failed("move back", &step.dst, error)),
                        }
                    }
                    _ => report.missing.add(&step.dst),
                }
            }
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

fn read_lines<T: for<'de> Deserialize<'de>>(
    dir: &OwnedFd,
    name: &str,
    what: &str,
) -> Result<Vec<T>, NativeError> {
    let file = open_journal_file(dir, name, journal_owner(), "home")?;
    let mut out = Vec::new();
    for (index, line) in io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| {
            NativeError::command_failed(format!("cannot read the {what}: {error}"))
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if out.len() as u64 > MAX_TREE_ENTRIES {
            return Err(NativeError::forbidden(format!("the {what} is too large")));
        }
        out.push(serde_json::from_str(&line).map_err(|_| {
            NativeError::forbidden(format!("line {} of the {what} is invalid", index + 1))
        })?);
    }
    Ok(out)
}

/// The journal of a migration: the HOME as it was, and the plan.
struct Journal {
    stamp: String,
    dir: OwnedFd,
    records: Vec<Record>,
    steps: Vec<Step>,
    applied: bool,
}

fn read_journal(stamp: String, dir: OwnedFd, owners: &Owners) -> Result<Journal, NativeError> {
    let records: Vec<Record> = read_lines(&dir, JOURNAL_FILE, "home journal")?;
    for (index, record) in records.iter().enumerate() {
        validate_record(record, owners).map_err(|why| {
            NativeError::forbidden(format!(
                "line {} of the home journal is refused: {why}",
                index + 1
            ))
        })?;
    }
    match records.first() {
        Some(first) if first.p.is_empty() && first.d && first.uid == owners.rast => {}
        _ => {
            return Err(NativeError::forbidden(
                "the home journal does not start with a HOME of the RAST service",
            ))
        }
    }
    let steps: Vec<Step> = read_lines(&dir, PLAN_FILE, "home plan")?;
    for (index, step) in steps.iter().enumerate() {
        step.validate().map_err(|why| {
            NativeError::forbidden(format!(
                "line {} of the home plan is refused: {why}",
                index + 1
            ))
        })?;
    }
    let applied = lstat_at(dir.as_raw_fd(), &cname(APPLIED_FILE)?).is_ok();
    Ok(Journal {
        stamp,
        dir,
        records,
        steps,
        applied,
    })
}

fn active_journal(root: &OwnedFd, owners: &Owners) -> Result<Option<Journal>, NativeError> {
    match newest_journal(root, "home")? {
        Some((stamp, dir)) => read_journal(stamp, dir, owners).map(Some),
        None => Ok(None),
    }
}

/// Create the next stamp folder below `root` and write the plan, then the
/// journal (the journal last: a folder without it is not a journal).
fn write_journal(
    root: &OwnedFd,
    records: &[Record],
    steps: &[Step],
) -> Result<(String, OwnedFd), NativeError> {
    let mut stamp = crate::native::shares::now_stamp();
    while lstat_at(root.as_raw_fd(), &cname(&stamp)?).is_ok() {
        let next: u64 = stamp.parse().unwrap_or_default();
        stamp = format!("{:012}", next + 1);
    }
    let name = cname(&stamp)?;
    mkdir_at(root.as_raw_fd(), &name).map_err(|error| {
        NativeError::command_failed(format!("cannot create the journal folder: {error}"))
    })?;
    let dir = open_at(root.as_raw_fd(), &name, DIR_FLAGS).map_err(|error| {
        NativeError::command_failed(format!("cannot open the journal folder: {error}"))
    })?;
    write_synced(&dir, PLAN_FILE, steps)?;
    write_synced(&dir, JOURNAL_FILE, records)?;
    Ok((stamp, dir))
}

// ---------------------------------------------------------------------------
// home_migrate_scan
// ---------------------------------------------------------------------------

/// The state of an account's HOME.
fn mode_of(owners: &Owners, home: &libc::stat) -> &'static str {
    let owner = owners.owner(home).0;
    if owner == owners.uid {
        "oneHome"
    } else if owner == owners.rast {
        "legacy"
    } else {
        "foreign"
    }
}

/// The names in the HOME's `Shared` folder, which collide with the links
/// to shared folders that one-HOME mode makes there.
fn shared_names(home: &OwnedFd) -> Option<Vec<String>> {
    let shared = open_at(home.as_raw_fd(), c"Shared", DIR_FLAGS).ok()?;
    let mut names: Vec<String> = dir_names(shared.as_raw_fd(), MAX_TREE_ENTRIES)
        .ok()??
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .collect();
    names.sort();
    names.truncate(SHARED_LISTED);
    Some(names)
}

pub(crate) async fn home_migrate_scan(
    options: &NativeOptions,
    op: HomeMigrateOp,
) -> Result<Value, NativeError> {
    let account = Account::of(options, op.user_id, &op.username)?;
    let passwd = getent_passwd(options, &account.identity.username).await?;
    if let Some(entry) = &passwd {
        check_existing_identity(&account.identity, Some(entry), None)?;
    }
    let units = running_user_units(options, op.user_id, account.identity.uid).await?;
    let options = options.clone();
    let scan = account.clone();
    let mut reply = blocking(move || {
        let account = scan;
        let owners = Owners::of(&options, &account.identity)?;
        let processes = identity_processes(&options, owners.uid)?;
        let (homes, home) = match account.open_home(&options) {
            Ok(found) => found,
            Err(error) if error.code == "not_found" => {
                return Ok(json!({ "mode": "missing", "processes": processes }))
            }
            Err(error) => return Err(error),
        };
        let home_stat = fstat(home.as_raw_fd()).map_err(|error| {
            NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
        })?;
        let mode = mode_of(&owners, &home_stat);
        let journal = match account.journal_root(&options, false)? {
            Some(root) => active_journal(&root, &owners)?,
            None => None,
        };
        let stray = lstat_at(homes.as_raw_fd(), &cname(&account.identity.username)?).is_ok();
        let mut reply = json!({
            "mode": mode,
            "journal": journal.as_ref().map(|journal| journal.stamp.clone()),
            "applied": journal.as_ref().is_some_and(|journal| journal.applied),
            "processes": processes,
            "strayHome": stray,
            "shared": shared_names(&home),
        });
        if mode == "legacy" {
            let mut tree = HomeTree::default();
            convert(
                home.as_raw_fd(),
                &home_stat,
                owners.owner(&home_stat),
                "",
                &owners,
                &mut tree,
                false,
            )?;
            walk_tree(&home, &owners, &mut tree, |visit, tree| {
                match visit.open() {
                    Some((fd, stat)) => convert(
                        fd.as_raw_fd(),
                        &stat,
                        visit.owner,
                        visit.path,
                        &owners,
                        tree,
                        false,
                    )?,
                    None => tree.changed_during_walk.add(visit.path),
                }
                Ok(())
            })?;
            let mut merge = MergeReport::default();
            let sources = open_sources(&options, &account, &owners, &mut merge)?;
            let (steps, merge) = plan(&home, &owners, &sources, merge, true)?;
            reply["tree"] = serde_json::to_value(&tree).unwrap_or(Value::Null);
            reply["merge"] = serde_json::to_value(&merge).unwrap_or(Value::Null);
            reply["steps"] = json!(steps.len());
        }
        Ok(reply)
    })
    .await?;
    reply["userId"] = json!(op.user_id);
    reply["username"] = json!(op.username);
    reply["home"] = json!(account.home.display().to_string());
    reply["passwdHome"] = json!(passwd.map(|entry| entry.home.display().to_string()));
    reply["units"] = json!(units);
    Ok(reply)
}

// ---------------------------------------------------------------------------
// home_migrate_apply
// ---------------------------------------------------------------------------

/// What the first part of an apply hands to the second.
struct Applying {
    _lock: MigrationLock,
    journal: String,
    resumed: bool,
    already: bool,
    tree: HomeTree,
    merge: Option<MergeReport>,
    steps: StepsReport,
}

fn apply_tree(options: &NativeOptions, account: &Account) -> Result<Applying, NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let (_, home) = account.open_home(options)?;
    let home_stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let root = account.journal_root(options, true)?.expect("created");
    let lock = account.lock(&root)?;
    refuse_running(options, &account.identity)?;
    let done = |lock, journal: String| Applying {
        _lock: lock,
        journal,
        resumed: false,
        already: true,
        tree: HomeTree::default(),
        merge: None,
        steps: StepsReport::default(),
    };
    let mut merge = None;
    let (journal, resumed) = match active_journal(&root, &owners)? {
        Some(journal) if journal.applied => return Ok(done(lock, journal.stamp)),
        Some(journal) => {
            let first = &journal.records[0];
            if first.dev != dev_of(&home_stat) || first.ino != ino_of(&home_stat) {
                return Err(NativeError::forbidden(
                    "the HOME is not the folder the journal recorded",
                ));
            }
            (journal, true)
        }
        None => {
            match mode_of(&owners, &home_stat) {
                "oneHome" => return Ok(done(lock, String::new())),
                "legacy" => {}
                _ => {
                    return Err(NativeError::identity_conflict(format!(
                        "{} belongs neither to the RAST service nor to {}",
                        account.home.display(),
                        account.identity.username
                    )))
                }
            }
            let mut records =
                vec![
                    record("", home.as_raw_fd(), &home_stat, &owners).map_err(|error| {
                        NativeError::command_failed(format!("cannot record the HOME: {error}"))
                    })?,
                ];
            let mut scan = HomeTree::default();
            walk_tree(&home, &owners, &mut scan, |visit, tree| {
                match visit
                    .open()
                    .and_then(|(fd, stat)| record(visit.path, fd.as_raw_fd(), &stat, &owners).ok())
                {
                    Some(record) => records.push(record),
                    None => tree.changed_during_walk.add(visit.path),
                }
                Ok(())
            })?;
            let mut report = MergeReport::default();
            let sources = open_sources(options, account, &owners, &mut report)?;
            let (steps, report) = plan(&home, &owners, &sources, report, false)?;
            merge = Some(report);
            let (stamp, dir) = write_journal(&root, &records, &steps)?;
            (
                Journal {
                    stamp,
                    dir,
                    records,
                    steps,
                    applied: false,
                },
                false,
            )
        }
    };
    // Every entry of the HOME (the HOME itself comes last, in `finish`).
    let mut tree = HomeTree::default();
    walk_tree(&home, &owners, &mut tree, |visit, tree| {
        match visit.open() {
            Some((fd, stat)) => convert(
                fd.as_raw_fd(),
                &stat,
                visit.owner,
                visit.path,
                &owners,
                tree,
                true,
            )?,
            None => tree.changed_during_walk.add(visit.path),
        }
        Ok(())
    })?;
    let mut ignored = MergeReport::default();
    let sources = open_sources(options, account, &owners, &mut ignored)?;
    let steps = run_steps(&home, &owners, &sources, &journal.steps)?;
    Ok(Applying {
        _lock: lock,
        journal: journal.stamp,
        resumed,
        already: false,
        tree,
        merge,
        steps,
    })
}

/// Give the HOME itself to the identity (0700, no ACLs) and mark the
/// journal as applied.
fn finish(options: &NativeOptions, account: &Account, stamp: &str) -> Result<(), NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let (_, home) = account.open_home(options)?;
    let stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let root = account
        .journal_root(options, false)?
        .ok_or_else(|| NativeError::forbidden("the journal of the migration is gone"))?;
    let dir = open_at(root.as_raw_fd(), &cname(stamp)?, DIR_FLAGS).map_err(|error| {
        NativeError::command_failed(format!("cannot open the journal: {error}"))
    })?;
    let journal = read_journal(stamp.to_string(), dir, &owners)?;
    let first = &journal.records[0];
    if first.dev != dev_of(&stat) || first.ino != ino_of(&stat) {
        return Err(NativeError::forbidden(
            "the HOME is not the folder the journal recorded",
        ));
    }
    let failed = |what: &str, error: io::Error| io_failed(what, "the HOME", error);
    owners
        .chown(home.as_raw_fd(), &stat, owners.uid, owners.gid)
        .map_err(|e| failed("chown", e))?;
    acl::remove(home.as_raw_fd(), acl::ACCESS_XATTR).map_err(|e| failed("change the ACL of", e))?;
    acl::remove(home.as_raw_fd(), acl::DEFAULT_XATTR)
        .map_err(|e| failed("change the ACL of", e))?;
    fchmod(home.as_raw_fd(), 0o700).map_err(|e| failed("chmod", e))?;
    write_synced::<Value>(&journal.dir, APPLIED_FILE, &[])
}

/// Where an account stands, read without changing anything: the mode of
/// its HOME and its newest journal (`Some(applied)`).
fn state(
    options: &NativeOptions,
    account: &Account,
) -> Result<(&'static str, Option<bool>), NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let (_, home) = account.open_home(options)?;
    let stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let journal = match account.journal_root(options, false)? {
        Some(root) => active_journal(&root, &owners)?.map(|journal| journal.applied),
        None => None,
    };
    Ok((mode_of(&owners, &stat), journal))
}

pub(crate) async fn home_migrate_apply(
    options: &NativeOptions,
    op: HomeMigrateOp,
) -> Result<Value, NativeError> {
    let account = Account::of(options, op.user_id, &op.username)?;
    let passwd = account.passwd(options).await?;
    // An account that is done keeps its session running.
    let (opts, acc) = (options.clone(), account.clone());
    let (mode, journal) = blocking(move || state(&opts, &acc)).await?;
    if mode == "oneHome" && journal != Some(false) && passwd.home == account.home {
        return Ok(json!({
            "userId": op.user_id,
            "username": op.username,
            "alreadyOneHome": true,
            "stopped": [],
        }));
    }
    let stopped = stop_user_units(options, &account.identity).await?;
    let (opts, acc) = (options.clone(), account.clone());
    let applying = blocking(move || apply_tree(&opts, &acc)).await?;
    if applying.already {
        if passwd.home != account.home {
            set_passwd_home(options, &account.identity, &account.home).await?;
        }
        return Ok(json!({
            "userId": op.user_id,
            "username": op.username,
            "alreadyOneHome": true,
            "journal": Some(applying.journal).filter(|stamp| !stamp.is_empty()),
            "stopped": stopped,
        }));
    }
    if passwd.home != account.home {
        set_passwd_home(options, &account.identity, &account.home).await?;
    }
    let (opts, acc, stamp) = (options.clone(), account.clone(), applying.journal.clone());
    blocking(move || finish(&opts, &acc, &stamp)).await?;
    tracing::info!(
        user_id = op.user_id,
        journal = %applying.journal,
        resumed = applying.resumed,
        moved = applying.steps.moved,
        "HOME migrated to one-HOME mode"
    );
    Ok(json!({
        "userId": op.user_id,
        "username": op.username,
        "alreadyOneHome": false,
        "journal": applying.journal,
        "resumed": applying.resumed,
        "stopped": stopped,
        "tree": applying.tree,
        "merge": applying.merge,
        "steps": applying.steps,
    }))
}

// ---------------------------------------------------------------------------
// home_migrate_trash
// ---------------------------------------------------------------------------

/// A folder a batch created, to remove (when empty) on rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrashDir {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    path: String,
}

/// One entry a batch moves: from `<storage>/.trash/<userId>/<id>` to
/// `files/<name>` of the HOME's or the container's trash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrashMove {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    name: String,
    /// The payload as it was (`""` first: the payload itself).
    records: Vec<Record>,
}

/// One line of a batch journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrashLine {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    created: Vec<TrashDir>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    moved: Option<TrashMove>,
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

fn validate_trash_entry(entry: &LegacyTrashEntry) -> Result<(), NativeError> {
    if !valid_uuid(&entry.id) {
        return Err(NativeError::invalid(
            "a recycle bin id is not a lowercase UUID",
        ));
    }
    if entry.deleted_at < 0 {
        return Err(NativeError::invalid("a deletion time is negative"));
    }
    if entry.path.is_empty()
        || entry.path.len() > 4096
        || !valid_relative(&entry.path)
        || entry
            .path
            .split('/')
            .any(|part| rast_base::storage::safe_path::validate_component(part).is_err())
    {
        return Err(NativeError::invalid(format!(
            "the original path of recycle bin entry {} is invalid",
            entry.id
        )));
    }
    if let Some(container) = &entry.container {
        validate_share_container(container)?;
    }
    Ok(())
}

fn validate_trash_line(line: &TrashLine, owners: &Owners) -> Result<(), String> {
    for dir in &line.created {
        if !valid_relative(&dir.path) || dir.path.is_empty() {
            return Err("an invalid folder".into());
        }
        if let Some(container) = &dir.container {
            validate_share_container(container).map_err(|error| error.message)?;
        }
    }
    if let Some(moved) = &line.moved {
        if !valid_uuid(&moved.id)
            || rast_base::storage::safe_path::validate_component(&moved.name).is_err()
        {
            return Err("an invalid entry".into());
        }
        if let Some(container) = &moved.container {
            validate_share_container(container).map_err(|error| error.message)?;
        }
        match moved.records.first() {
            Some(first) if first.p.is_empty() => {}
            _ => return Err("an entry without its payload".into()),
        }
        for record in &moved.records {
            validate_record(record, owners)?;
        }
    }
    Ok(())
}

/// The trash folder of the HOME (`.local/share/Trash`) or of a container
/// (`.Trash-<uid>`), relative to the HOME or to the container.
fn trash_rel(container: Option<&str>, uid: u32) -> String {
    match container {
        None => rast_base::storage::xdg_trash::HOME_TRASH.join("/"),
        Some(_) => rast_base::storage::xdg_trash::share_trash_name(uid),
    }
}

/// The folder a trash folder lives in: the HOME or the container.
fn trash_base(
    options: &NativeOptions,
    home: &OwnedFd,
    container: Option<&str>,
) -> Result<OwnedFd, NativeError> {
    match container {
        None => open_at(home.as_raw_fd(), c".", DIR_FLAGS)
            .map_err(|error| NativeError::command_failed(format!("cannot open the HOME: {error}"))),
        Some(container) => {
            let shared = options.storage_dir.join("shared");
            let root = open_dir(&shared).map_err(|error| {
                NativeError::command_failed(format!("cannot open {}: {error}", shared.display()))
            })?;
            open_at(root.as_raw_fd(), &cname(container)?, DIR_FLAGS).map_err(|error| {
                NativeError::not_found(format!("the shared folder {container} is gone: {error}"))
            })
        }
    }
}

/// Open (creating what is missing, for the identity) `files` and `info` of
/// a trash folder; the folders made are added to `created`.
fn ensure_trash(
    base: &OwnedFd,
    container: Option<&str>,
    owners: &Owners,
    created: &mut Vec<TrashDir>,
) -> Result<(OwnedFd, OwnedFd), NativeError> {
    let rel = trash_rel(container, owners.uid);
    let mut dir = open_at(base.as_raw_fd(), c".", DIR_FLAGS)
        .map_err(|error| NativeError::command_failed(format!("cannot open a folder: {error}")))?;
    let mut path = String::new();
    let keep_group = container.is_some();
    for part in rel.split('/') {
        path = join(&path, part);
        let (next, made) = ensure_dir_in(&dir, part, owners, &path, keep_group)?;
        if made {
            created.push(TrashDir {
                container: container.map(str::to_string),
                path: path.clone(),
            });
        }
        dir = next;
    }
    let mut out = Vec::new();
    for part in ["files", "info"] {
        let full = join(&path, part);
        let (fd, made) = ensure_dir_in(&dir, part, owners, &full, keep_group)?;
        if made {
            created.push(TrashDir {
                container: container.map(str::to_string),
                path: full,
            });
        }
        out.push(fd);
    }
    let info = out.pop().expect("info");
    let files = out.pop().expect("files");
    Ok((files, info))
}

/// Create the info file `<name>.trashinfo` (never over another one).
fn write_info(info: &OwnedFd, name: &str, body: &str, owners: &Owners) -> io::Result<bool> {
    use std::io::Write;
    let file_name = c_name(&rast_base::storage::xdg_trash::info_name(name))?;
    // SAFETY: valid fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            info.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            return Ok(false);
        }
        return Err(error);
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let stat = fstat(fd.as_raw_fd())?;
    owners.chown(fd.as_raw_fd(), &stat, owners.uid, owners.gid)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(true)
}

/// The legacy recycle bin of the account, `<storage>/.trash/<userId>`.
fn legacy_trash(options: &NativeOptions, user_id: i64) -> Result<Option<OwnedFd>, NativeError> {
    let storage = open_dir(&options.storage_dir).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot open {}: {error}",
            options.storage_dir.display()
        ))
    })?;
    let Ok(trash) = open_at(storage.as_raw_fd(), c".trash", DIR_FLAGS) else {
        return Ok(None);
    };
    Ok(open_at(trash.as_raw_fd(), &cname(&user_id.to_string())?, DIR_FLAGS).ok())
}

/// The batch journals of a migration, in order.
fn trash_batches(dir: &OwnedFd) -> Result<Vec<String>, NativeError> {
    let mut names: Vec<String> = dir_names(dir.as_raw_fd(), (MAX_TRASH_BATCHES + 16) as u64)
        .map_err(|error| NativeError::command_failed(format!("cannot read the journal: {error}")))?
        .ok_or_else(|| NativeError::forbidden("too many recycle bin batches"))?
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| {
            name.strip_prefix(TRASH_PREFIX)
                .and_then(|rest| rest.strip_suffix(".jsonl"))
                .is_some_and(|number| {
                    number.len() == 4 && number.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .collect();
    names.sort();
    Ok(names)
}

/// The moves of every batch so far, by id.
fn moved_so_far(dir: &OwnedFd, owners: &Owners) -> Result<HashMap<String, TrashMove>, NativeError> {
    let mut out = HashMap::new();
    for batch in trash_batches(dir)? {
        let lines: Vec<TrashLine> = read_lines(dir, &batch, "recycle bin journal")?;
        for (index, line) in lines.into_iter().enumerate() {
            validate_trash_line(&line, owners).map_err(|why| {
                NativeError::forbidden(format!("line {} of {batch} is refused: {why}", index + 1))
            })?;
            if let Some(moved) = line.moved {
                out.insert(moved.id.clone(), moved);
            }
        }
    }
    Ok(out)
}

/// The records of a payload (the payload first) and whether all of it can
/// be given to the identity.
fn payload_records(
    parent: &OwnedFd,
    name: &CStr,
    owners: &Owners,
) -> Result<Option<(Vec<Record>, libc::stat)>, String> {
    let stat = lstat_at(parent.as_raw_fd(), name).map_err(|error| error.to_string())?;
    let owner = owners.owner(&stat).0;
    if !owners.managed(owner) {
        return Err("it belongs to another account".into());
    }
    if is_regular(&stat) {
        if stat.st_nlink > 1 {
            return Err("it is a hard link".into());
        }
        let fd = open_at(parent.as_raw_fd(), name, FILE_FLAGS).map_err(|e| e.to_string())?;
        let opened = fstat(fd.as_raw_fd()).map_err(|e| e.to_string())?;
        if !same_inode(&opened, &stat) {
            return Ok(None);
        }
        let record = record("", fd.as_raw_fd(), &opened, owners).map_err(|e| e.to_string())?;
        return Ok(Some((vec![record], opened)));
    }
    if !is_dir(&stat) {
        return Err("it is neither a file nor a folder".into());
    }
    let fd = open_at(parent.as_raw_fd(), name, DIR_FLAGS).map_err(|e| e.to_string())?;
    let opened = fstat(fd.as_raw_fd()).map_err(|e| e.to_string())?;
    if !same_inode(&opened, &stat) {
        return Ok(None);
    }
    let mut records = vec![record("", fd.as_raw_fd(), &opened, owners).map_err(|e| e.to_string())?];
    let mut tree = HomeTree::default();
    walk_tree(&fd, owners, &mut tree, |visit, tree| {
        match visit
            .open()
            .and_then(|(fd, stat)| record(visit.path, fd.as_raw_fd(), &stat, owners).ok())
        {
            Some(record) => records.push(record),
            None => tree.changed_during_walk.add(visit.path),
        }
        Ok(())
    })
    .map_err(|error| error.message)?;
    Ok(Some((records, opened)))
}

/// Give a moved payload to the identity, entry by entry, with the group
/// `group`.
fn convert_payload(
    files: &OwnedFd,
    name: &CStr,
    owners: &Owners,
    group: u32,
) -> Result<(), NativeError> {
    let stat = lstat_at(files.as_raw_fd(), name).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect an entry: {error}"))
    })?;
    let mut tree = HomeTree::default();
    let flags = if is_dir(&stat) { DIR_FLAGS } else { FILE_FLAGS };
    let fd = open_at(files.as_raw_fd(), name, flags)
        .map_err(|error| NativeError::command_failed(format!("cannot open an entry: {error}")))?;
    let opened = fstat(fd.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect an entry: {error}"))
    })?;
    if !same_inode(&opened, &stat) {
        return Err(NativeError::forbidden(
            "an entry changed while it was moved",
        ));
    }
    if is_dir(&stat) {
        walk_tree(&fd, owners, &mut tree, |visit, tree| {
            if let Some((fd, stat)) = visit.open() {
                let (path, owner) = (visit.path, visit.owner);
                convert_to(
                    fd.as_raw_fd(),
                    &stat,
                    owner,
                    path,
                    owners,
                    group,
                    tree,
                    true,
                )?;
            }
            Ok(())
        })?;
    }
    let owner = owners.owner(&opened);
    convert_to(
        fd.as_raw_fd(),
        &opened,
        owner,
        "",
        owners,
        group,
        &mut tree,
        true,
    )
}

/// The outcome of one entry of a batch.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashResult {
    pub id: String,
    /// `moved`, `already` (an earlier batch moved it), `missing` (no
    /// payload) or `failed`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn move_trash(
    options: &NativeOptions,
    account: &Account,
    entries: &[LegacyTrashEntry],
) -> Result<Vec<TrashResult>, NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let (_, home) = account.open_home(options)?;
    let home_stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let root = account.journal_root(options, false)?.ok_or_else(|| {
        NativeError::forbidden("the HOME has not been migrated; run home_migrate_apply first")
    })?;
    let _lock = account.lock(&root)?;
    refuse_running(options, &account.identity)?;
    let journal = active_journal(&root, &owners)?
        .filter(|journal| journal.applied)
        .ok_or_else(|| {
            NativeError::forbidden("the HOME has not been migrated; run home_migrate_apply first")
        })?;
    if mode_of(&owners, &home_stat) != "oneHome" {
        return Err(NativeError::forbidden("the HOME is not in one-HOME mode"));
    }
    let batches = trash_batches(&journal.dir)?;
    if batches.len() >= MAX_TRASH_BATCHES {
        return Err(NativeError::forbidden("too many recycle bin batches"));
    }
    let done = moved_so_far(&journal.dir, &owners)?;
    let legacy = legacy_trash(options, account.identity.user_id)?;
    let home_path = account.home.display().to_string();

    // Plan every entry first: the batch journal is written before any
    // payload moves.
    struct Planned<'a> {
        entry: &'a LegacyTrashEntry,
        records: Vec<Record>,
        stat: libc::stat,
    }
    let mut results: BTreeMap<usize, TrashResult> = BTreeMap::new();
    let mut planned: Vec<(usize, Planned<'_>)> = Vec::new();
    let mut total = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        let result = |status, message: Option<String>| TrashResult {
            id: entry.id.clone(),
            status,
            name: None,
            message,
        };
        if let Some(moved) = done.get(&entry.id) {
            // An earlier batch planned it. It moved unless its payload is
            // still in the legacy recycle bin (that batch stopped before
            // the move): then it is planned again.
            let still_legacy = match (&legacy, cname(&entry.id)) {
                (Some(legacy), Ok(name)) => lstat_at(legacy.as_raw_fd(), &name).is_ok(),
                _ => false,
            };
            if !still_legacy {
                results.insert(
                    index,
                    TrashResult {
                        name: Some(moved.name.clone()),
                        ..result("already", None)
                    },
                );
                continue;
            }
        }
        let Some(legacy) = &legacy else {
            results.insert(index, result("missing", None));
            continue;
        };
        let name = cname(&entry.id)?;
        match lstat_at(legacy.as_raw_fd(), &name) {
            Err(error) if is_enoent(&error) => {
                results.insert(index, result("missing", None));
                continue;
            }
            Err(error) => {
                results.insert(index, result("failed", Some(error.to_string())));
                continue;
            }
            Ok(stat) if stat.st_dev != home_stat.st_dev => {
                results.insert(
                    index,
                    result(
                        "failed",
                        Some("on another file system than the HOME".into()),
                    ),
                );
                continue;
            }
            Ok(_) => {}
        }
        match payload_records(legacy, &name, &owners) {
            Ok(Some((records, stat))) => {
                total += records.len() as u64;
                if total > MAX_TREE_ENTRIES {
                    return Err(too_many());
                }
                planned.push((
                    index,
                    Planned {
                        entry,
                        records,
                        stat,
                    },
                ));
            }
            Ok(None) => {
                results.insert(index, result("failed", Some("it changed".into())));
            }
            Err(why) => {
                results.insert(index, result("failed", Some(why)));
            }
        }
    }
    if planned.is_empty() {
        return Ok(results.into_values().collect());
    }
    let legacy = legacy.expect("planned entries have a payload");

    // The trash folders, and a free name in each entry's `files`. A trash
    // folder that cannot be used (for example a `.Trash-<uid>` made first
    // by another member) fails the entries of that folder only.
    let mut created = Vec::new();
    let mut folders: HashMap<Option<String>, Result<(OwnedFd, OwnedFd), String>> = HashMap::new();
    let mut taken: HashMap<Option<String>, std::collections::HashSet<String>> = HashMap::new();
    let mut lines = vec![TrashLine {
        created: Vec::new(),
        moved: None,
    }];
    let mut moves = Vec::new();
    for (index, plan) in planned {
        let container = plan.entry.container.clone();
        let failed = |message: String| TrashResult {
            id: plan.entry.id.clone(),
            status: "failed",
            name: None,
            message: Some(message),
        };
        let pair = folders.entry(container.clone()).or_insert_with(|| {
            trash_base(options, &home, container.as_deref())
                .and_then(|base| ensure_trash(&base, container.as_deref(), &owners, &mut created))
                .map_err(|error| error.message)
        });
        let (files, info) = match pair {
            Ok((files, info)) => (files, info),
            Err(message) => {
                results.insert(index, failed(message.clone()));
                continue;
            }
        };
        let (_, base_name) = split(&plan.entry.path);
        let taken = taken.entry(container.clone()).or_default();
        let mut chosen = None;
        for attempt in 1..=MAX_TRASH_NAMES {
            let candidate = if attempt == 1 {
                base_name.to_string()
            } else {
                format!("{base_name}.{attempt}")
            };
            if rast_base::storage::safe_path::validate_component(&candidate).is_err() {
                break;
            }
            let info_name = rast_base::storage::xdg_trash::info_name(&candidate);
            // Nothing else writes there while the lock is held and the
            // account's processes are stopped; the info file is created
            // with `O_EXCL` before the move all the same.
            if taken.contains(&candidate)
                || lstat_at(files.as_raw_fd(), &cname(&candidate)?).is_ok()
                || lstat_at(info.as_raw_fd(), &cname(&info_name)?).is_ok()
            {
                continue;
            }
            taken.insert(candidate.clone());
            chosen = Some(candidate);
            break;
        }
        let Some(name) = chosen else {
            results.insert(
                index,
                TrashResult {
                    id: plan.entry.id.clone(),
                    status: "failed",
                    name: None,
                    message: Some("no free name in the recycle bin".into()),
                },
            );
            continue;
        };
        lines.push(TrashLine {
            created: Vec::new(),
            moved: Some(TrashMove {
                id: plan.entry.id.clone(),
                container: container.clone(),
                name: name.clone(),
                records: plan.records,
            }),
        });
        moves.push((index, plan.entry, container, name, plan.stat));
    }
    lines[0].created = created;
    if moves.is_empty() && lines[0].created.is_empty() {
        return Ok(results.into_values().collect());
    }
    // The batch journal is written before any info file or payload.
    let batch = format!("{TRASH_PREFIX}{:04}.jsonl", batches.len() + 1);
    write_synced(&journal.dir, &batch, &lines)?;

    for (index, entry, container, name, stat) in moves {
        let Some(Ok((files, info))) = folders.get(&container) else {
            continue;
        };
        let from = cname(&entry.id)?;
        let to = cname(&name)?;
        let origin = match &container {
            None => format!("{home_path}/{}", entry.path),
            Some(_) => entry.path.clone(),
        };
        let body = rast_base::storage::xdg_trash::format_info(&origin, entry.deleted_at);
        match write_info(info, &name, &body, &owners) {
            Ok(true) => {}
            Ok(false) => {
                results.insert(
                    index,
                    TrashResult {
                        id: entry.id.clone(),
                        status: "failed",
                        name: None,
                        message: Some("its name in the recycle bin was taken".into()),
                    },
                );
                continue;
            }
            Err(error) => {
                return Err(io_failed("write the trash info of", &name, error));
            }
        }
        let still = lstat_at(legacy.as_raw_fd(), &from).is_ok_and(|now| same_inode(&now, &stat));
        // An entry of a shared folder's bin keeps the folder's group.
        let group = match &container {
            Some(_) => fstat(files.as_raw_fd())
                .map(|stat| owners.owner(&stat).1)
                .unwrap_or(owners.gid),
            None => owners.gid,
        };
        let outcome = if still {
            rename_noreplace(legacy.as_raw_fd(), &from, files.as_raw_fd(), &to)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    convert_payload(files, &to, &owners, group).map_err(|error| error.message)
                })
        } else {
            Err("it changed".to_string())
        };
        let result = match outcome {
            Ok(()) => TrashResult {
                id: entry.id.clone(),
                status: "moved",
                name: Some(name),
                message: None,
            },
            Err(message) => {
                if lstat_at(files.as_raw_fd(), &to).is_err() {
                    // Nothing moved: give the name back.
                    let info_name = rast_base::storage::xdg_trash::info_name(&name);
                    let _ = unlink_at(info.as_raw_fd(), &cname(&info_name)?, false);
                }
                TrashResult {
                    id: entry.id.clone(),
                    status: "failed",
                    name: None,
                    message: Some(message),
                }
            }
        };
        results.insert(index, result);
    }
    Ok(results.into_values().collect())
}

pub(crate) async fn home_migrate_trash(
    options: &NativeOptions,
    op: HomeTrashOp,
) -> Result<Value, NativeError> {
    let account = Account::of(options, op.user_id, &op.username)?;
    if op.entries.is_empty() || op.entries.len() > MAX_TRASH_BATCH {
        return Err(NativeError::invalid(format!(
            "a batch holds 1 to {MAX_TRASH_BATCH} entries"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &op.entries {
        validate_trash_entry(entry)?;
        if !seen.insert(entry.id.as_str()) {
            return Err(NativeError::invalid(format!(
                "recycle bin entry {} is listed twice",
                entry.id
            )));
        }
    }
    account.passwd(options).await?;
    let options = options.clone();
    let acc = account.clone();
    let entries = op.entries.clone();
    let results = blocking(move || move_trash(&options, &acc, &entries)).await?;
    Ok(json!({
        "userId": op.user_id,
        "username": op.username,
        "results": results,
    }))
}

// ---------------------------------------------------------------------------
// home_migrate_rollback
// ---------------------------------------------------------------------------

/// What a rollback did.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeRollbackReport {
    /// Entries of the HOME given back their recorded owner, mode and ACLs.
    pub restored: u64,
    /// Entries made after the migration, given to the service.
    pub returned: Sample,
    /// Recorded entries that are another inode now.
    pub changed_since: Sample,
    /// Recorded entries that no longer exist.
    pub missing: Sample,
    pub steps: StepsReport,
    /// Recycle bin entries moved back to the legacy recycle bin.
    pub trash_restored: Vec<String>,
    /// Recycle bin entries that are gone (restored or deleted since).
    pub trash_gone: Vec<String>,
    pub tree: HomeTree,
}

fn undo_trash(
    options: &NativeOptions,
    account: &Account,
    home: &OwnedFd,
    dir: &OwnedFd,
    owners: &Owners,
    report: &mut HomeRollbackReport,
) -> Result<(), NativeError> {
    let legacy = legacy_trash(options, account.identity.user_id)?;
    for batch in trash_batches(dir)?.into_iter().rev() {
        let lines: Vec<TrashLine> = read_lines(dir, &batch, "recycle bin journal")?;
        for (index, line) in lines.iter().enumerate() {
            validate_trash_line(line, owners).map_err(|why| {
                NativeError::forbidden(format!("line {} of {batch} is refused: {why}", index + 1))
            })?;
        }
        for line in lines.iter().rev() {
            if let Some(moved) = &line.moved {
                let gone =
                    |report: &mut HomeRollbackReport| report.trash_gone.push(moved.id.clone());
                let Ok(base) = trash_base(options, home, moved.container.as_deref()) else {
                    gone(report);
                    continue;
                };
                let rel = trash_rel(moved.container.as_deref(), owners.uid);
                let Ok(trash) = open_rel(&base, &rel) else {
                    gone(report);
                    continue;
                };
                let (Ok(files), Some(legacy)) = (open_rel(&trash, "files"), legacy.as_ref()) else {
                    gone(report);
                    continue;
                };
                let name = cname(&moved.name)?;
                let first = &moved.records[0];
                let here = lstat_at(files.as_raw_fd(), &name).ok().filter(|stat| {
                    dev_of(stat) == first.dev
                        && ino_of(stat) == first.ino
                        && is_dir(stat) == first.d
                });
                if here.is_none() {
                    let id = cname(&moved.id)?;
                    if lstat_at(legacy.as_raw_fd(), &id).is_ok() {
                        // Never moved (the batch stopped after its journal):
                        // only its info file, if made, is the migration's.
                        remove_info(&trash, &moved.name)?;
                    } else {
                        gone(report);
                    }
                    continue;
                }
                restore_payload(&files, &name, &moved.records, owners)?;
                match rename_noreplace(
                    files.as_raw_fd(),
                    &name,
                    legacy.as_raw_fd(),
                    &cname(&moved.id)?,
                ) {
                    Ok(()) => {}
                    Err(error) => return Err(io_failed("move back", &moved.name, error)),
                }
                remove_info(&trash, &moved.name)?;
                report.trash_restored.push(moved.id.clone());
            }
            // The folders the batch made, when they are empty again.
            for created in line.created.iter().rev() {
                let Ok(base) = trash_base(options, home, created.container.as_deref()) else {
                    continue;
                };
                let (parent, name) = split(&created.path);
                if let Ok(parent) = open_rel(&base, parent) {
                    let _ = unlink_at(parent.as_raw_fd(), &cname(name)?, true);
                }
            }
        }
    }
    Ok(())
}

/// Remove the info file of `name` in the trash folder `trash` (a regular
/// file only).
fn remove_info(trash: &OwnedFd, name: &str) -> Result<(), NativeError> {
    if let Ok(info) = open_rel(trash, "info") {
        let info_name = rast_base::storage::xdg_trash::info_name(name);
        let c = cname(&info_name)?;
        if lstat_at(info.as_raw_fd(), &c).is_ok_and(|stat| is_regular(&stat)) {
            let _ = unlink_at(info.as_raw_fd(), &c, false);
        }
    }
    Ok(())
}

/// Give a payload back its recorded owners, modes and ACLs.
fn restore_payload(
    files: &OwnedFd,
    name: &CStr,
    records: &[Record],
    owners: &Owners,
) -> Result<(), NativeError> {
    let first = &records[0];
    let flags = if first.d { DIR_FLAGS } else { FILE_FLAGS };
    let fd = open_at(files.as_raw_fd(), name, flags)
        .map_err(|error| NativeError::command_failed(format!("cannot open an entry: {error}")))?;
    let stat = fstat(fd.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect an entry: {error}"))
    })?;
    if first.d {
        let by_path: HashMap<&str, &Record> = records[1..]
            .iter()
            .map(|record| (record.p.as_str(), record))
            .collect();
        let mut tree = HomeTree::default();
        walk_tree(&fd, owners, &mut tree, |visit, _| {
            let Some((entry, stat)) = visit.open() else {
                return Ok(());
            };
            match by_path.get(visit.path) {
                Some(record)
                    if dev_of(&stat) == record.dev
                        && ino_of(&stat) == record.ino
                        && is_dir(&stat) == record.d =>
                {
                    restore(entry.as_raw_fd(), &stat, record, owners)
                        .map_err(|error| io_failed("restore", visit.path, error))?;
                }
                _ if visit.owner.0 == owners.uid => owners
                    .chown(entry.as_raw_fd(), &stat, owners.rast, first.gid)
                    .map_err(|error| io_failed("chown", visit.path, error))?,
                _ => {}
            }
            Ok(())
        })?;
    }
    restore(fd.as_raw_fd(), &stat, first, owners)
        .map_err(|error| io_failed("restore", "a recycle bin entry", error))
}

struct RollingBack {
    _lock: MigrationLock,
    journal: String,
    home_record: Record,
    report: HomeRollbackReport,
}

fn rollback_tree(options: &NativeOptions, account: &Account) -> Result<RollingBack, NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let no_journal = || NativeError::not_found("this account has no HOME migration to roll back");
    let root = account
        .journal_root(options, false)?
        .ok_or_else(no_journal)?;
    let lock = account.lock(&root)?;
    refuse_running(options, &account.identity)?;
    let journal = active_journal(&root, &owners)?.ok_or_else(no_journal)?;
    let (_, home) = account.open_home(options)?;
    let home_stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    let home_record = journal.records[0].clone();
    if home_record.dev != dev_of(&home_stat) || home_record.ino != ino_of(&home_stat) {
        return Err(NativeError::forbidden(
            "the HOME is not the folder the journal recorded",
        ));
    }
    let mut report = HomeRollbackReport::default();
    // The recycle bin first, then the merge, then the HOME's own entries.
    undo_trash(options, account, &home, &journal.dir, &owners, &mut report)?;
    let mut ignored = MergeReport::default();
    let sources = open_sources(options, account, &owners, &mut ignored)?;
    report.steps = undo_steps(&home, &sources, &journal.steps)?;
    let mut by_path: HashMap<&str, &Record> = journal.records[1..]
        .iter()
        .map(|record| (record.p.as_str(), record))
        .collect();
    let mut tree = HomeTree::default();
    let mut restored = 0u64;
    let mut returned = Sample::default();
    let mut changed = Sample::default();
    walk_tree(&home, &owners, &mut tree, |visit, _| {
        let Some((fd, stat)) = visit.open() else {
            return Ok(());
        };
        match by_path.remove(visit.path) {
            Some(record)
                if dev_of(&stat) == record.dev
                    && ino_of(&stat) == record.ino
                    && is_dir(&stat) == record.d =>
            {
                restore(fd.as_raw_fd(), &stat, record, &owners)
                    .map_err(|error| io_failed("restore", visit.path, error))?;
                restored += 1;
            }
            found => {
                if found.is_some() {
                    changed.add(visit.path);
                }
                // Made after the migration: the service serves it again.
                if visit.owner.0 == owners.uid {
                    owners
                        .chown(fd.as_raw_fd(), &stat, owners.rast, home_record.gid)
                        .map_err(|error| io_failed("chown", visit.path, error))?;
                    returned.add(visit.path);
                }
            }
        }
        Ok(())
    })?;
    report.restored = restored;
    report.returned = returned;
    report.changed_since = changed;
    for path in by_path.keys() {
        report.missing.add(path);
    }
    report.tree = tree;
    Ok(RollingBack {
        _lock: lock,
        journal: journal.stamp,
        home_record,
        report,
    })
}

/// Restore the HOME itself and mark the journal as rolled back.
fn finish_rollback(
    options: &NativeOptions,
    account: &Account,
    stamp: &str,
    record: &Record,
) -> Result<(), NativeError> {
    let owners = Owners::of(options, &account.identity)?;
    let (_, home) = account.open_home(options)?;
    let stat = fstat(home.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the HOME: {error}"))
    })?;
    if record.dev != dev_of(&stat) || record.ino != ino_of(&stat) {
        return Err(NativeError::forbidden(
            "the HOME is not the folder the journal recorded",
        ));
    }
    restore(home.as_raw_fd(), &stat, record, &owners)
        .map_err(|error| io_failed("restore", "the HOME", error))?;
    let root = account
        .journal_root(options, false)?
        .ok_or_else(|| NativeError::forbidden("the journal of the migration is gone"))?;
    let from = cname(stamp)?;
    let to = cname(&format!("{stamp}{ROLLED_BACK_SUFFIX}"))?;
    rename_noreplace(root.as_raw_fd(), &from, root.as_raw_fd(), &to).map_err(|error| {
        NativeError::command_failed(format!(
            "the HOME was restored, but its journal could not be marked as rolled back: {error}"
        ))
    })
}

pub(crate) async fn home_migrate_rollback(
    options: &NativeOptions,
    op: HomeMigrateOp,
) -> Result<Value, NativeError> {
    let account = Account::of(options, op.user_id, &op.username)?;
    let passwd = account.passwd(options).await?;
    let (opts, acc) = (options.clone(), account.clone());
    if blocking(move || state(&opts, &acc)).await?.1.is_none() {
        return Err(NativeError::not_found(
            "this account has no HOME migration to roll back",
        ));
    }
    let stopped = stop_user_units(options, &account.identity).await?;
    let (opts, acc) = (options.clone(), account.clone());
    let rolling = blocking(move || rollback_tree(&opts, &acc)).await?;
    if passwd.home != account.identity.home {
        set_passwd_home(options, &account.identity, &account.identity.home).await?;
    }
    let (opts, acc, stamp, record) = (
        options.clone(),
        account.clone(),
        rolling.journal.clone(),
        rolling.home_record.clone(),
    );
    blocking(move || finish_rollback(&opts, &acc, &stamp, &record)).await?;
    tracing::info!(
        user_id = op.user_id,
        journal = %rolling.journal,
        restored = rolling.report.restored,
        "HOME migration rolled back"
    );
    Ok(json!({
        "userId": op.user_id,
        "username": op.username,
        "journal": rolling.journal,
        "stopped": stopped,
        "report": rolling.report,
    }))
}

#[cfg(test)]
mod tests;
