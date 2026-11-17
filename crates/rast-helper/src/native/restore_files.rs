//! `rast restore-files` (review F1, decision A6): root restores the `users`
//! part of a Backup Center archive.
//!
//! The archive, its manifest and its checksums are written by the `rast`
//! service and kept where the service can change them, so root treats every
//! byte of it as untrusted (code running as `rast` must not become root):
//!
//! * The archive is read with the strict codec
//!   ([`archive::read_backup_entry`]): only regular files, folders, symbolic
//!   links and hard links, relative normalised paths, no device, FIFO or
//!   socket, no unknown pax record. Anything else refuses the run.
//! * A folder keeps numeric owners only in the two layouts that need them,
//!   and only after they are checked against the database and the host's
//!   account files ([`HostAccounts`]): a HOME in one-HOME mode, whose root
//!   belongs to its account's identity `rast-u<id>` (uid
//!   `linux_uid_base + id`) and whose entries belong to that identity or to
//!   the service; and a migrated shared folder, whose root belongs to root
//!   and to the share's `rast-s<h>-rw` group and whose entries belong to
//!   the service or to a RAST identity. The manifest's `preservedTrees`
//!   must name exactly those folders; it never decides. Every other folder
//!   was read by the service (files only) and is restored as `rast:rast`.
//! * Set-uid and set-gid bits never land on a file (cleared and counted);
//!   set-gid stays only on folders of a migrated shared folder. A group
//!   outside the layout becomes the owner's own group, and named ACL
//!   entries for accounts outside the layout are dropped (both counted).
//! * Everything is written below a new folder that root creates (mode 0700,
//!   [`Target`]), through descriptors: one name at a time with
//!   `O_NOFOLLOW`, files with `O_EXCL`, so no entry passes through a link
//!   or leaves the target and nothing is overwritten. A hard link may only
//!   point to a file of the same folder written before. Folders get their
//!   owners and modes last, deepest first, so nobody else can enter one
//!   while it is written.
//! * One refused entry refuses the run: whatever was written is removed and
//!   the report lists the reasons. A dry run writes nothing at all.
//!
//! With [`Target::Storage`] the folders are written to a staging folder in
//! `storage_dir`, then renamed into `homes/<name>` and `shared/<container>`
//! (`RENAME_NOREPLACE`): a folder that exists there is never replaced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::native::acl::{self, Acl, Entry, Tag};
use crate::native::backup_reader::archive::{self, ArchiveError, EntryMeta, Kind, Next};
use crate::native::backup_reader::{TreeKind, MAX_DEPTH};
use crate::native::homes::{
    dir_is_empty, fchmod, fchown, fstat, is_dir, mkdir_at, open_created_dir, DIR_FLAGS,
};
use crate::native::protocol::{validate_home_username, validate_share_container, MAX_USER_ID};
use crate::native::shares::{
    dev_of, dir_names, identity_uid_range, ino_of, is_share_group_name, lstat_at, open_at,
    same_inode, share_gid_range,
};

/// Refusals listed in a report at most (the rest is counted).
pub const MAX_REFUSALS: usize = 50;
/// Longest refusal text, in characters.
const MAX_REFUSAL_CHARS: usize = 300;
/// Largest `manifest.json` read.
const MAX_MANIFEST_BYTES: u64 = 256 << 20;
/// Path bytes remembered for one run at most (parents and hard links are
/// checked against them).
const MAX_KNOWN_BYTES: usize = 256 << 20;
/// Free space a restore leaves on the target's file system.
pub const FREE_SPACE_RESERVE: u64 = 512 << 20;
/// The longest symbolic link target Linux stores.
const MAX_SYMLINK_TARGET: usize = 4095;
const COPY_BUFFER: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// The host's account files (`/etc/passwd`, `/etc/group`): what root can
/// trust about who a uid or gid is. RAST identities are local accounts.
#[derive(Debug, Clone, Default)]
pub struct HostAccounts {
    users: HashMap<String, (u32, u32)>,
    user_names: HashMap<u32, String>,
    groups: HashMap<String, u32>,
    group_names: HashMap<u32, String>,
}

impl HostAccounts {
    /// Parse the text of a passwd and a group file (malformed lines are
    /// skipped; the first entry of a name or an id wins, as for NSS).
    pub fn parse(passwd: &str, group: &str) -> Self {
        let mut out = Self::default();
        for line in passwd.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 4 || fields[0].is_empty() || fields[0].starts_with('#') {
                continue;
            }
            let (Ok(uid), Ok(gid)) = (fields[2].parse::<u32>(), fields[3].parse::<u32>()) else {
                continue;
            };
            out.users.entry(fields[0].to_string()).or_insert((uid, gid));
            out.user_names
                .entry(uid)
                .or_insert_with(|| fields[0].to_string());
        }
        for line in group.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 3 || fields[0].is_empty() || fields[0].starts_with('#') {
                continue;
            }
            let Ok(gid) = fields[2].parse::<u32>() else {
                continue;
            };
            out.groups.entry(fields[0].to_string()).or_insert(gid);
            out.group_names
                .entry(gid)
                .or_insert_with(|| fields[0].to_string());
        }
        out
    }

    /// Read `<etc>/passwd` and `<etc>/group`.
    pub fn read(etc: &Path) -> io::Result<Self> {
        Ok(Self::parse(
            &std::fs::read_to_string(etc.join("passwd"))?,
            &std::fs::read_to_string(etc.join("group"))?,
        ))
    }

    /// The uid and gid of the account `name`.
    pub fn user(&self, name: &str) -> Option<(u32, u32)> {
        self.users.get(name).copied()
    }
}

/// What the database says (untrusted too: the service writes it).
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// User id by username.
    pub accounts: HashMap<String, i64>,
    /// Share id by container.
    pub shares: HashMap<String, String>,
}

/// Where a restore writes.
#[derive(Debug, Clone)]
pub enum Target {
    /// Check every entry, write nothing.
    DryRun,
    /// A new folder (it must not exist; its parent must), root-only, that
    /// gets `homes/<name>` and `shared/<container>`.
    Scratch(PathBuf),
    /// The live storage: a staging folder `<storage>/<staging>`, then each
    /// folder renamed into `<storage>/homes` or `<storage>/shared`, never
    /// over an existing one.
    Storage { storage: PathBuf, staging: String },
}

/// The owners each new entry gets by `(device, inode)`, when owners are
/// recorded instead of set.
pub type OwnerLog = Arc<Mutex<HashMap<(u64, u64), (u32, u32)>>>;

/// How owners are given.
#[derive(Debug, Clone)]
pub enum Owners {
    /// `fchown` (root).
    Apply,
    /// Recorded only: tests, which run unprivileged.
    Record(OwnerLog),
}

impl Owners {
    fn chown_fd(&self, fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
        match self {
            Self::Apply => fchown(fd, uid, gid),
            Self::Record(log) => {
                let stat = fstat(fd)?;
                log.lock()
                    .unwrap()
                    .insert((dev_of(&stat), ino_of(&stat)), (uid, gid));
                Ok(())
            }
        }
    }

    fn chown_link(&self, dir: RawFd, name: &CStr, uid: u32, gid: u32) -> io::Result<()> {
        match self {
            Self::Apply => {
                // SAFETY: valid fd and NUL-terminated name.
                let rc = unsafe {
                    libc::fchownat(dir, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW)
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            Self::Record(log) => {
                let stat = lstat_at(dir, name)?;
                log.lock()
                    .unwrap()
                    .insert((dev_of(&stat), ino_of(&stat)), (uid, gid));
                Ok(())
            }
        }
    }
}

/// Options of one restore.
#[derive(Debug, Clone)]
pub struct Options {
    /// `linux_uid_base`.
    pub uid_base: u32,
    /// The `rast` service account.
    pub service_uid: u32,
    pub service_gid: u32,
    /// Only these folders (`users/homes/<name>`, `users/shared/<container>`);
    /// empty: every folder of the archive.
    pub trees: Vec<String>,
    pub target: Target,
    pub owners: Owners,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// One folder of the archive.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreeReport {
    /// `users/homes/<name>` or `users/shared/<container>`.
    pub tree: String,
    /// `one-home`, `group` (a migrated shared folder), `legacy` (read by
    /// the service) or `unknown` (its folder entry was refused).
    pub layout: String,
    /// Who owns the folder after the restore.
    pub owner: String,
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    /// `checked` (dry run), `restored`, `exists` (a live folder of that
    /// name exists: not written), `kept` (written, but the live folder
    /// appeared meanwhile: left in the staging folder), `refused` or
    /// `not written` (another entry refused the run).
    pub status: String,
    /// Where it is now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// What a restore did.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub ok: bool,
    /// `dry-run`, `scratch` or `storage`.
    pub mode: String,
    /// The folder that holds the result (scratch), or the staging folder
    /// when something was left in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub trees: Vec<TreeReport>,
    /// The first [`MAX_REFUSALS`] refusals, `<path>: <reason>`.
    pub refused: Vec<String>,
    pub refused_count: u64,
    /// Files and folders whose set-uid or set-gid bit was cleared.
    pub set_id_cleared: u64,
    /// Entries whose group was outside the layout and became the owner's.
    pub groups_changed: u64,
    /// Entries of a shared folder owned by the identity of an account that
    /// no longer exists, given to the service.
    pub owners_changed: u64,
    /// Named ACL entries for accounts outside the layout, dropped.
    pub acl_entries_dropped: u64,
    /// Folders listed under `preservedTrees` that have no entry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The report as text lines for the terminal.
pub fn describe(report: &Report) -> Vec<String> {
    let mut lines = Vec::new();
    for tree in &report.trees {
        let mut line = format!(
            "{}: {} ({} layout, owner {}; {} entries, {} files, {} bytes)",
            tree.tree, tree.status, tree.layout, tree.owner, tree.entries, tree.files, tree.bytes
        );
        if let Some(path) = &tree.path {
            line.push_str(&format!(" -> {path}"));
        }
        match tree.status.as_str() {
            "exists" => line.push_str(
                "; the live folder exists and was not replaced: move it aside and run again",
            ),
            "kept" => line.push_str(
                "; a live folder of that name appeared meanwhile, so it stays in the staging folder",
            ),
            _ => {}
        }
        lines.push(line);
    }
    if report.trees.is_empty() {
        lines.push("The archive has no user files to restore.".into());
    }
    for (count, what) in [
        (report.set_id_cleared, "set-uid or set-gid bits cleared"),
        (report.groups_changed, "groups outside the layout replaced"),
        (
            report.owners_changed,
            "entries of removed accounts given to the service",
        ),
        (
            report.acl_entries_dropped,
            "ACL entries for other accounts dropped",
        ),
    ] {
        if count > 0 {
            lines.push(format!("{count} {what}"));
        }
    }
    lines.extend(report.notes.iter().map(|note| format!("note: {note}")));
    if report.ok {
        lines.push(match (report.mode.as_str(), &report.target) {
            ("dry-run", _) => "dry run: every entry passed the checks; nothing was written".into(),
            ("scratch", Some(target)) => format!(
                "restored into {target} (root only); compare it with the live folders, then \
                 restore with --to-storage"
            ),
            (_, Some(target)) => {
                format!("restored; some folders were kept in the staging folder {target}")
            }
            _ => "restored".into(),
        });
    } else {
        lines.push(format!(
            "refused: {} entries did not pass the checks; nothing was written",
            report.refused_count
        ));
        lines.extend(report.refused.iter().map(|reason| format!("  {reason}")));
        if report.refused_count as usize > report.refused.len() {
            lines.push(format!(
                "  and {} more",
                report.refused_count as usize - report.refused.len()
            ));
        }
    }
    lines
}

/// Check a `--tree` value: `users/homes/<name>` or `users/shared/<container>`.
pub fn parse_tree(text: &str) -> Result<String, String> {
    let invalid = || {
        format!("{text:?} is not a folder of an archive (users/homes/<name> or users/shared/<container>)")
    };
    let rest = text.strip_prefix("users/").ok_or_else(invalid)?;
    let (kind, name) = rest.split_once('/').ok_or_else(invalid)?;
    match kind {
        "homes" => validate_home_username(name),
        "shared" => validate_share_container(name),
        _ => return Err(invalid()),
    }
    .map_err(|_| invalid())?;
    Ok(text.to_string())
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// What [`Policy::owner`] replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Changed {
    No,
    /// A group outside the layout became the owner's own.
    Group,
    /// The identity of an account that no longer exists became the
    /// service.
    Owner,
}

/// How one folder of the archive is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Read by the service: files only, restored as the service's.
    Legacy,
    /// A HOME in one-HOME mode of the identity `rast-u<id>` (`uid`).
    OneHome { uid: u32, id: i64 },
    /// A migrated shared folder with the read-write group `gid`.
    Group { gid: u32 },
    /// The folder entry itself was refused: the rest is only counted.
    Unknown,
}

struct Policy<'a> {
    options: &'a Options,
    facts: &'a Facts,
    host: &'a HostAccounts,
    ids: HashSet<i64>,
    identities: RangeInclusive<u32>,
    share_gids: RangeInclusive<u32>,
}

impl<'a> Policy<'a> {
    fn new(options: &'a Options, facts: &'a Facts, host: &'a HostAccounts) -> Result<Self, String> {
        Ok(Self {
            options,
            facts,
            host,
            ids: facts.accounts.values().copied().collect(),
            identities: identity_uid_range(options.uid_base),
            share_gids: share_gid_range(options.uid_base).map_err(|error| error.message)?,
        })
    }

    /// Why the identity of the user id `id` cannot have the uid and gid
    /// `uid` on this host (`None`: it can). The host's account files are
    /// the authority: the uid and the name must not belong to anyone else.
    fn identity_problem(&self, id: i64, uid: u32) -> Option<String> {
        let name = format!("rast-u{id}");
        if let Some((have_uid, have_gid)) = self.host.users.get(&name) {
            if (*have_uid, *have_gid) != (uid, uid) {
                return Some(format!(
                    "the host account {name} has uid/gid {have_uid}/{have_gid}, RAST expects {uid}"
                ));
            }
        }
        if let Some(other) = self
            .host
            .user_names
            .get(&uid)
            .filter(|other| **other != name)
        {
            return Some(format!("uid {uid} belongs to the host account {other}"));
        }
        if let Some(gid) = self.host.groups.get(&name).filter(|gid| **gid != uid) {
            return Some(format!(
                "the host group {name} has gid {gid}, RAST expects {uid}"
            ));
        }
        if let Some(other) = self
            .host
            .group_names
            .get(&uid)
            .filter(|other| **other != name)
        {
            return Some(format!("gid {uid} belongs to the host group {other}"));
        }
        None
    }

    /// The identity uid of the account `name`, checked.
    fn home_identity(&self, name: &str) -> Result<(u32, i64), String> {
        let id = *self
            .facts
            .accounts
            .get(name)
            .ok_or_else(|| "no RAST account has this name in the database".to_string())?;
        if !(1..=MAX_USER_ID).contains(&id) {
            return Err(format!("the account's id {id} is out of range"));
        }
        let uid = u32::try_from(id)
            .ok()
            .and_then(|id| self.options.uid_base.checked_add(id))
            .filter(|uid| self.identities.contains(uid))
            .ok_or_else(|| format!("the account's id {id} gives no identity uid"))?;
        match self.identity_problem(id, uid) {
            Some(problem) => Err(problem),
            None => Ok((uid, id)),
        }
    }

    /// Whether `uid` (or a gid of the same number) is the identity of an
    /// account in the database that nobody else holds on this host.
    fn is_identity(&self, uid: u32) -> bool {
        if !self.identities.contains(&uid) {
            return false;
        }
        let id = i64::from(uid - self.options.uid_base);
        self.ids.contains(&id) && self.identity_problem(id, uid).is_none()
    }

    /// Whether `gid` may be a shared folder group: in the range, and on
    /// this host either free or a share group.
    fn is_share_gid(&self, gid: u32) -> bool {
        self.share_gids.contains(&gid)
            && self
                .host
                .group_names
                .get(&gid)
                .is_none_or(|name| is_share_group_name(name))
    }

    /// Why `gid` cannot be the read-write group of the shared folder with
    /// the container `container`.
    fn container_problem(&self, container: &str, gid: u32) -> Option<String> {
        let Some(share_id) = self.facts.shares.get(container) else {
            return Some("no shared folder has this container in the database".into());
        };
        let Some(rw) = rast_base::storage::shares::rw_group_name(share_id) else {
            return Some("the shared folder's id is invalid".into());
        };
        if !self.share_gids.contains(&gid) {
            return Some(format!(
                "its group {gid} is not a shared folder group (the group layout needs {rw})"
            ));
        }
        if let Some(have) = self.host.groups.get(&rw).filter(|have| **have != gid) {
            return Some(format!(
                "its group is {gid}, but the host group {rw} has gid {have}"
            ));
        }
        if let Some(other) = self
            .host
            .group_names
            .get(&gid)
            .filter(|other| **other != rw)
        {
            return Some(format!(
                "gid {gid} belongs to the host group {other}, not {rw}"
            ));
        }
        None
    }

    /// The owner and group an entry below a folder's root gets, or why it
    /// is refused, and what was replaced.
    fn owner(&self, layout: Layout, uid: u32, gid: u32) -> Result<(u32, u32, Changed), String> {
        let service = (self.options.service_uid, self.options.service_gid);
        let (allowed_gid, primary) = match layout {
            Layout::OneHome { uid: me, .. } => {
                if uid != me && uid != service.0 {
                    return Err(format!(
                        "owned by uid {uid}; a HOME in one-HOME mode holds only entries of its \
                         account (uid {me}) and of the service"
                    ));
                }
                (
                    gid == me || gid == service.1 || self.is_share_gid(gid),
                    if uid == me { me } else { service.1 },
                )
            }
            Layout::Group { .. } => {
                if uid == 0 {
                    return Err("owned by root below the top of a shared folder".into());
                }
                if uid != service.0 && !self.is_identity(uid) {
                    // The identity of an account that is gone (its files
                    // stay in shared folders): the service's, never a uid
                    // that a later account or another user could get.
                    if self.identities.contains(&uid)
                        && !self.host.user_names.contains_key(&uid)
                        && !self.ids.contains(&i64::from(uid - self.options.uid_base))
                    {
                        return Ok((service.0, service.1, Changed::Owner));
                    }
                    return Err(format!(
                        "owned by uid {uid}, which is neither the service nor the identity of \
                         a RAST account"
                    ));
                }
                (
                    gid == service.1 || self.is_identity(gid) || self.is_share_gid(gid),
                    if uid == service.0 { service.1 } else { uid },
                )
            }
            Layout::Legacy | Layout::Unknown => return Ok((service.0, service.1, Changed::No)),
        };
        Ok(if allowed_gid {
            (uid, gid, Changed::No)
        } else {
            (uid, primary, Changed::Group)
        })
    }

    /// `acl` without named entries for accounts outside `layout` (and how
    /// many were dropped), or why it is invalid.
    fn acl(&self, layout: Layout, acl: Option<Acl>) -> Result<(Option<Acl>, u64), String> {
        let Some(acl) = acl else {
            return Ok((None, 0));
        };
        let service = (self.options.service_uid, self.options.service_gid);
        let group_layout = matches!(layout, Layout::Group { .. });
        let keep = |tag: Tag| match tag {
            Tag::User(uid) => self.is_identity(uid) || (group_layout && uid == service.0),
            Tag::Group(gid) => {
                self.is_identity(gid)
                    || self.is_share_gid(gid)
                    || (group_layout && gid == service.1)
            }
            _ => true,
        };
        let before = acl.0.len();
        let entries: Vec<Entry> = acl.0.into_iter().filter(|entry| keep(entry.tag)).collect();
        let dropped = (before - entries.len()) as u64;
        let has = |tag: Tag| entries.iter().any(|entry| entry.tag == tag);
        let named = entries
            .iter()
            .any(|entry| matches!(entry.tag, Tag::User(_) | Tag::Group(_)));
        if !has(Tag::UserObj)
            || !has(Tag::GroupObj)
            || !has(Tag::Other)
            || (named && !has(Tag::Mask))
        {
            return Err("an invalid ACL".into());
        }
        Ok((Some(Acl::new(entries)), dropped))
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Dir,
    File,
    Other,
}

/// Owners, mode and ACLs a folder gets at the end.
struct Pending {
    relative: Vec<u8>,
    uid: u32,
    gid: u32,
    mode: u32,
    acl_access: Option<Acl>,
    acl_default: Option<Acl>,
    mtime: Option<u64>,
}

struct Tree {
    kind: TreeKind,
    name: String,
    layout: Option<Layout>,
    known: HashMap<Vec<u8>, Known>,
    entries: u64,
    files: u64,
    bytes: u64,
    /// The live folder exists (storage): checked, never written.
    exists: bool,
    refused: bool,
    /// The folder in the output, once created.
    root: Option<OwnedFd>,
    /// The last parent folder opened, by relative path.
    cache: Option<(Vec<u8>, OwnedFd)>,
    dirs: Vec<Pending>,
    status: Option<String>,
    path: Option<String>,
}

/// The new folder the run writes into.
struct Output {
    parent: OwnedFd,
    name: CString,
    root: OwnedFd,
    root_stat: libc::stat,
    homes: OwnedFd,
    shared: OwnedFd,
    shown: PathBuf,
    /// `<storage>/homes` and `<storage>/shared` (storage mode).
    live: Option<(OwnedFd, OwnedFd, PathBuf)>,
}

/// What an entry turns into.
enum Plan {
    Dir {
        uid: u32,
        gid: u32,
        mode: u32,
        acl_access: Option<Acl>,
        acl_default: Option<Acl>,
    },
    File {
        uid: u32,
        gid: u32,
        mode: u32,
        acl: Option<Acl>,
        /// Folders the service did not store, made for this file first
        /// (outermost first; a folder read by the service only).
        new_dirs: Vec<Vec<u8>>,
    },
    Symlink {
        uid: u32,
        gid: u32,
        target: Vec<u8>,
    },
    HardLink {
        target: Vec<u8>,
    },
}

struct Run<'a> {
    policy: &'a Policy<'a>,
    options: &'a Options,
    selection: Option<HashSet<String>>,
    out: Option<Output>,
    trees: BTreeMap<String, Tree>,
    refused: Vec<String>,
    refused_count: u64,
    manifest: Option<Vec<String>>,
    manifest_seen: bool,
    manifest_notes: Vec<String>,
    known_bytes: usize,
    set_id_cleared: u64,
    groups_changed: u64,
    owners_changed: u64,
    acl_entries_dropped: u64,
    buffer: Vec<u8>,
}

fn shown(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn cname(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(io::Error::from)
}

fn unlink_at(dir: RawFd, name: &CStr, folder: bool) -> io::Result<()> {
    let flags = if folder { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn times(mtime: u64) -> [libc::timespec; 2] {
    // SAFETY: zeroed timespecs are valid; the fields are set below.
    let mut times: [libc::timespec; 2] = unsafe { std::mem::zeroed() };
    times[0].tv_nsec = libc::UTIME_OMIT;
    times[1].tv_sec = i64::try_from(mtime).unwrap_or(i64::MAX) as libc::time_t;
    times
}

fn set_mtime(fd: RawFd, mtime: u64) -> io::Result<()> {
    let times = times(mtime);
    // SAFETY: valid fd and two timespecs.
    if unsafe { libc::futimens(fd, times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_link_mtime(dir: RawFd, name: &CStr, mtime: u64) -> io::Result<()> {
    let times = times(mtime);
    // SAFETY: valid fd, NUL-terminated name and two timespecs.
    let rc = unsafe {
        libc::utimensat(
            dir,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Bytes available to unprivileged users on the file system of `fd`.
fn available(fd: RawFd) -> Option<u64> {
    // SAFETY: a zeroed `statvfs` is a valid out parameter.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out parameter.
    if unsafe { libc::fstatvfs(fd, &mut stats) } != 0 {
        return None;
    }
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

/// Open the folder `rel` below `root`, one name at a time, never following
/// a link (`""`: `root` itself).
fn open_rel(root: &OwnedFd, rel: &[u8]) -> io::Result<OwnedFd> {
    let mut dir = open_at(root.as_raw_fd(), c".", DIR_FLAGS)?;
    if rel.is_empty() {
        return Ok(dir);
    }
    for part in rel.split(|byte| *byte == b'/') {
        dir = open_at(dir.as_raw_fd(), &cname(part)?, DIR_FLAGS)?;
    }
    Ok(dir)
}

/// Open an absolute folder path one name at a time from `/`, refusing a
/// symbolic link anywhere in it (the service owns `storage_dir` and could
/// put one there).
pub fn open_dir_no_links(path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", path.display()),
        ));
    }
    let mut dir = open_at(libc::AT_FDCWD, c"/", DIR_FLAGS)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                dir = open_at(dir.as_raw_fd(), &cname(name.as_encoded_bytes())?, DIR_FLAGS)
                    .map_err(|error| match error.raw_os_error() {
                        Some(libc::ELOOP) | Some(libc::ENOTDIR) => io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{} contains a symbolic link or a file; give the real folder",
                                path.display()
                            ),
                        ),
                        _ => error,
                    })?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a normalised path", path.display()),
                ))
            }
        }
    }
    Ok(dir)
}

/// Open the archive for reading: a regular file, never through a link at
/// its last name, never a FIFO or device that would block or never end.
pub fn open_archive(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)?;
    let stat = fstat(file.as_raw_fd())?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    // SAFETY: valid fd; clears O_NONBLOCK on the open file description.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(file)
}

/// The instance lock of the RAST service (`<data_dir>/rast.lock`), as a
/// restore into the live storage sees it.
#[derive(Debug)]
pub enum ServiceLock {
    /// No lock file: the service never ran with this data folder.
    Absent,
    /// Held by this process until it is dropped: the service cannot start
    /// meanwhile.
    Held(std::fs::File),
    /// The service (or another restore) holds it.
    Busy,
}

/// Take the service's instance lock for the length of a restore. The file
/// is only opened, never created: a lock file that root created would be
/// root's, and the service could not open it any more.
pub fn take_service_lock(path: &Path) -> io::Result<ServiceLock> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ServiceLock::Absent),
        Err(error) => return Err(error),
    };
    if fstat(file.as_raw_fd())?.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    match file.try_lock() {
        Ok(()) => Ok(ServiceLock::Held(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(ServiceLock::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Remove everything below `dir` (a folder this run created).
fn remove_contents(dir: &OwnedFd, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH + 4 {
        return Err(io::Error::other("nested too deeply"));
    }
    let names = dir_names(dir.as_raw_fd(), u64::MAX)?.unwrap_or_default();
    for name in names {
        let stat = lstat_at(dir.as_raw_fd(), &name)?;
        if is_dir(&stat) {
            let sub = open_at(dir.as_raw_fd(), &name, DIR_FLAGS)?;
            if !same_inode(&stat, &fstat(sub.as_raw_fd())?) {
                return Err(io::Error::other("a folder changed while it was removed"));
            }
            remove_contents(&sub, depth + 1)?;
            unlink_at(dir.as_raw_fd(), &name, true)?;
        } else {
            unlink_at(dir.as_raw_fd(), &name, false)?;
        }
    }
    Ok(())
}

/// Create the run's new folder `name` in `parent` (root only, mode 0700)
/// with `homes` and `shared` in it.
fn create_output(parent_path: &Path, name: &str, live: Option<&Path>) -> Result<Output, String> {
    let shown = parent_path.join(name);
    let failed =
        |what: &str, error: io::Error| format!("cannot {what} {}: {error}", shown.display());
    let parent = open_dir_no_links(parent_path)
        .map_err(|error| format!("cannot open {}: {error}", parent_path.display()))?;
    let c = CString::new(name).map_err(|_| format!("invalid folder name {name:?}"))?;
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(format!("invalid folder name {name:?}"));
    }
    match mkdir_at(parent.as_raw_fd(), name, 0o700) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            return Err(format!(
                "{} exists; the restore writes only into a new folder",
                shown.display()
            ))
        }
        Err(error) => return Err(failed("create", error)),
    }
    let root = open_created_dir(parent.as_raw_fd(), name).map_err(|error| failed("open", error))?;
    fchmod(root.as_raw_fd(), 0o700).map_err(|error| failed("chmod", error))?;
    let root_stat = fstat(root.as_raw_fd()).map_err(|error| failed("inspect", error))?;
    let sub = |part: &str| -> Result<OwnedFd, String> {
        mkdir_at(root.as_raw_fd(), part, 0o700).map_err(|error| failed("fill", error))?;
        open_created_dir(root.as_raw_fd(), part).map_err(|error| failed("fill", error))
    };
    let homes = sub("homes")?;
    let shared = sub("shared")?;
    let live = match live {
        None => None,
        Some(storage) => {
            let storage_fd = open_dir_no_links(storage)
                .map_err(|error| format!("cannot open {}: {error}", storage.display()))?;
            let open = |part: &CStr| {
                open_at(storage_fd.as_raw_fd(), part, DIR_FLAGS).map_err(|error| {
                    format!(
                        "cannot open {}/{}: {error}",
                        storage.display(),
                        part.to_string_lossy()
                    )
                })
            };
            Some((open(c"homes")?, open(c"shared")?, storage.to_path_buf()))
        }
    };
    Ok(Output {
        parent,
        name: c,
        root,
        root_stat,
        homes,
        shared,
        shown,
        live,
    })
}

impl Output {
    fn container(&self, kind: TreeKind) -> &OwnedFd {
        match kind {
            TreeKind::Home => &self.homes,
            TreeKind::Shared => &self.shared,
        }
    }

    fn live(&self, kind: TreeKind) -> Option<&OwnedFd> {
        self.live.as_ref().map(|(homes, shared, _)| match kind {
            TreeKind::Home => homes,
            TreeKind::Shared => shared,
        })
    }

    /// Remove the new folder and everything in it.
    fn remove(self) -> io::Result<()> {
        let Output {
            parent,
            name,
            root,
            root_stat,
            homes,
            shared,
            ..
        } = self;
        drop((homes, shared));
        let opened = fstat(root.as_raw_fd())?;
        if !same_inode(&opened, &root_stat) {
            return Err(io::Error::other("the output folder changed"));
        }
        remove_contents(&root, 0)?;
        let now = lstat_at(parent.as_raw_fd(), &name)?;
        if !same_inode(&now, &root_stat) {
            return Err(io::Error::other(
                "the output folder was renamed; its contents are removed",
            ));
        }
        unlink_at(parent.as_raw_fd(), &name, true)
    }
}

/// Read and drop `size` bytes.
fn discard(input: &mut dyn Read, size: u64, buffer: &mut [u8]) -> Result<(), ArchiveError> {
    let mut remaining = size;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        archive::read_exact(input, &mut buffer[..want])?;
        remaining -= want as u64;
    }
    Ok(())
}

/// Where an archive path belongs.
enum Place {
    /// Not user files (database, configuration, apps).
    Elsewhere,
    /// Below `users/` but not in a folder of a HOME or shared folder.
    Invalid(String),
    Tree {
        kind: TreeKind,
        name: String,
        relative: Vec<u8>,
    },
}

fn place(path: &[u8]) -> Place {
    let Some(rest) = path
        .strip_prefix(b"users/")
        .or_else(|| (path == b"users").then_some(&b""[..]))
    else {
        return Place::Elsewhere;
    };
    let mut parts = rest.splitn(3, |byte| *byte == b'/');
    let kind = match parts.next() {
        Some(b"homes") => TreeKind::Home,
        Some(b"shared") => TreeKind::Shared,
        _ => return Place::Invalid("not in users/homes or users/shared".into()),
    };
    let Some(name) = parts.next().and_then(|name| std::str::from_utf8(name).ok()) else {
        return Place::Invalid("not in the folder of a HOME or a shared folder".into());
    };
    let valid = match kind {
        TreeKind::Home => validate_home_username(name),
        TreeKind::Shared => validate_share_container(name),
    };
    if valid.is_err() {
        return Place::Invalid(format!("{name:?} is not a valid folder name"));
    }
    Place::Tree {
        kind,
        name: name.to_string(),
        relative: parts.next().unwrap_or_default().to_vec(),
    }
}

fn prefix_of(kind: TreeKind, name: &str) -> String {
    match kind {
        TreeKind::Home => format!("users/homes/{name}"),
        TreeKind::Shared => format!("users/shared/{name}"),
    }
}

impl<'a> Run<'a> {
    fn writing(&self) -> bool {
        self.out.is_some() && self.refused_count == 0
    }

    fn refuse(&mut self, path: &str, reason: &str) {
        self.refused_count += 1;
        if self.refused.len() < MAX_REFUSALS {
            let text: String = format!("{path}: {reason}")
                .chars()
                .take(MAX_REFUSAL_CHARS)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            self.refused.push(text);
        }
    }

    fn refuse_in(&mut self, prefix: &str, relative: &[u8], reason: &str) {
        if let Some(tree) = self.trees.get_mut(prefix) {
            tree.refused = true;
        }
        let path = if relative.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix}/{}", shown(relative))
        };
        self.refuse(&path, reason);
    }

    fn selected(&self, prefix: &str) -> bool {
        self.selection
            .as_ref()
            .is_none_or(|selection| selection.contains(prefix))
    }

    /// One entry and its data.
    fn entry(&mut self, meta: EntryMeta, input: &mut dyn Read) -> Result<(), ArchiveError> {
        let size = meta.data_size();
        let mut buffer = std::mem::take(&mut self.buffer);
        let result = self.entry_data(meta, input, &mut buffer);
        self.buffer = buffer;
        result?;
        archive::read_padding(input, size)
    }

    fn entry_data(
        &mut self,
        meta: EntryMeta,
        input: &mut dyn Read,
        buffer: &mut [u8],
    ) -> Result<(), ArchiveError> {
        let size = meta.data_size();
        if meta.path == b"manifest.json" {
            return self.manifest(&meta, input, buffer);
        }
        let (kind, name, relative) = match place(&meta.path) {
            Place::Elsewhere => return discard(input, size, buffer),
            Place::Invalid(reason) => {
                if self.selection.is_none() {
                    self.refuse(&shown(&meta.path), &reason);
                }
                return discard(input, size, buffer);
            }
            Place::Tree {
                kind,
                name,
                relative,
            } => (kind, name, relative),
        };
        let prefix = prefix_of(kind, &name);
        if !self.selected(&prefix) {
            return discard(input, size, buffer);
        }
        if !self.trees.contains_key(&prefix) {
            let exists = self
                .out
                .as_ref()
                .and_then(|out| out.live(kind))
                .and_then(|live| {
                    CString::new(name.as_str())
                        .ok()
                        .map(|c| lstat_at(live.as_raw_fd(), &c).is_ok())
                })
                .unwrap_or(false);
            self.trees.insert(
                prefix.clone(),
                Tree {
                    kind,
                    name: name.clone(),
                    layout: None,
                    known: HashMap::new(),
                    entries: 0,
                    files: 0,
                    bytes: 0,
                    exists,
                    refused: false,
                    root: None,
                    cache: None,
                    dirs: Vec::new(),
                    status: None,
                    path: None,
                },
            );
        }
        let tree = self.trees.get_mut(&prefix).expect("inserted above");
        tree.entries += 1;
        if let Kind::File { size } = meta.kind {
            tree.files += 1;
            tree.bytes += size;
        }
        if tree.layout == Some(Layout::Unknown) {
            return discard(input, size, buffer);
        }
        let plan = match self.check(&prefix, &relative, &meta) {
            Ok(plan) => plan,
            Err(reason) => {
                self.refuse_in(&prefix, &relative, &reason);
                return discard(input, size, buffer);
            }
        };
        let write = self.writing() && !self.trees[&prefix].exists;
        if !write {
            return discard(input, size, buffer);
        }
        let mut pending_data = Some(size);
        if let Err(reason) = self.write(
            &prefix,
            &relative,
            plan,
            &meta,
            input,
            buffer,
            &mut pending_data,
        ) {
            self.refuse_in(&prefix, &relative, &reason);
        }
        // Whatever a failed write left unread.
        if let Some(rest) = pending_data {
            discard(input, rest, buffer)?;
        }
        Ok(())
    }

    fn manifest(
        &mut self,
        meta: &EntryMeta,
        input: &mut dyn Read,
        buffer: &mut [u8],
    ) -> Result<(), ArchiveError> {
        let size = meta.data_size();
        let refuse = |run: &mut Self, reason: &str| run.refuse("manifest.json", reason);
        if self.manifest_seen {
            refuse(self, "appears twice");
            return discard(input, size, buffer);
        }
        self.manifest_seen = true;
        if !matches!(meta.kind, Kind::File { .. }) || size > MAX_MANIFEST_BYTES {
            refuse(self, "not a file of a sensible size");
            return discard(input, size, buffer);
        }
        let mut data = vec![0u8; size as usize];
        archive::read_exact(input, &mut data)?;
        #[derive(Deserialize)]
        struct View {
            #[serde(default, rename = "preservedTrees")]
            preserved_trees: Vec<String>,
        }
        match serde_json::from_slice::<View>(&data) {
            Ok(view) => self.manifest = Some(view.preserved_trees),
            Err(error) => refuse(self, &format!("invalid: {error}")),
        }
        Ok(())
    }

    /// Remember `relative` as `known` in the tree.
    fn know(&mut self, prefix: &str, relative: &[u8], known: Known) -> Result<(), String> {
        self.known_bytes += relative.len() + 48;
        if self.known_bytes > MAX_KNOWN_BYTES {
            return Err(
                "too many entries for one run; restore fewer folders at a time with --tree".into(),
            );
        }
        self.trees
            .get_mut(prefix)
            .expect("known tree")
            .known
            .insert(relative.to_vec(), known);
        Ok(())
    }

    /// Check one entry of the folder `prefix` and decide what it becomes.
    fn check(&mut self, prefix: &str, relative: &[u8], meta: &EntryMeta) -> Result<Plan, String> {
        let policy = self.policy;
        let options = self.options;
        let service = (options.service_uid, options.service_gid);
        let tree = &self.trees[prefix];
        let (kind, name) = (tree.kind, tree.name.clone());
        let layout = match tree.layout {
            Some(layout) => layout,
            None if relative.is_empty() => {
                // The folder itself first: a folder with owners.
                if meta.kind != Kind::Dir {
                    self.trees.get_mut(prefix).unwrap().layout = Some(Layout::Unknown);
                    return Err("the folder itself is not a folder".into());
                }
                let layout = match kind {
                    TreeKind::Home => match policy.home_identity(&name) {
                        Ok((uid, _)) if meta.uid != uid => Err(format!(
                            "the HOME belongs to uid {} in the archive; in one-HOME mode it \
                             belongs to its account's identity (uid {uid})",
                            meta.uid
                        )),
                        Ok((uid, id)) => Ok(Layout::OneHome { uid, id }),
                        Err(problem) => Err(format!("a HOME with owners, but {problem}")),
                    },
                    TreeKind::Shared => {
                        if meta.uid != 0 {
                            Err(format!(
                                "the container belongs to uid {} in the archive; a migrated \
                                 shared folder belongs to root",
                                meta.uid
                            ))
                        } else if let Some(problem) = policy.container_problem(&name, meta.gid) {
                            Err(format!("a shared folder with owners, but {problem}"))
                        } else {
                            Ok(Layout::Group { gid: meta.gid })
                        }
                    }
                };
                let tree = self.trees.get_mut(prefix).unwrap();
                tree.layout = Some(*layout.as_ref().unwrap_or(&Layout::Unknown));
                let layout = layout?;
                let (acl_access, dropped_access) = policy.acl(layout, meta.acl_access.clone())?;
                let (acl_default, dropped_default) =
                    policy.acl(layout, meta.acl_default.clone())?;
                self.acl_entries_dropped += dropped_access + dropped_default;
                self.know(prefix, b"", Known::Dir)?;
                return Ok(match layout {
                    Layout::OneHome { uid, .. } => Plan::Dir {
                        uid,
                        gid: uid,
                        mode: 0o700,
                        acl_access,
                        acl_default,
                    },
                    Layout::Group { gid } => Plan::Dir {
                        uid: 0,
                        gid,
                        mode: 0o2770,
                        acl_access,
                        acl_default,
                    },
                    _ => unreachable!("a folder entry is one-HOME or group"),
                });
            }
            None => {
                // Files first: a folder the service read.
                self.trees.get_mut(prefix).unwrap().layout = Some(Layout::Legacy);
                self.know(prefix, b"", Known::Dir)?;
                Layout::Legacy
            }
        };
        if relative.is_empty() {
            return Err("the folder itself appears twice".into());
        }
        if relative.split(|byte| *byte == b'/').count() > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        if self.trees[prefix].known.contains_key(relative) {
            return Err("appears twice".into());
        }
        let parent = relative
            .iter()
            .rposition(|byte| *byte == b'/')
            .map(|at| &relative[..at])
            .unwrap_or(b"");
        if layout == Layout::Legacy {
            if !matches!(meta.kind, Kind::File { .. }) {
                return Err(
                    "a folder the service read holds only files; this entry is another type".into(),
                );
            }
            // Its folders were not stored: they are made as the service's.
            let mut ends: Vec<usize> = parent
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .map(|(at, _)| at)
                .collect();
            if !parent.is_empty() {
                ends.push(parent.len());
            }
            let mut new_dirs = Vec::new();
            for end in ends {
                let ancestor = parent[..end].to_vec();
                match self.trees[prefix].known.get(&ancestor) {
                    Some(Known::Dir) => {}
                    Some(_) => return Err("its folder is a file".into()),
                    None => {
                        self.know(prefix, &ancestor, Known::Dir)?;
                        new_dirs.push(ancestor);
                    }
                }
            }
            if meta.mode & 0o6000 != 0 {
                self.set_id_cleared += 1;
            }
            self.know(prefix, relative, Known::File)?;
            return Ok(Plan::File {
                uid: service.0,
                gid: service.1,
                mode: meta.mode & 0o777,
                acl: None,
                new_dirs,
            });
        }
        match self.trees[prefix].known.get(parent) {
            Some(Known::Dir) => {}
            Some(_) => return Err("its folder is a file or a symbolic link".into()),
            None => return Err("its folder is not in the archive before it".into()),
        }
        let plan = match &meta.kind {
            Kind::HardLink { target } => {
                let inside = target
                    .strip_prefix(prefix.as_bytes())
                    .and_then(|rest| rest.strip_prefix(b"/"));
                match inside.map(|rel| self.trees[prefix].known.get(rel)) {
                    Some(Some(Known::File)) => Plan::HardLink {
                        target: inside.unwrap().to_vec(),
                    },
                    Some(_) => {
                        return Err(format!(
                            "a hard link to {}, which is not a file before it",
                            shown(target)
                        ))
                    }
                    None => {
                        return Err(format!(
                            "a hard link to {}, outside its folder",
                            shown(target)
                        ))
                    }
                }
            }
            other => {
                let (uid, gid, changed) = policy.owner(layout, meta.uid, meta.gid)?;
                match changed {
                    Changed::No => {}
                    Changed::Group => self.groups_changed += 1,
                    Changed::Owner => self.owners_changed += 1,
                }
                match other {
                    Kind::Dir => {
                        let keep = match layout {
                            Layout::Group { .. } => 0o3777,
                            _ => 0o1777,
                        };
                        if meta.mode & 0o6000 & !keep != 0 {
                            self.set_id_cleared += 1;
                        }
                        let (acl_access, a) = policy.acl(layout, meta.acl_access.clone())?;
                        let (acl_default, d) = policy.acl(layout, meta.acl_default.clone())?;
                        self.acl_entries_dropped += a + d;
                        Plan::Dir {
                            uid,
                            gid,
                            mode: meta.mode & keep,
                            acl_access,
                            acl_default,
                        }
                    }
                    Kind::File { .. } => {
                        if meta.mode & 0o6000 != 0 {
                            self.set_id_cleared += 1;
                        }
                        let (acl, dropped) = policy.acl(layout, meta.acl_access.clone())?;
                        self.acl_entries_dropped += dropped;
                        Plan::File {
                            uid,
                            gid,
                            mode: meta.mode & 0o777,
                            acl,
                            new_dirs: Vec::new(),
                        }
                    }
                    Kind::Symlink { target } => {
                        if target.len() > MAX_SYMLINK_TARGET {
                            return Err("the link target is too long".into());
                        }
                        Plan::Symlink {
                            uid,
                            gid,
                            target: target.clone(),
                        }
                    }
                    Kind::HardLink { .. } => unreachable!("handled above"),
                }
            }
        };
        let known = match plan {
            Plan::Dir { .. } => Known::Dir,
            Plan::File { .. } => Known::File,
            // A second name of a file: a later hard link may name it too.
            Plan::HardLink { .. } => Known::File,
            Plan::Symlink { .. } => Known::Other,
        };
        self.know(prefix, relative, known)?;
        Ok(plan)
    }

    /// The folder `parent` of the tree, opened (cached).
    fn parent_fd(&mut self, prefix: &str, parent: &[u8]) -> io::Result<RawFd> {
        let tree = self.trees.get_mut(prefix).unwrap();
        let hit = tree
            .cache
            .as_ref()
            .is_some_and(|(cached, _)| cached == parent);
        if !hit {
            let root = tree.root.as_ref().expect("the folder exists");
            let fd = open_rel(root, parent)?;
            tree.cache = Some((parent.to_vec(), fd));
        }
        Ok(tree.cache.as_ref().unwrap().1.as_raw_fd())
    }

    /// Create the tree's folder in the output (and its implicit folders).
    fn ensure_root(&mut self, prefix: &str) -> Result<(), String> {
        if self.trees[prefix].root.is_some() {
            return Ok(());
        }
        let out = self.out.as_ref().expect("writing");
        let (kind, name) = (self.trees[prefix].kind, self.trees[prefix].name.clone());
        let container = out.container(kind);
        mkdir_at(container.as_raw_fd(), &name, 0o700)
            .map_err(|error| format!("cannot create the folder: {error}"))?;
        let fd = open_created_dir(container.as_raw_fd(), &name)
            .map_err(|error| format!("cannot open the folder: {error}"))?;
        let (uid, gid) = (self.options.service_uid, self.options.service_gid);
        let tree = self.trees.get_mut(prefix).unwrap();
        tree.root = Some(fd);
        if tree.layout == Some(Layout::Legacy) {
            // A folder the service read: its own, like its folders.
            tree.dirs.push(Pending {
                relative: Vec::new(),
                uid,
                gid,
                mode: 0o700,
                acl_access: None,
                acl_default: None,
                mtime: None,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        prefix: &str,
        relative: &[u8],
        plan: Plan,
        meta: &EntryMeta,
        input: &mut dyn Read,
        buffer: &mut [u8],
        pending_data: &mut Option<u64>,
    ) -> Result<(), String> {
        self.ensure_root(prefix)?;
        let owners = self.options.owners.clone();
        let failed = |what: &str, error: io::Error| format!("cannot {what}: {error}");
        if relative.is_empty() {
            let Plan::Dir {
                uid,
                gid,
                mode,
                acl_access,
                acl_default,
            } = plan
            else {
                unreachable!("the folder itself is a folder");
            };
            self.trees.get_mut(prefix).unwrap().dirs.push(Pending {
                relative: Vec::new(),
                uid,
                gid,
                mode,
                acl_access,
                acl_default,
                mtime: Some(meta.mtime),
            });
            return Ok(());
        }
        // Folders the service did not store, for a file it read.
        if let Plan::File { new_dirs, .. } = &plan {
            let (uid, gid) = (self.options.service_uid, self.options.service_gid);
            for new in new_dirs {
                let (up, last) = split(new);
                let at = self
                    .parent_fd(prefix, up)
                    .map_err(|e| failed("open a folder", e))?;
                let last = cname(last).map_err(|e| failed("name a folder", e))?;
                // SAFETY: valid fd and NUL-terminated name.
                if unsafe { libc::mkdirat(at, last.as_ptr(), 0o700) } != 0 {
                    return Err(failed("create a folder", io::Error::last_os_error()));
                }
                self.trees.get_mut(prefix).unwrap().dirs.push(Pending {
                    relative: new.clone(),
                    uid,
                    gid,
                    mode: 0o700,
                    acl_access: None,
                    acl_default: None,
                    mtime: None,
                });
            }
        }
        let (parent, name) = split(relative);
        let c = cname(name).map_err(|e| failed("name the entry", e))?;
        let dir = self
            .parent_fd(prefix, parent)
            .map_err(|e| failed("open its folder", e))?;
        match plan {
            Plan::Dir {
                uid,
                gid,
                mode,
                acl_access,
                acl_default,
            } => {
                // SAFETY: valid fd and NUL-terminated name.
                if unsafe { libc::mkdirat(dir, c.as_ptr(), 0o700) } != 0 {
                    return Err(failed("create the folder", io::Error::last_os_error()));
                }
                self.trees.get_mut(prefix).unwrap().dirs.push(Pending {
                    relative: relative.to_vec(),
                    uid,
                    gid,
                    mode,
                    acl_access,
                    acl_default,
                    mtime: Some(meta.mtime),
                });
                Ok(())
            }
            Plan::File {
                uid,
                gid,
                mode,
                acl,
                ..
            } => {
                let size = meta.data_size();
                let out = self.out.as_ref().expect("writing");
                if available(out.root.as_raw_fd())
                    .is_some_and(|free| free < size.saturating_add(FREE_SPACE_RESERVE))
                {
                    return Err(format!(
                        "not enough free space for {size} bytes (a restore leaves {} bytes free)",
                        FREE_SPACE_RESERVE
                    ));
                }
                let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
                // SAFETY: valid fd and NUL-terminated name; mode as varargs.
                let raw = unsafe {
                    libc::openat(
                        dir,
                        c.as_ptr(),
                        flags | libc::O_CLOEXEC,
                        0o600 as libc::c_uint,
                    )
                };
                if raw < 0 {
                    return Err(failed("create the file", io::Error::last_os_error()));
                }
                // SAFETY: a fresh, valid descriptor.
                let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
                let mut remaining = size;
                let mut failure = None;
                while remaining > 0 {
                    let want = remaining.min(buffer.len() as u64) as usize;
                    if let Err(error) = archive::read_exact(input, &mut buffer[..want]) {
                        // The archive itself broke: nothing more can be read.
                        *pending_data = None;
                        return Err(error.0);
                    }
                    remaining -= want as u64;
                    *pending_data = Some(remaining);
                    if failure.is_none() {
                        if let Err(error) = file.write_all(&buffer[..want]) {
                            failure = Some(error);
                        }
                    }
                }
                *pending_data = None;
                if let Some(error) = failure {
                    return Err(failed("write the file", error));
                }
                let fd = file.as_raw_fd();
                owners
                    .chown_fd(fd, uid, gid)
                    .map_err(|e| failed("chown the file", e))?;
                if let Some(acl) = &acl {
                    acl::write(fd, acl::ACCESS_XATTR, acl)
                        .map_err(|e| failed("set the ACL of the file", e))?;
                }
                fchmod(fd, mode as libc::mode_t).map_err(|e| failed("chmod the file", e))?;
                set_mtime(fd, meta.mtime).map_err(|e| failed("set the time of the file", e))?;
                Ok(())
            }
            Plan::Symlink { uid, gid, target } => {
                let target = cname(&target).map_err(|e| failed("read the link target", e))?;
                // SAFETY: NUL-terminated target and name, valid fd.
                if unsafe { libc::symlinkat(target.as_ptr(), dir, c.as_ptr()) } != 0 {
                    return Err(failed("create the link", io::Error::last_os_error()));
                }
                owners
                    .chown_link(dir, &c, uid, gid)
                    .map_err(|e| failed("chown the link", e))?;
                set_link_mtime(dir, &c, meta.mtime)
                    .map_err(|e| failed("set the time of the link", e))?;
                Ok(())
            }
            Plan::HardLink { target } => {
                let (target_parent, target_name) = split(&target);
                let root = self.trees[prefix].root.as_ref().unwrap();
                let from = open_rel(root, target_parent).map_err(|e| failed("open a folder", e))?;
                let from_name = cname(target_name).map_err(|e| failed("name the target", e))?;
                // Only to a regular file this run wrote (the run's folder is
                // root's alone, so nothing else can be there).
                let stat = lstat_at(from.as_raw_fd(), &from_name)
                    .map_err(|e| failed("find the link target", e))?;
                if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err("the hard link target is not a file".into());
                }
                let dir = self
                    .parent_fd(prefix, parent)
                    .map_err(|e| failed("open its folder", e))?;
                // SAFETY: valid fds and NUL-terminated names; no
                // AT_SYMLINK_FOLLOW.
                if unsafe { libc::linkat(from.as_raw_fd(), from_name.as_ptr(), dir, c.as_ptr(), 0) }
                    != 0
                {
                    return Err(failed("create the hard link", io::Error::last_os_error()));
                }
                Ok(())
            }
        }
    }

    /// Owners, ACLs, modes and times of the folders of a tree, deepest
    /// first, the tree's own folder last.
    fn finish_dirs(&mut self, prefix: &str) -> Result<(), String> {
        let owners = self.options.owners.clone();
        let tree = self.trees.get_mut(prefix).unwrap();
        tree.cache = None;
        let Some(root) = tree.root.as_ref() else {
            return Ok(());
        };
        let dirs = std::mem::take(&mut tree.dirs);
        for dir in dirs.iter().rev() {
            let shown_path = || format!("{prefix}/{}", shown(&dir.relative));
            let failed =
                |what: &str, error: io::Error| format!("{}: cannot {what}: {error}", shown_path());
            let fd = open_rel(root, &dir.relative).map_err(|e| failed("open", e))?;
            owners
                .chown_fd(fd.as_raw_fd(), dir.uid, dir.gid)
                .map_err(|e| failed("chown", e))?;
            if let Some(acl) = &dir.acl_access {
                acl::write(fd.as_raw_fd(), acl::ACCESS_XATTR, acl)
                    .map_err(|e| failed("set the ACL of", e))?;
            }
            if let Some(acl) = &dir.acl_default {
                acl::write(fd.as_raw_fd(), acl::DEFAULT_XATTR, acl)
                    .map_err(|e| failed("set the default ACL of", e))?;
            }
            fchmod(fd.as_raw_fd(), dir.mode as libc::mode_t).map_err(|e| failed("chmod", e))?;
            if let Some(mtime) = dir.mtime {
                set_mtime(fd.as_raw_fd(), mtime).map_err(|e| failed("set the time of", e))?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Report {
        // The manifest must agree with what the archive holds; it never
        // decides.
        match self.manifest.clone() {
            None if !self.manifest_seen => self.refuse(
                "manifest.json",
                "missing; this is not a complete Backup Center archive",
            ),
            None => {}
            Some(listed) => {
                let listed: HashSet<String> = listed.into_iter().collect();
                let checks: Vec<(String, Option<Layout>)> = self
                    .trees
                    .iter()
                    .map(|(prefix, tree)| (prefix.clone(), tree.layout))
                    .collect();
                for (prefix, layout) in checks {
                    let preserved =
                        matches!(layout, Some(Layout::OneHome { .. } | Layout::Group { .. }));
                    let legacy = layout == Some(Layout::Legacy);
                    if preserved && !listed.contains(&prefix) {
                        self.refuse_in(
                            &prefix,
                            b"",
                            "the archive holds its owners, but manifest.json does not list it \
                             under preservedTrees",
                        );
                    } else if legacy && listed.contains(&prefix) {
                        self.refuse_in(
                            &prefix,
                            b"",
                            "listed under preservedTrees, but the archive holds it as the \
                             service read it (without its folder and owners)",
                        );
                    }
                }
                let invalid = listed
                    .iter()
                    .filter(|prefix| parse_tree(prefix).is_err())
                    .count();
                if invalid > 0 {
                    self.refuse(
                        "manifest.json",
                        &format!("lists {invalid} invalid folder names under preservedTrees"),
                    );
                }
                let mut notes: Vec<String> = listed
                    .iter()
                    .filter(|prefix| parse_tree(prefix).is_ok())
                    .filter(|prefix| self.selected(prefix) && !self.trees.contains_key(*prefix))
                    .map(|prefix| {
                        format!(
                            "{prefix} is listed under preservedTrees but has no entry (its \
                             backup broke; see the backup's warnings)"
                        )
                    })
                    .collect();
                notes.sort();
                notes.truncate(MAX_REFUSALS);
                self.manifest_notes = notes;
            }
        }
        if let Some(selection) = self.selection.clone() {
            let mut missing: Vec<&String> = selection
                .iter()
                .filter(|prefix| !self.trees.contains_key(*prefix))
                .collect();
            missing.sort();
            for prefix in missing {
                self.refuse(prefix, "not in the archive");
            }
        }
        let mode = match self.options.target {
            Target::DryRun => "dry-run",
            Target::Scratch(_) => "scratch",
            Target::Storage { .. } => "storage",
        };
        let prefixes: Vec<String> = self.trees.keys().cloned().collect();
        if self.refused_count == 0 && self.out.is_some() {
            for prefix in &prefixes {
                if let Err(reason) = self.finish_dirs(prefix) {
                    self.refuse(prefix, &reason);
                    break;
                }
            }
        }
        let mut target = None;
        if self.refused_count > 0 {
            for prefix in &prefixes {
                let tree = self.trees.get_mut(prefix).unwrap();
                tree.root = None;
                tree.cache = None;
            }
            if let Some(out) = self.out.take() {
                let shown_out = out.shown.clone();
                if let Err(error) = out.remove() {
                    self.refuse(
                        &shown_out.display().to_string(),
                        &format!("cannot remove what was written: {error}; remove it as root"),
                    );
                    target = Some(shown_out.display().to_string());
                }
            }
        } else if let Some(out) = self.out.take() {
            target = self.publish(out, &prefixes);
        }
        let ok = self.refused_count == 0;
        let trees = prefixes
            .iter()
            .map(|prefix| {
                let tree = &self.trees[prefix];
                let layout = tree.layout.unwrap_or(Layout::Unknown);
                let (layout_name, owner) = match layout {
                    Layout::Legacy => ("legacy", "rast (the service)".to_string()),
                    Layout::OneHome { uid, id } => ("one-home", format!("rast-u{id} (uid {uid})")),
                    Layout::Group { gid } => ("group", format!("root, group {gid}")),
                    Layout::Unknown => ("unknown", "-".to_string()),
                };
                let status = if tree.refused {
                    "refused".to_string()
                } else if !ok {
                    "not written".to_string()
                } else if mode == "dry-run" {
                    "checked".to_string()
                } else if tree.exists {
                    "exists".to_string()
                } else {
                    tree.status.clone().unwrap_or_else(|| "restored".into())
                };
                TreeReport {
                    tree: prefix.clone(),
                    layout: layout_name.into(),
                    owner,
                    entries: tree.entries,
                    files: tree.files,
                    bytes: tree.bytes,
                    status,
                    path: if ok { tree.path.clone() } else { None },
                }
            })
            .collect();
        Report {
            ok,
            mode: mode.into(),
            target,
            trees,
            refused: self.refused,
            refused_count: self.refused_count,
            set_id_cleared: self.set_id_cleared,
            groups_changed: self.groups_changed,
            owners_changed: self.owners_changed,
            acl_entries_dropped: self.acl_entries_dropped,
            notes: self.manifest_notes,
        }
    }

    /// Put the written folders where they belong; the folder that holds
    /// what stays (scratch, or a staging folder that is not empty).
    fn publish(&mut self, out: Output, prefixes: &[String]) -> Option<String> {
        for prefix in prefixes {
            let tree = self.trees.get_mut(prefix).unwrap();
            tree.cache = None;
            tree.root = None;
        }
        let Some((live_homes, live_shared, storage)) = out.live.as_ref() else {
            for prefix in prefixes {
                let tree = self.trees.get_mut(prefix).unwrap();
                if !tree.exists {
                    let part = match tree.kind {
                        TreeKind::Home => "homes",
                        TreeKind::Shared => "shared",
                    };
                    tree.path = Some(out.shown.join(part).join(&tree.name).display().to_string());
                }
            }
            return Some(out.shown.display().to_string());
        };
        let mut left = false;
        for prefix in prefixes {
            let tree = self.trees.get_mut(prefix).unwrap();
            if tree.exists {
                continue;
            }
            let (from, to, part) = match tree.kind {
                TreeKind::Home => (&out.homes, live_homes, "homes"),
                TreeKind::Shared => (&out.shared, live_shared, "shared"),
            };
            let Ok(c) = CString::new(tree.name.as_str()) else {
                continue;
            };
            // SAFETY: valid fds and NUL-terminated names.
            let rc = unsafe {
                libc::renameat2(
                    from.as_raw_fd(),
                    c.as_ptr(),
                    to.as_raw_fd(),
                    c.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc == 0 {
                tree.path = Some(storage.join(part).join(&tree.name).display().to_string());
            } else {
                left = true;
                tree.status = Some("kept".into());
                tree.path = Some(out.shown.join(part).join(&tree.name).display().to_string());
            }
        }
        if left {
            return Some(out.shown.display().to_string());
        }
        let _ = unlink_at(out.root.as_raw_fd(), c"homes", true);
        let _ = unlink_at(out.root.as_raw_fd(), c"shared", true);
        let still = lstat_at(out.parent.as_raw_fd(), &out.name)
            .is_ok_and(|stat| same_inode(&stat, &out.root_stat));
        if still && dir_is_empty(out.root.as_raw_fd()).unwrap_or(false) {
            let _ = unlink_at(out.parent.as_raw_fd(), &out.name, true);
            None
        } else {
            Some(out.shown.display().to_string())
        }
    }
}

/// The parent and the last name of a relative path.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&b""[..], path),
    }
}

/// Restore the `users` part of the (decompressed) archive `input`. An error
/// means nothing could be started (the target, the options); everything
/// after that, refusals included, is in the report.
pub fn restore(
    input: &mut dyn Read,
    options: &Options,
    facts: &Facts,
    host: &HostAccounts,
) -> Result<Report, String> {
    let selection = if options.trees.is_empty() {
        None
    } else {
        Some(
            options
                .trees
                .iter()
                .map(|tree| parse_tree(tree))
                .collect::<Result<HashSet<String>, String>>()?,
        )
    };
    let policy = Policy::new(options, facts, host)?;
    let out = match &options.target {
        Target::DryRun => None,
        Target::Scratch(path) => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(format!("{} cannot be a new folder", path.display()));
            };
            let name = name
                .to_str()
                .ok_or_else(|| format!("{} is not a valid name", path.display()))?;
            Some(create_output(parent, name, None)?)
        }
        Target::Storage { storage, staging } => {
            Some(create_output(storage, staging, Some(storage))?)
        }
    };
    let mut run = Run {
        policy: &policy,
        options,
        selection,
        out,
        trees: BTreeMap::new(),
        refused: Vec::new(),
        refused_count: 0,
        manifest: None,
        manifest_seen: false,
        manifest_notes: Vec::new(),
        known_bytes: 0,
        set_id_cleared: 0,
        groups_changed: 0,
        owners_changed: 0,
        acl_entries_dropped: 0,
        buffer: vec![0u8; COPY_BUFFER],
    };
    loop {
        match archive::read_backup_entry(input) {
            Ok(Next::End) => {
                if let Err(error) = archive::read_end(input) {
                    run.refuse("archive", &error.0);
                }
                break;
            }
            Ok(Next::Entry(meta)) => {
                if let Err(error) = run.entry(meta, input) {
                    run.refuse("archive", &format!("cannot be read: {error}"));
                    break;
                }
            }
            Err(error) => {
                run.refuse("archive", &format!("refused: {error}"));
                break;
            }
        }
    }
    Ok(run.finish())
}

#[cfg(test)]
mod tests;
