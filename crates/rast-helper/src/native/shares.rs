//! Shared folders as Linux groups (ADR 0005, sections 1, 3, 4 and 8).
//!
//! Every shared folder has two groups, `rast-s<h>-rw` and `rast-s<h>-ro`
//! (`<h>`: the first 16 hex digits of the share id), with gids from a range
//! derived from `linux_uid_base` (D11). Its container
//! `<storage>/shared/<container>` becomes `root:rast-s<h>-rw` 2770 with the
//! ACL `g:rast-s<h>-ro:r-x`, `u:rast:rwx` and the same entries as its
//! default ACL, so new files inherit the group and the entries.
//!
//! * `share_group_ensure`, `share_group_remove` and `share_members_set`
//!   manage the groups (`groupadd`, `groupdel`, `gpasswd -M`).
//! * `share_dir_prepare` applies owner, mode and ACLs to the container (and,
//!   with `recursive`, to everything in it); `share_dir_remove` removes the
//!   container of a deleted share.
//! * `shares_migrate_scan`, `shares_migrate_apply` and
//!   `shares_migrate_rollback` are the root steps of `rast migrate shares`:
//!   a read-only report, a journal of every group, mode and ACL followed by
//!   the recursive prepare, and the restore from that journal.
//! * `share_dir_repair` is the recursive prepare of a container that
//!   already has the layout (the daily repair pass; legacy containers wait
//!   for `rast migrate shares`).
//! * `share_deny_set` sets the per-user deny entries `u:rast-u<id>:---` on
//!   a container while a revocation waits for the user's session restart
//!   (D7); every prepare keeps them.
//! * `share_session_groups` reports which share groups the running
//!   processes of some users carry, from `/proc/<pid>/status`, so the
//!   server can find sessions that still hold a revoked group.
//!
//! The members of a shared folder can change it while root works on it, so
//! every tree walk goes through directory descriptors: entries are looked up
//! with `fstatat(AT_SYMLINK_NOFOLLOW)`, opened with `O_NOFOLLOW` and checked
//! against that inode before anything is changed, and changes are made with
//! `fchown`, `fchmod` and `fsetxattr` on the open descriptor. Walks never
//! cross into another file system. Symbolic links, special files, files with
//! more than one hard link, entries owned by root and entries owned by any
//! account other than the `rast` service and the RAST identities are left
//! unchanged and reported: such an entry can only be in a share as a hard
//! link to a file elsewhere (with `fs.protected_hardlinks = 0`, any file the
//! member can reach), whose original name may since be gone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CStr, CString, OsString};
use std::io::{self, BufRead, Write};
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native::acl::{self, Acl, Entry, Tag};
use crate::native::homes::{clear_errno, fstat, is_dir, is_symlink, open_dir, DIR_FLAGS};
use crate::native::identity::PasswdEntry;
use crate::native::protocol::{
    identity_ids, username_for, validate_share_container, validate_share_id, NativeError,
    ShareDenySetOp, ShareDirOp, ShareDirPrepareOp, ShareGroupOp, ShareIdOp, ShareMembersSetOp,
    ShareSessionGroupsOp, MAX_SESSION_GROUP_USERS, MAX_SHARE_DENIALS, MAX_SHARE_MEMBERS,
    MAX_USER_ID,
};
use crate::native::{run_command, summarize_command, NativeOptions};

/// Share gids are `linux_uid_base` plus an offset from this range (D11). The
/// identities use offsets 1 ..= 1 000 000, so the ranges never meet.
pub const SHARE_GID_MIN_OFFSET: u32 = 1_000_001;
pub const SHARE_GID_MAX_OFFSET: u32 = 1_999_999;
/// Largest number of entries below one container that a walk accepts.
pub const MAX_TREE_ENTRIES: u64 = 1_000_000;
/// Deepest nesting a walk descends into (every level keeps a descriptor).
pub const MAX_DEPTH: usize = 256;
/// Paths listed per category in a report.
pub const REPORT_SAMPLE: usize = 50;
/// File name of a complete journal.
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// Suffix of a journal folder after its rollback.
pub const ROLLED_BACK_SUFFIX: &str = ".rolled-back";

const ACCOUNT_CMD_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ACCOUNT_OUTPUT: usize = 1024 * 1024;
/// Names per `getent passwd` call.
const GETENT_BATCH: usize = 500;

// ---------------------------------------------------------------------------
// Names, ranges and the group database
// ---------------------------------------------------------------------------

/// The two group names of a shared folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareGroups {
    pub rw: String,
    pub ro: String,
}

impl ShareGroups {
    pub fn for_share(share_id: &str) -> Result<Self, NativeError> {
        validate_share_id(share_id)?;
        let rw = rast_base::storage::shares::rw_group_name(share_id);
        let ro = rast_base::storage::shares::ro_group_name(share_id);
        match (rw, ro) {
            (Some(rw), Some(ro)) => Ok(Self { rw, ro }),
            _ => Err(NativeError::invalid("shareId must be a lowercase UUID v4")),
        }
    }
}

/// The gids that share groups may have.
pub fn share_gid_range(uid_base: u32) -> Result<RangeInclusive<u32>, NativeError> {
    let min = uid_base.checked_add(SHARE_GID_MIN_OFFSET);
    let max = uid_base.checked_add(SHARE_GID_MAX_OFFSET);
    match (min, max) {
        (Some(min), Some(max)) if max < u32::MAX - 1 => Ok(min..=max),
        _ => Err(NativeError::invalid(
            "linux_uid_base leaves no room for shared folder groups",
        )),
    }
}

/// The uids of RAST identities.
fn identity_uid_range(uid_base: u32) -> RangeInclusive<u32> {
    uid_base.saturating_add(1)..=uid_base.saturating_add(MAX_USER_ID as u32)
}

/// One `getent group` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Parse `name:x:gid:member,member`.
pub fn parse_group_line(line: &str) -> Result<GroupEntry, NativeError> {
    let fields: Vec<&str> = line.trim().split(':').collect();
    if fields.len() < 4 || fields[0].is_empty() {
        return Err(NativeError::command_failed(
            "unexpected output from getent group",
        ));
    }
    let gid = fields[2]
        .parse::<u32>()
        .map_err(|_| NativeError::command_failed("unexpected gid from getent group"))?;
    let members = fields[3]
        .split(',')
        .map(str::trim)
        .filter(|member| !member.is_empty())
        .map(str::to_string)
        .collect();
    Ok(GroupEntry {
        name: fields[0].to_string(),
        gid,
        members,
    })
}

/// `getent group <key>` (a name or a gid).
async fn group_lookup(
    options: &NativeOptions,
    key: &str,
) -> Result<Option<GroupEntry>, NativeError> {
    let output = run_command(
        &options.getent_bin,
        vec![OsString::from("group"), OsString::from(key)],
        &[],
        ACCOUNT_CMD_TIMEOUT,
        MAX_ACCOUNT_OUTPUT,
        64 * 1024,
    )
    .await?;
    let line = output.stdout.lines().find(|line| !line.trim().is_empty());
    match (output.success, line) {
        (_, Some(line)) => parse_group_line(line).map(Some),
        // `getent` exits 2 when the entry does not exist.
        (false, None) if output.stderr.trim().is_empty() => Ok(None),
        (true, None) => Ok(None),
        (false, None) => Err(NativeError::command_failed(format!(
            "cannot look up group {key}: {}",
            summarize_command(&output)
        ))),
    }
}

/// A share group that must exist with a gid from the share range.
async fn existing_share_group(
    options: &NativeOptions,
    name: &str,
    range: &RangeInclusive<u32>,
) -> Result<GroupEntry, NativeError> {
    let entry = group_lookup(options, name).await?.ok_or_else(|| {
        NativeError::not_found(format!(
            "group {name} does not exist; run share_group_ensure first"
        ))
    })?;
    if !range.contains(&entry.gid) {
        return Err(NativeError::identity_conflict(format!(
            "group {name} has gid {}, outside the RAST shared folder range",
            entry.gid
        )));
    }
    Ok(entry)
}

/// `groupadd` arguments for a share group: a gid from the share range, or
/// `gid` itself when it is given (a restore).
pub fn groupadd_share_args(
    name: &str,
    range: &RangeInclusive<u32>,
    gid: Option<u32>,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-K"),
        OsString::from(format!("GID_MIN={}", range.start())),
        OsString::from("-K"),
        OsString::from(format!("GID_MAX={}", range.end())),
    ];
    if let Some(gid) = gid {
        args.push(OsString::from("-g"));
        args.push(OsString::from(gid.to_string()));
    }
    args.push(OsString::from(name));
    args
}

/// Create `name` unless it exists; returns its gid and whether it was made.
async fn ensure_group(
    options: &NativeOptions,
    name: &str,
    range: &RangeInclusive<u32>,
    reuse: Option<u32>,
) -> Result<(u32, bool), NativeError> {
    if let Some(entry) = group_lookup(options, name).await? {
        if !range.contains(&entry.gid) {
            return Err(NativeError::identity_conflict(format!(
                "group {name} exists with gid {}, outside the RAST shared folder range",
                entry.gid
            )));
        }
        return Ok((entry.gid, false));
    }
    let mut gid = None;
    if let Some(candidate) = reuse.filter(|gid| range.contains(gid)) {
        if group_lookup(options, &candidate.to_string())
            .await?
            .is_none()
        {
            gid = Some(candidate);
        }
    }
    let output = run_command(
        &options.groupadd_bin,
        groupadd_share_args(name, range, gid),
        &[],
        ACCOUNT_CMD_TIMEOUT,
        64 * 1024,
        64 * 1024,
    )
    .await?;
    // A concurrent ensure may have created it: look again either way.
    match group_lookup(options, name).await? {
        Some(entry) if range.contains(&entry.gid) => Ok((entry.gid, output.success)),
        Some(entry) => Err(NativeError::identity_conflict(format!(
            "group {name} was created with gid {}, outside the RAST shared folder range",
            entry.gid
        ))),
        None => Err(NativeError::command_failed(format!(
            "cannot create group {name}: {}",
            summarize_command(&output)
        ))),
    }
}

/// The gids a container still carries, for reuse: its group (read-write)
/// and a named read-only group entry of its ACL, when they are in range.
fn reuse_candidates(
    options: &NativeOptions,
    container: &str,
    range: &RangeInclusive<u32>,
) -> (Option<u32>, Option<u32>) {
    let Ok((fd, stat)) = open_container(options, container) else {
        return (None, None);
    };
    let rw = Some(stat.st_gid).filter(|gid| range.contains(gid));
    let ro = acl::read(fd.as_raw_fd(), acl::ACCESS_XATTR)
        .ok()
        .flatten()
        .and_then(|acl| {
            acl.named_groups()
                .find(|(gid, perm)| range.contains(gid) && Some(*gid) != rw && perm & 2 == 0)
                .map(|(gid, _)| gid)
        });
    (rw, ro)
}

pub(crate) async fn share_group_ensure(
    options: &NativeOptions,
    op: ShareGroupOp,
) -> Result<Value, NativeError> {
    let groups = ShareGroups::for_share(&op.share_id)?;
    let range = share_gid_range(options.uid_base)?;
    let (reuse_rw, reuse_ro) = match &op.container {
        Some(container) => {
            validate_share_container(container)?;
            reuse_candidates(options, container, &range)
        }
        None => (None, None),
    };
    let (rw_gid, rw_created) = ensure_group(options, &groups.rw, &range, reuse_rw).await?;
    let reuse_ro = reuse_ro.filter(|gid| *gid != rw_gid);
    let (ro_gid, ro_created) = ensure_group(options, &groups.ro, &range, reuse_ro).await?;
    if rw_gid == ro_gid {
        return Err(NativeError::identity_conflict(format!(
            "groups {} and {} share gid {rw_gid}",
            groups.rw, groups.ro
        )));
    }
    tracing::info!(share = %op.share_id, rw_gid, ro_gid, "shared folder groups ensured");
    Ok(json!({
        "shareId": op.share_id,
        "rw": {"name": groups.rw, "gid": rw_gid, "created": rw_created},
        "ro": {"name": groups.ro, "gid": ro_gid, "created": ro_created},
    }))
}

pub(crate) async fn share_group_remove(
    options: &NativeOptions,
    op: ShareIdOp,
) -> Result<Value, NativeError> {
    let groups = ShareGroups::for_share(&op.share_id)?;
    let range = share_gid_range(options.uid_base)?;
    let mut removed = Vec::new();
    for name in [&groups.rw, &groups.ro] {
        let Some(entry) = group_lookup(options, name).await? else {
            continue;
        };
        if !range.contains(&entry.gid) {
            return Err(NativeError::identity_conflict(format!(
                "group {name} has gid {}, outside the RAST shared folder range; not removed",
                entry.gid
            )));
        }
        let output = run_command(
            &options.groupdel_bin,
            vec![OsString::from(name)],
            &[],
            ACCOUNT_CMD_TIMEOUT,
            64 * 1024,
            64 * 1024,
        )
        .await?;
        if !output.success && group_lookup(options, name).await?.is_some() {
            return Err(NativeError::command_failed(format!(
                "cannot remove group {name}: {}",
                summarize_command(&output)
            )));
        }
        removed.push(name.clone());
    }
    Ok(json!({"shareId": op.share_id, "removed": removed}))
}

/// Validate one member list.
fn member_list(ids: &[i64], what: &str) -> Result<BTreeSet<i64>, NativeError> {
    if ids.len() > MAX_SHARE_MEMBERS {
        return Err(NativeError::invalid(format!(
            "`{what}` lists more than {MAX_SHARE_MEMBERS} users"
        )));
    }
    let mut set = BTreeSet::new();
    for id in ids {
        if !(1..=MAX_USER_ID).contains(id) {
            return Err(NativeError::invalid(format!(
                "user ids must be between 1 and {MAX_USER_ID}"
            )));
        }
        if !set.insert(*id) {
            return Err(NativeError::invalid(format!(
                "`{what}` lists user {id} twice"
            )));
        }
    }
    Ok(set)
}

/// Passwd entries of `names` (missing names are absent from the map).
async fn passwd_entries(
    options: &NativeOptions,
    names: &[String],
) -> Result<HashMap<String, PasswdEntry>, NativeError> {
    let mut out = HashMap::new();
    for chunk in names.chunks(GETENT_BATCH) {
        let mut args = vec![OsString::from("passwd")];
        args.extend(chunk.iter().map(OsString::from));
        let output = run_command(
            &options.getent_bin,
            args,
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            64 * 1024,
        )
        .await?;
        // Exit status 2: some keys were not found; the others are printed.
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            if let Some(entry) = crate::native::identity::parse_getent_passwd(line)? {
                out.insert(entry.username.clone(), entry);
            }
        }
        if !output.success && output.stdout.trim().is_empty() && !output.stderr.trim().is_empty() {
            return Err(NativeError::command_failed(format!(
                "cannot look up users: {}",
                summarize_command(&output)
            )));
        }
    }
    Ok(out)
}

/// The RAST user id of an identity name (`rast-u<id>`).
fn user_id_of(name: &str) -> Option<i64> {
    let digits = name.strip_prefix("rast-u")?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse()
        .ok()
        .filter(|id| (1..=MAX_USER_ID).contains(id))
}

pub(crate) async fn share_members_set(
    options: &NativeOptions,
    op: ShareMembersSetOp,
) -> Result<Value, NativeError> {
    let groups = ShareGroups::for_share(&op.share_id)?;
    let write = member_list(&op.write, "write")?;
    let read = member_list(&op.read, "read")?;
    if let Some(id) = write.intersection(&read).next() {
        return Err(NativeError::invalid(format!(
            "user {id} is listed in both `write` and `read`"
        )));
    }
    let range = share_gid_range(options.uid_base)?;
    let rw = existing_share_group(options, &groups.rw, &range).await?;
    let ro = existing_share_group(options, &groups.ro, &range).await?;

    // Users without their Linux account yet cannot be members: they are
    // left out and reported, and added once the account exists. An account
    // with another uid than RAST derives is a conflict.
    let names: Vec<String> = write
        .iter()
        .chain(&read)
        .map(|id| username_for(*id))
        .collect();
    let entries = passwd_entries(options, &names).await?;
    let mut skipped = Vec::new();
    for id in write.iter().chain(&read) {
        let name = username_for(*id);
        let expected = crate::native::protocol::identity_ids(&options.validation_context(), *id)?;
        match entries.get(&name) {
            None => skipped.push(*id),
            Some(entry) if entry.uid != expected.uid => {
                return Err(NativeError::identity_conflict(format!(
                    "user {name} has uid {}, RAST expects {}",
                    entry.uid, expected.uid
                )))
            }
            Some(_) => {}
        }
    }
    let write: BTreeSet<i64> = write
        .into_iter()
        .filter(|id| !skipped.contains(id))
        .collect();
    let read: BTreeSet<i64> = read
        .into_iter()
        .filter(|id| !skipped.contains(id))
        .collect();
    skipped.sort_unstable();

    let mut report = serde_json::Map::new();
    for (key, group, wanted) in [("write", &rw, &write), ("read", &ro, &read)] {
        let current: BTreeSet<&str> = group.members.iter().map(String::as_str).collect();
        let wanted_names: BTreeSet<String> = wanted.iter().map(|id| username_for(*id)).collect();
        let added: Vec<i64> = wanted
            .iter()
            .filter(|id| !current.contains(username_for(**id).as_str()))
            .copied()
            .collect();
        let removed_names: Vec<&str> = current
            .iter()
            .filter(|name| !wanted_names.contains(**name))
            .copied()
            .collect();
        let removed: Vec<i64> = removed_names.iter().filter_map(|n| user_id_of(n)).collect();
        let removed_other = removed_names.len() - removed.len();
        if !added.is_empty() || !removed_names.is_empty() {
            let list = wanted_names.iter().cloned().collect::<Vec<_>>().join(",");
            let output = run_command(
                &options.gpasswd_bin,
                vec![
                    OsString::from("-M"),
                    OsString::from(list),
                    OsString::from(&group.name),
                ],
                &[],
                ACCOUNT_CMD_TIMEOUT,
                64 * 1024,
                64 * 1024,
            )
            .await?;
            if !output.success {
                return Err(NativeError::command_failed(format!(
                    "cannot set the members of {}: {}",
                    group.name,
                    summarize_command(&output)
                )));
            }
        }
        report.insert(
            key.to_string(),
            json!({
                "group": group.name,
                "added": added,
                "removed": removed,
                "removedOther": removed_other,
            }),
        );
    }
    report.insert("skipped".into(), json!(skipped));
    report.insert("shareId".into(), Value::String(op.share_id));
    Ok(Value::Object(report))
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn lstat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: zeroed `stat` is a valid out parameter.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd, NUL-terminated name, valid out pointer.
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: valid fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

const FILE_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;

// The field types of `struct stat` differ between architectures.
#[allow(clippy::unnecessary_cast)]
fn dev_of(stat: &libc::stat) -> u64 {
    stat.st_dev as u64
}

#[allow(clippy::unnecessary_cast)]
fn ino_of(stat: &libc::stat) -> u64 {
    stat.st_ino as u64
}

fn same_inode(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn is_regular(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// The names in the folder `dir` (without `.` and `..`); `None` when it
/// holds more than `limit`. Members fill shared folders, so a folder with
/// millions of names is refused before they fill root's memory.
fn dir_names(dir: RawFd, limit: u64) -> io::Result<Option<Vec<CString>>> {
    // A new open file description, so reading moves no shared offset.
    let own = open_at(dir, c".", DIR_FLAGS)?;
    // SAFETY: `own` is a valid directory fd; `fdopendir` takes it over and
    // `closedir` below closes it.
    let stream = unsafe { libc::fdopendir(own.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(own);
    let mut names = Vec::new();
    loop {
        clear_errno();
        // SAFETY: `stream` is a valid, open DIR stream.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `stream` is valid and closed exactly once.
            unsafe { libc::closedir(stream) };
            return match error.raw_os_error() {
                Some(0) | None => Ok(Some(names)),
                Some(_) => Err(error),
            };
        }
        // SAFETY: `d_name` of a returned entry is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            if names.len() as u64 >= limit {
                // SAFETY: `stream` is valid and closed exactly once.
                unsafe { libc::closedir(stream) };
                return Ok(None);
            }
            names.push(name.to_owned());
        }
    }
}

fn too_many_entries(limit: u64) -> NativeError {
    NativeError::forbidden(format!("the shared folder has more than {limit} entries"))
}

/// The owner of prepared containers, whose other entries walks leave
/// alone: root. Tests run as one unprivileged account that owns every test
/// file and cannot chown, so they use a uid that owns nothing.
fn root_uid(options: &NativeOptions) -> u32 {
    if options.is_test_mode() {
        u32::MAX - 15
    } else {
        0
    }
}

/// The owner of the journal folders: the helper itself (root).
fn journal_owner() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// The `rast` service uid (the owner of legacy containers).
fn service_uid(options: &NativeOptions) -> Result<u32, NativeError> {
    match options.rast_uid {
        Some(uid) => Ok(uid),
        // SAFETY: geteuid has no preconditions.
        None if options.is_test_mode() => Ok(unsafe { libc::geteuid() }),
        None => Err(NativeError::unavailable(
            "the rast service account does not exist; reinstall RAST",
        )),
    }
}

fn shared_root(options: &NativeOptions) -> PathBuf {
    options.storage_dir.join("shared")
}

/// Open `<storage>/shared/<container>`: a real folder (no link followed) on
/// the file system of the shared root.
fn open_container(
    options: &NativeOptions,
    container: &str,
) -> Result<(OwnedFd, libc::stat), NativeError> {
    validate_share_container(container)?;
    let root_path = shared_root(options);
    let root = open_dir(&root_path).map_err(|error| tree_error("open", &root_path, error))?;
    let root_stat =
        fstat(root.as_raw_fd()).map_err(|error| tree_error("inspect", &root_path, error))?;
    let name = c_name(container).map_err(|_| NativeError::invalid("invalid container"))?;
    let path = root_path.join(container);
    let stat =
        lstat_at(root.as_raw_fd(), &name).map_err(|error| tree_error("inspect", &path, error))?;
    if !is_dir(&stat) {
        return Err(NativeError::forbidden(format!(
            "{} is not a real folder",
            path.display()
        )));
    }
    let fd = open_at(root.as_raw_fd(), &name, DIR_FLAGS)
        .map_err(|error| tree_error("open", &path, error))?;
    let opened = fstat(fd.as_raw_fd()).map_err(|error| tree_error("inspect", &path, error))?;
    if !same_inode(&opened, &stat) || opened.st_dev != root_stat.st_dev {
        return Err(NativeError::forbidden(format!(
            "{} changed while it was opened, or is on another file system",
            path.display()
        )));
    }
    Ok((fd, opened))
}

fn tree_error(what: &str, path: &std::path::Path, error: io::Error) -> NativeError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => NativeError::not_found(format!("{} does not exist", path.display())),
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
            NativeError::forbidden(format!("{} is not a real folder", path.display()))
        }
        _ => NativeError::command_failed(format!("cannot {what} {}: {error}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Target state
// ---------------------------------------------------------------------------

/// What the entries of one shared folder must look like.
#[derive(Debug, Clone, Copy)]
struct Layout {
    rw_gid: u32,
    ro_gid: u32,
    rast_uid: u32,
    root_uid: u32,
    identity_uids: (u32, u32),
    test_mode: bool,
}

impl Layout {
    async fn for_share(options: &NativeOptions, share_id: &str) -> Result<Self, NativeError> {
        let groups = ShareGroups::for_share(share_id)?;
        let range = share_gid_range(options.uid_base)?;
        let rw = existing_share_group(options, &groups.rw, &range).await?;
        let ro = existing_share_group(options, &groups.ro, &range).await?;
        let identities = identity_uid_range(options.uid_base);
        Ok(Self {
            rw_gid: rw.gid,
            ro_gid: ro.gid,
            rast_uid: service_uid(options)?,
            root_uid: root_uid(options),
            identity_uids: (*identities.start(), *identities.end()),
            test_mode: options.is_test_mode(),
        })
    }

    fn acl(&self, owner: u16, group: u16) -> Acl {
        let read = (group & 1) | 4;
        Acl::new(vec![
            Entry {
                tag: Tag::UserObj,
                perm: owner,
            },
            Entry {
                tag: Tag::User(self.rast_uid),
                perm: group,
            },
            Entry {
                tag: Tag::GroupObj,
                perm: group,
            },
            Entry {
                tag: Tag::Group(self.ro_gid),
                perm: read,
            },
            Entry {
                tag: Tag::Mask,
                perm: group,
            },
            Entry {
                tag: Tag::Other,
                perm: 0,
            },
        ])
    }

    /// Mode and access ACL of a folder (the container: `container`), and
    /// its default ACL.
    fn dir_target(&self, mode: u32, container: bool) -> (u32, Acl, Acl) {
        let owner = if container { 7 } else { (mode >> 6) & 7 };
        let target_mode = (owner << 6) | 0o2070;
        (target_mode, self.acl(owner as u16, 7), self.acl(7, 7))
    }

    /// Mode and access ACL of a regular file: read-write for the group,
    /// executable if the owner may run it; never set-uid or set-gid.
    fn file_target(&self, mode: u32) -> (u32, Acl) {
        let owner = (mode >> 6) & 7;
        let group = 6 | (owner & 1);
        (
            (owner << 6) | (group << 3),
            self.acl(owner as u16, group as u16),
        )
    }

    fn is_identity(&self, uid: u32) -> bool {
        (self.identity_uids.0..=self.identity_uids.1).contains(&uid)
    }

    fn owners(&self) -> Owners {
        Owners {
            root_uid: self.root_uid,
            rast_uid: self.rast_uid,
            identity_uids: self.identity_uids,
        }
    }
}

/// The owners of the entries a walk may hand to its visitor.
#[derive(Debug, Clone, Copy)]
struct Owners {
    /// Root: its entries (and the prepared container) are left alone.
    root_uid: u32,
    /// The `rast` service, which owns every entry of a legacy share.
    rast_uid: u32,
    identity_uids: (u32, u32),
}

impl Owners {
    fn of(options: &NativeOptions) -> Result<Self, NativeError> {
        let identities = identity_uid_range(options.uid_base);
        Ok(Self {
            root_uid: root_uid(options),
            rast_uid: service_uid(options)?,
            identity_uids: (*identities.start(), *identities.end()),
        })
    }

    /// An entry of RAST's own accounts: the service or a RAST identity.
    fn managed(&self, uid: u32) -> bool {
        uid == self.rast_uid || (self.identity_uids.0..=self.identity_uids.1).contains(&uid)
    }
}

/// What an entry is now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Current {
    gid: u32,
    mode: u32,
    access: Option<Vec<u8>>,
    default: Option<Vec<u8>>,
}

fn current(fd: RawFd, stat: &libc::stat, dir: bool) -> io::Result<Current> {
    Ok(Current {
        gid: stat.st_gid,
        mode: stat.st_mode & 0o7777,
        access: acl::read_raw(fd, acl::ACCESS_XATTR)?,
        default: if dir {
            acl::read_raw(fd, acl::DEFAULT_XATTR)?
        } else {
            None
        },
    })
}

/// The target of an entry; `container` also sets the owner to root.
struct Target {
    uid: Option<u32>,
    gid: u32,
    mode: u32,
    access: Acl,
    default: Option<Acl>,
}

impl Target {
    fn of(layout: &Layout, stat: &libc::stat, container: bool) -> Self {
        let mode = stat.st_mode & 0o7777;
        if is_dir(stat) {
            let (mode, access, default) = layout.dir_target(mode, container);
            Self {
                // Tests cannot chown (and skip it).
                uid: container
                    .then_some(layout.root_uid)
                    .filter(|_| !layout.test_mode),
                gid: layout.rw_gid,
                mode,
                access,
                default: Some(default),
            }
        } else {
            let (mode, access) = layout.file_target(mode);
            Self {
                uid: None,
                gid: layout.rw_gid,
                mode,
                access,
                default: None,
            }
        }
    }

    fn matches(&self, stat: &libc::stat, now: &Current, test_mode: bool) -> bool {
        self.uid.is_none_or(|uid| uid == stat.st_uid)
            && (test_mode || now.gid == self.gid)
            && now.mode == self.mode
            && now.access.as_deref() == Some(self.access.encode().as_slice())
            && match &self.default {
                Some(default) => now.default.as_deref() == Some(default.encode().as_slice()),
                None => true,
            }
    }

    /// Apply through the open descriptor `fd`.
    fn apply(&self, fd: RawFd, test_mode: bool) -> io::Result<()> {
        if !test_mode {
            let uid = self.uid.unwrap_or(u32::MAX);
            // SAFETY: valid fd; (uid_t)-1 leaves the owner unchanged.
            if unsafe { libc::fchown(fd, uid, self.gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: valid fd.
        if unsafe { libc::fchmod(fd, self.mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        acl::write(fd, acl::ACCESS_XATTR, &self.access)?;
        if let Some(default) = &self.default {
            acl::write(fd, acl::DEFAULT_XATTR, default)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Walking a container
// ---------------------------------------------------------------------------

/// A count and the first paths of one kind of entry.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Sample {
    pub count: u64,
    pub paths: Vec<String>,
}

impl Sample {
    fn add(&mut self, path: &str) {
        self.count += 1;
        if self.paths.len() < REPORT_SAMPLE {
            self.paths.push(path.to_string());
        }
    }
}

/// What a walk found (and, for apply and rollback, did).
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeReport {
    pub dirs: u64,
    pub files: u64,
    pub bytes: u64,
    /// Entries (the container included) that are or were changed.
    pub changes: Sample,
    pub symlinks: Sample,
    pub hard_links: Sample,
    pub root_owned: Sample,
    /// Entries owned by an account other than the `rast` service and the
    /// RAST identities (left unchanged).
    pub other_owner: Sample,
    pub other_device: Sample,
    pub special: Sample,
    pub non_utf8: u64,
    /// Entries with named-user ACL entries of RAST identities (terminal
    /// grants), which the new ACL replaces.
    pub identity_acl_entries: Sample,
    /// Entries whose ACLs hold named entries that RAST does not manage
    /// (neither `u:rast`, the read-only group nor a RAST identity), which
    /// the new ACL removes. Each path is followed by those entries, for
    /// example `docs/plan.txt: u:1001:rw-, default g:2000:r-x`.
    pub foreign_acl_entries: Sample,
    /// Entries that vanished or were swapped while the walk ran.
    pub changed_during_walk: Sample,
    /// The container already had (before this run) its target owner,
    /// group, mode and ACLs.
    pub container_prepared: bool,
}

/// One entry the walk hands to its visitor: a folder (opened and checked)
/// or a regular file with a single link, owned by the `rast` service or a
/// RAST identity, on the container's file system.
struct Found<'a> {
    parent: RawFd,
    name: &'a CStr,
    path: &'a str,
    stat: &'a libc::stat,
    /// The open folder (folders only).
    dir: Option<&'a OwnedFd>,
}

/// Walk the tree below `container` depth first. Entries that are not
/// handed to `visit` are counted in `report`.
fn walk(
    container: &OwnedFd,
    owners: &Owners,
    report: &mut TreeReport,
    visit: impl FnMut(&Found<'_>, &mut TreeReport) -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    walk_limited(container, owners, MAX_TREE_ENTRIES, report, visit)
}

/// [`walk`] of at most `limit` entries. Every name read is visited or
/// counted, so the names read so far (visited plus waiting in the open
/// folders) never exceed `limit` either.
fn walk_limited(
    container: &OwnedFd,
    owners: &Owners,
    limit: u64,
    report: &mut TreeReport,
    mut visit: impl FnMut(&Found<'_>, &mut TreeReport) -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    let device = fstat(container.as_raw_fd())
        .map_err(|error| {
            NativeError::command_failed(format!("cannot inspect the container: {error}"))
        })?
        .st_dev;
    struct Frame {
        dir: OwnedFd,
        path: String,
        names: std::vec::IntoIter<CString>,
    }
    let mut read = 0u64;
    let mut names = |fd: &OwnedFd| {
        let names = dir_names(fd.as_raw_fd(), limit.saturating_sub(read))
            .map_err(|error| NativeError::command_failed(format!("cannot read a folder: {error}")))?
            .ok_or_else(|| too_many_entries(limit))?;
        read += names.len() as u64;
        Ok::<_, NativeError>(names)
    };
    let first = open_at(container.as_raw_fd(), c".", DIR_FLAGS).map_err(|error| {
        NativeError::command_failed(format!("cannot open the container: {error}"))
    })?;
    let mut stack = vec![Frame {
        names: names(&first)?.into_iter(),
        dir: first,
        path: String::new(),
    }];
    let mut seen = 0u64;
    while let Some(frame) = stack.last_mut() {
        let Some(name) = frame.names.next() else {
            stack.pop();
            continue;
        };
        seen += 1;
        if seen > limit {
            return Err(too_many_entries(limit));
        }
        let Ok(text) = name.to_str() else {
            report.non_utf8 += 1;
            continue;
        };
        let path = if frame.path.is_empty() {
            text.to_string()
        } else {
            format!("{}/{text}", frame.path)
        };
        let parent = frame.dir.as_raw_fd();
        let stat = match lstat_at(parent, &name) {
            Ok(stat) => stat,
            Err(_) => {
                report.changed_during_walk.add(&path);
                continue;
            }
        };
        if is_symlink(&stat) {
            report.symlinks.add(&path);
            continue;
        }
        if stat.st_dev != device {
            report.other_device.add(&path);
            continue;
        }
        if is_dir(&stat) {
            let dir = match open_at(parent, &name, DIR_FLAGS) {
                Ok(dir) => dir,
                Err(_) => {
                    report.changed_during_walk.add(&path);
                    continue;
                }
            };
            match fstat(dir.as_raw_fd()) {
                Ok(opened) if same_inode(&opened, &stat) => {}
                _ => {
                    report.changed_during_walk.add(&path);
                    continue;
                }
            }
            report.dirs += 1;
            if stat.st_uid == owners.root_uid {
                report.root_owned.add(&path);
            } else if !owners.managed(stat.st_uid) {
                report.other_owner.add(&path);
            } else {
                visit(
                    &Found {
                        parent,
                        name: &name,
                        path: &path,
                        stat: &stat,
                        dir: Some(&dir),
                    },
                    report,
                )?;
            }
            if stack.len() >= MAX_DEPTH {
                return Err(NativeError::forbidden(format!(
                    "the shared folder is nested deeper than {MAX_DEPTH} levels"
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
            report.special.add(&path);
            continue;
        }
        report.files += 1;
        report.bytes = report.bytes.saturating_add(stat.st_size.max(0) as u64);
        if stat.st_nlink > 1 {
            report.hard_links.add(&path);
            continue;
        }
        if stat.st_uid == owners.root_uid {
            report.root_owned.add(&path);
            continue;
        }
        if !owners.managed(stat.st_uid) {
            report.other_owner.add(&path);
            continue;
        }
        visit(
            &Found {
                parent,
                name: &name,
                path: &path,
                stat: &stat,
                dir: None,
            },
            report,
        )?;
    }
    Ok(())
}

/// Open the regular file of `found` and check that it is still that file,
/// with one link and the owner the walk checked.
fn open_found_file(found: &Found<'_>) -> Option<(OwnedFd, libc::stat)> {
    let fd = open_at(found.parent, found.name, FILE_FLAGS).ok()?;
    let stat = fstat(fd.as_raw_fd()).ok()?;
    (is_regular(&stat)
        && same_inode(&stat, found.stat)
        && stat.st_nlink == 1
        && stat.st_uid == found.stat.st_uid)
        .then_some((fd, stat))
}

/// Whether an ACL grants named entries to RAST identities (with
/// `skip_denials`, `---` entries do not count: on a container they are the
/// deny entries of `share_deny_set`).
fn has_identity_entries(layout: &Layout, raw: Option<&[u8]>, skip_denials: bool) -> bool {
    raw.and_then(Acl::decode).is_some_and(|acl| {
        acl.named_users()
            .any(|(uid, perm)| layout.is_identity(uid) && !(skip_denials && perm == 0))
    })
}

/// The deny entries (`u:<identity>:---`) of an ACL.
fn denials(layout: &Layout, acl: &Acl) -> Vec<Entry> {
    acl.0
        .iter()
        .filter(|entry| {
            entry.perm == 0 && matches!(entry.tag, Tag::User(uid) if layout.is_identity(uid))
        })
        .copied()
        .collect()
}

/// The named entries of an ACL that the layout does not manage, in
/// `getfacl` notation (`prefix` marks the default ACL).
fn foreign_entries(layout: &Layout, raw: Option<&[u8]>, prefix: &str) -> Vec<String> {
    let Some(acl) = raw.and_then(Acl::decode) else {
        return Vec::new();
    };
    acl.0
        .iter()
        .filter(|entry| match entry.tag {
            Tag::User(uid) => uid != layout.rast_uid && !layout.is_identity(uid),
            Tag::Group(gid) => gid != layout.ro_gid,
            _ => false,
        })
        .map(|entry| format!("{prefix}{}", Acl(vec![*entry]).describe()))
        .collect()
}

/// Inspect (and with `apply`, change) one entry.
fn handle_entry(
    layout: &Layout,
    fd: RawFd,
    stat: &libc::stat,
    container: bool,
    path: &str,
    apply: bool,
    report: &mut TreeReport,
) -> Result<(), NativeError> {
    let dir = is_dir(stat);
    let now = current(fd, stat, dir).map_err(|error| {
        NativeError::command_failed(format!("cannot read the ACL of {}: {error}", show(path)))
    })?;
    if has_identity_entries(layout, now.access.as_deref(), container)
        || has_identity_entries(layout, now.default.as_deref(), false)
    {
        report.identity_acl_entries.add(show(path));
    }
    let mut foreign = foreign_entries(layout, now.access.as_deref(), "");
    foreign.extend(foreign_entries(layout, now.default.as_deref(), "default "));
    if !foreign.is_empty() {
        report
            .foreign_acl_entries
            .add(&format!("{}: {}", show(path), foreign.join(", ")));
    }
    let mut target = Target::of(layout, stat, container);
    // The container keeps the deny entries of pending restarts.
    if container {
        let kept = now
            .access
            .as_deref()
            .and_then(Acl::decode)
            .map(|acl| denials(layout, &acl))
            .unwrap_or_default();
        if !kept.is_empty() {
            target.access = Acl::new(target.access.0.iter().copied().chain(kept).collect());
        }
    }
    if target.matches(stat, &now, layout.test_mode) {
        return Ok(());
    }
    report.changes.add(show(path));
    if apply {
        target.apply(fd, layout.test_mode).map_err(|error| {
            NativeError::command_failed(format!("cannot change {}: {error}", show(path)))
        })?;
    }
    Ok(())
}

fn show(path: &str) -> &str {
    if path.is_empty() {
        "."
    } else {
        path
    }
}

/// Inspect or change every entry below the container, then the container.
fn prepare_tree(
    layout: &Layout,
    container: &OwnedFd,
    container_stat: &libc::stat,
    recursive: bool,
    apply: bool,
) -> Result<TreeReport, NativeError> {
    let mut report = TreeReport::default();
    if recursive {
        walk(
            container,
            &layout.owners(),
            &mut report,
            |found, report| match found.dir {
                Some(dir) => handle_entry(
                    layout,
                    dir.as_raw_fd(),
                    found.stat,
                    false,
                    found.path,
                    apply,
                    report,
                ),
                None => match open_found_file(found) {
                    Some((fd, stat)) => handle_entry(
                        layout,
                        fd.as_raw_fd(),
                        &stat,
                        false,
                        found.path,
                        apply,
                        report,
                    ),
                    None => {
                        report.changed_during_walk.add(found.path);
                        Ok(())
                    }
                },
            },
        )?;
    }
    let before = report.changes.count;
    handle_entry(
        layout,
        container.as_raw_fd(),
        container_stat,
        true,
        "",
        apply,
        &mut report,
    )?;
    report.container_prepared = report.changes.count == before;
    Ok(report)
}

/// The container of a share, which must be a legacy container (owned by
/// the service) or one already prepared (owned by root).
fn checked_container(
    options: &NativeOptions,
    container: &str,
    layout: &Layout,
) -> Result<(OwnedFd, libc::stat), NativeError> {
    let (fd, stat) = open_container(options, container)?;
    if stat.st_uid != layout.rast_uid && stat.st_uid != layout.root_uid {
        return Err(NativeError::identity_conflict(format!(
            "the container {container} is owned by uid {}, not by the RAST service or root",
            stat.st_uid
        )));
    }
    Ok((fd, stat))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, NativeError> + Send + 'static,
) -> Result<T, NativeError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        NativeError::command_failed(format!("shared folder task failed: {error}"))
    })?
}

pub(crate) async fn share_dir_prepare(
    options: &NativeOptions,
    op: ShareDirPrepareOp,
) -> Result<Value, NativeError> {
    validate_share_container(&op.container)?;
    let layout = Layout::for_share(options, &op.share_id).await?;
    let options = options.clone();
    let container = op.container.clone();
    let recursive = op.recursive;
    let report = blocking(move || {
        let (fd, stat) = checked_container(&options, &container, &layout)?;
        prepare_tree(&layout, &fd, &stat, recursive, true)
    })
    .await?;
    tracing::info!(
        share = %op.share_id,
        recursive,
        changes = report.changes.count,
        "shared folder prepared"
    );
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "recursive": recursive,
        "report": report,
    }))
}

pub(crate) async fn share_dir_remove(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_id(&op.share_id)?;
    validate_share_container(&op.container)?;
    let options = options.clone();
    let container = op.container.clone();
    let removed = blocking(move || {
        let root_path = shared_root(&options);
        let root = open_dir(&root_path).map_err(|error| tree_error("open", &root_path, error))?;
        let name = c_name(&container).map_err(|_| NativeError::invalid("invalid container"))?;
        match open_container(&options, &container) {
            Ok((fd, stat)) => {
                // Only a container of RAST: the service's or a prepared one.
                if stat.st_uid != service_uid(&options)? && stat.st_uid != root_uid(&options) {
                    return Err(NativeError::identity_conflict(format!(
                        "the container {container} is owned by uid {}, not by the RAST service \
                         or root",
                        stat.st_uid
                    )));
                }
                remove_contents(&fd, 0, &mut 0)?;
                drop(fd);
                // SAFETY: valid fd and NUL-terminated name.
                if unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                    != 0
                {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ENOENT) {
                        return Err(tree_error("remove", &root_path.join(&container), error));
                    }
                }
                Ok(true)
            }
            Err(error) if error.code == "not_found" => Ok(false),
            Err(error) => Err(error),
        }
    })
    .await?;
    Ok(json!({"shareId": op.share_id, "container": op.container, "removed": removed}))
}

/// Close the container of a deleted shared folder (whose data RAST keeps)
/// before its groups are removed: back to the service (`rast:rast` 0700, no
/// ACL). Otherwise a later share whose group gets the freed gid would open
/// it. Only the container changes; nothing below it can be reached without
/// passing it.
pub(crate) async fn share_dir_release(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_id(&op.share_id)?;
    validate_share_container(&op.container)?;
    let rast_uid = service_uid(options)?;
    let rast_gid = options.effective_rast_gid()?;
    let options = options.clone();
    let container = op.container.clone();
    let released = blocking(move || {
        let (fd, stat) = match open_container(&options, &container) {
            Ok(found) => found,
            Err(error) if error.code == "not_found" => return Ok(false),
            Err(error) => return Err(error),
        };
        if stat.st_uid != rast_uid && stat.st_uid != root_uid(&options) {
            return Err(NativeError::identity_conflict(format!(
                "the container {container} is owned by uid {}, not by the RAST service or root",
                stat.st_uid
            )));
        }
        let failed = |error: io::Error| {
            NativeError::command_failed(format!("cannot close the container {container}: {error}"))
        };
        if !options.is_test_mode() {
            // SAFETY: valid fd.
            if unsafe { libc::fchown(fd.as_raw_fd(), rast_uid, rast_gid) } != 0 {
                return Err(failed(io::Error::last_os_error()));
            }
        }
        acl::remove(fd.as_raw_fd(), acl::DEFAULT_XATTR).map_err(failed)?;
        acl::remove(fd.as_raw_fd(), acl::ACCESS_XATTR).map_err(failed)?;
        // SAFETY: valid fd.
        if unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
            return Err(failed(io::Error::last_os_error()));
        }
        Ok(true)
    })
    .await?;
    Ok(json!({"shareId": op.share_id, "container": op.container, "released": released}))
}

/// Remove everything in the folder `dir`, never following a link and never
/// leaving its file system. `pending` counts the names read but not yet
/// removed in the folders above (at most [`MAX_TREE_ENTRIES`] in all).
fn remove_contents(dir: &OwnedFd, depth: usize, pending: &mut u64) -> Result<(), NativeError> {
    if depth >= MAX_DEPTH {
        return Err(NativeError::forbidden(
            "the shared folder is nested too deeply",
        ));
    }
    let device = fstat(dir.as_raw_fd())
        .map_err(|error| NativeError::command_failed(format!("cannot inspect a folder: {error}")))?
        .st_dev;
    let names = dir_names(dir.as_raw_fd(), MAX_TREE_ENTRIES.saturating_sub(*pending))
        .map_err(|error| NativeError::command_failed(format!("cannot read a folder: {error}")))?
        .ok_or_else(|| too_many_entries(MAX_TREE_ENTRIES))?;
    *pending += names.len() as u64;
    for name in names {
        *pending -= 1;
        let Ok(stat) = lstat_at(dir.as_raw_fd(), &name) else {
            continue;
        };
        let flags = if is_dir(&stat) {
            if stat.st_dev != device {
                return Err(NativeError::forbidden(
                    "the shared folder contains another file system; unmount it first",
                ));
            }
            let child = open_at(dir.as_raw_fd(), &name, DIR_FLAGS).map_err(|error| {
                NativeError::command_failed(format!("cannot open a folder: {error}"))
            })?;
            let opened = fstat(child.as_raw_fd())
                .map_err(|error| NativeError::command_failed(format!("cannot inspect: {error}")))?;
            if !same_inode(&opened, &stat) {
                return Err(NativeError::forbidden(
                    "a folder changed while it was removed",
                ));
            }
            remove_contents(&child, depth + 1, pending)?;
            libc::AT_REMOVEDIR
        } else {
            0
        };
        // SAFETY: valid fd and NUL-terminated name.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(NativeError::command_failed(format!(
                    "cannot remove an entry of the shared folder: {error}"
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Repair, deny entries and the groups of running processes
// ---------------------------------------------------------------------------

/// Whether a container has the group layout: it is owned by root. (Tests
/// cannot chown: there it carries the read-only group entry.)
fn is_prepared(layout: &Layout, fd: RawFd, stat: &libc::stat) -> bool {
    if stat.st_uid == layout.root_uid {
        return true;
    }
    layout.test_mode
        && acl::read(fd, acl::ACCESS_XATTR)
            .ok()
            .flatten()
            .is_some_and(|acl| acl.named_groups().any(|(gid, _)| gid == layout.ro_gid))
}

pub(crate) async fn share_dir_repair(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_container(&op.container)?;
    let layout = Layout::for_share(options, &op.share_id).await?;
    let options = options.clone();
    let container = op.container.clone();
    let report = blocking(move || {
        let (fd, stat) = checked_container(&options, &container, &layout)?;
        if !is_prepared(&layout, fd.as_raw_fd(), &stat) {
            return Ok(None);
        }
        prepare_tree(&layout, &fd, &stat, true, true).map(Some)
    })
    .await?;
    let Some(report) = report else {
        return Ok(json!({
            "shareId": op.share_id,
            "container": op.container,
            "repaired": false,
            "reason": "not_migrated",
        }));
    };
    tracing::info!(
        share = %op.share_id,
        changes = report.changes.count,
        foreign_acl = report.foreign_acl_entries.count,
        "shared folder repaired"
    );
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "repaired": true,
        "report": report,
    }))
}

pub(crate) async fn share_deny_set(
    options: &NativeOptions,
    op: ShareDenySetOp,
) -> Result<Value, NativeError> {
    validate_share_container(&op.container)?;
    if op.user_ids.len() > MAX_SHARE_DENIALS {
        return Err(NativeError::invalid(format!(
            "`userIds` lists more than {MAX_SHARE_DENIALS} users"
        )));
    }
    let ids = member_list(&op.user_ids, "userIds")?;
    let ctx = options.validation_context();
    let uids = ids
        .iter()
        .map(|id| identity_ids(&ctx, *id).map(|ids| ids.uid))
        .collect::<Result<BTreeSet<u32>, _>>()?;
    let layout = Layout::for_share(options, &op.share_id).await?;
    let options = options.clone();
    let container = op.container.clone();
    let (applied, changed) = blocking(move || {
        let (fd, stat) = checked_container(&options, &container, &layout)?;
        if !is_prepared(&layout, fd.as_raw_fd(), &stat) {
            return Ok((false, false));
        }
        let failed = |error: io::Error| {
            NativeError::command_failed(format!(
                "cannot set the deny entries of {container}: {error}"
            ))
        };
        let Some(current) = acl::read(fd.as_raw_fd(), acl::ACCESS_XATTR).map_err(failed)? else {
            return Ok((false, false));
        };
        let old: BTreeSet<Entry> = denials(&layout, &current).into_iter().collect();
        let mut entries: Vec<Entry> = current
            .0
            .iter()
            .filter(|entry| !old.contains(entry))
            .filter(|entry| !matches!(entry.tag, Tag::User(uid) if uids.contains(&uid)))
            .copied()
            .collect();
        entries.extend(uids.iter().map(|uid| Entry {
            tag: Tag::User(*uid),
            perm: 0,
        }));
        let wanted = Acl::new(entries);
        if wanted == current {
            return Ok((true, false));
        }
        acl::write(fd.as_raw_fd(), acl::ACCESS_XATTR, &wanted).map_err(failed)?;
        Ok((true, true))
    })
    .await?;
    if changed {
        tracing::info!(share = %op.share_id, users = ?ids, "shared folder deny entries set");
    }
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "applied": applied,
        "changed": changed,
        "userIds": ids,
    }))
}

/// Processes `share_session_groups` reads at most.
const MAX_PROCESSES: usize = 1 << 20;
/// Bytes of one `/proc/<pid>/status` read at most.
const MAX_STATUS_BYTES: u64 = 64 * 1024;

/// The effective uid and the supplementary groups of a process, from its
/// `/proc/<pid>/status`.
pub fn parse_proc_status(text: &str) -> Option<(u32, Vec<u32>)> {
    let mut euid = None;
    let mut groups = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // Real, effective, saved set and file system uid.
            euid = rest.split_whitespace().nth(1)?.parse().ok();
        } else if let Some(rest) = line.strip_prefix("Groups:") {
            groups = rest
                .split_whitespace()
                .map(|gid| gid.parse().ok())
                .collect::<Option<Vec<u32>>>();
        }
    }
    Some((euid?, groups?))
}

fn proc_dir(options: &NativeOptions) -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = &options.test_proc_dir {
        return dir.clone();
    }
    let _ = options;
    PathBuf::from("/proc")
}

/// Per user: how many processes run with the identity's uid, and which gids
/// from `range` they carry.
fn scan_processes(
    proc: &std::path::Path,
    users: &HashMap<u32, i64>,
    range: &RangeInclusive<u32>,
) -> Result<BTreeMap<i64, (u64, BTreeSet<u32>)>, NativeError> {
    use std::io::Read;
    let entries = std::fs::read_dir(proc).map_err(|error| {
        NativeError::command_failed(format!("cannot list {}: {error}", proc.display()))
    })?;
    let mut out: BTreeMap<i64, (u64, BTreeSet<u32>)> = BTreeMap::new();
    for entry in entries.take(MAX_PROCESSES) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that ended meanwhile is skipped.
        let Ok(file) = std::fs::File::open(entry.path().join("status")) else {
            continue;
        };
        let mut text = String::new();
        if file
            .take(MAX_STATUS_BYTES)
            .read_to_string(&mut text)
            .is_err()
        {
            continue;
        }
        let Some((euid, groups)) = parse_proc_status(&text) else {
            continue;
        };
        let Some(user_id) = users.get(&euid) else {
            continue;
        };
        let slot = out.entry(*user_id).or_default();
        slot.0 += 1;
        slot.1
            .extend(groups.into_iter().filter(|gid| range.contains(gid)));
    }
    Ok(out)
}

/// `rast-s<16 hex>-rw` or `rast-s<16 hex>-ro`.
fn is_share_group_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("rast-s") else {
        return false;
    };
    let Some(hex) = rest
        .strip_suffix("-rw")
        .or_else(|| rest.strip_suffix("-ro"))
    else {
        return false;
    };
    hex.len() == 16
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The share group names of `gids` (gids without a share group are left
/// out).
async fn share_group_names(
    options: &NativeOptions,
    gids: &BTreeSet<u32>,
    range: &RangeInclusive<u32>,
) -> Result<BTreeMap<u32, String>, NativeError> {
    let mut out = BTreeMap::new();
    let keys: Vec<String> = gids.iter().map(u32::to_string).collect();
    for chunk in keys.chunks(GETENT_BATCH) {
        let mut args = vec![OsString::from("group")];
        args.extend(chunk.iter().map(OsString::from));
        let output = run_command(
            &options.getent_bin,
            args,
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            64 * 1024,
        )
        .await?;
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let entry = parse_group_line(line)?;
            if gids.contains(&entry.gid)
                && range.contains(&entry.gid)
                && is_share_group_name(&entry.name)
            {
                out.insert(entry.gid, entry.name);
            }
        }
        // Exit status 2: some keys were not found; the others are printed.
        if !output.success && output.stdout.trim().is_empty() && !output.stderr.trim().is_empty() {
            return Err(NativeError::command_failed(format!(
                "cannot look up groups: {}",
                summarize_command(&output)
            )));
        }
    }
    Ok(out)
}

pub(crate) async fn share_session_groups(
    options: &NativeOptions,
    op: ShareSessionGroupsOp,
) -> Result<Value, NativeError> {
    if op.user_ids.is_empty() || op.user_ids.len() > MAX_SESSION_GROUP_USERS {
        return Err(NativeError::invalid(format!(
            "`userIds` must list 1 to {MAX_SESSION_GROUP_USERS} users"
        )));
    }
    let ids = member_list(&op.user_ids, "userIds")?;
    let ctx = options.validation_context();
    let mut users = HashMap::new();
    for id in &ids {
        users.insert(identity_ids(&ctx, *id)?.uid, *id);
    }
    let range = share_gid_range(options.uid_base)?;
    let proc = proc_dir(options);
    let scan_range = range.clone();
    let found = blocking(move || scan_processes(&proc, &users, &scan_range)).await?;
    let gids: BTreeSet<u32> = found
        .values()
        .flat_map(|(_, gids)| gids.iter().copied())
        .collect();
    let names = if gids.is_empty() {
        BTreeMap::new()
    } else {
        share_group_names(options, &gids, &range).await?
    };
    let users: Vec<Value> = found
        .into_iter()
        .map(|(user_id, (processes, gids))| {
            json!({"userId": user_id, "processes": processes, "gids": gids})
        })
        .collect();
    let groups: serde_json::Map<String, Value> = names
        .into_iter()
        .map(|(gid, name)| (gid.to_string(), Value::String(name)))
        .collect();
    Ok(json!({"users": users, "groups": groups}))
}

// ---------------------------------------------------------------------------
// rast migrate shares: journal, apply, rollback
// ---------------------------------------------------------------------------

/// One journal line: an entry as it was before the migration. `p` is the
/// path below the container (`""`: the container itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub p: String,
    /// A folder.
    pub d: bool,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    /// The access and default ACL xattrs, hex-encoded (`None`: not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dacl: Option<String>,
}

fn record_of(path: &str, fd: RawFd, stat: &libc::stat) -> io::Result<Record> {
    let dir = is_dir(stat);
    let now = current(fd, stat, dir)?;
    Ok(Record {
        p: path.to_string(),
        d: dir,
        dev: dev_of(stat),
        ino: ino_of(stat),
        uid: stat.st_uid,
        gid: stat.st_gid,
        mode: now.mode,
        acl: now.access.map(hex::encode),
        dacl: now.default.map(hex::encode),
    })
}

/// A relative path of a journal: names joined by `/`, none empty, `.` or
/// `..`, no NUL.
fn valid_relative(path: &str) -> bool {
    path.is_empty()
        || path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
}

impl Record {
    /// A journal is read back as untrusted input: only values that the
    /// migration could have recorded for a shared folder are restored.
    fn validate(&self, rast_uid: u32) -> Result<(), String> {
        if !valid_relative(&self.p) || self.p.len() > 64 * 1024 {
            return Err("an invalid path".into());
        }
        if self.uid == 0 || self.gid == 0 {
            return Err(format!("{}: owner or group root", show(&self.p)));
        }
        if self.p.is_empty() && (!self.d || self.uid != rast_uid) {
            return Err("the container was not a folder of the RAST service".into());
        }
        if self.mode & !0o7777 != 0 {
            return Err(format!("{}: an invalid mode", show(&self.p)));
        }
        if !self.d && self.dacl.is_some() {
            return Err(format!("{}: a default ACL on a file", show(&self.p)));
        }
        for value in [&self.acl, &self.dacl].into_iter().flatten() {
            let raw =
                hex::decode(value).map_err(|_| format!("{}: an invalid ACL", show(&self.p)))?;
            if Acl::decode(&raw).is_none() {
                return Err(format!("{}: an invalid ACL", show(&self.p)));
            }
        }
        Ok(())
    }

    /// The mode a rollback sets: never set-uid, and set-gid only on
    /// folders (where it only makes new entries inherit the group).
    fn restored_mode(&self) -> u32 {
        if self.d {
            self.mode & 0o3777
        } else {
            self.mode & 0o1777
        }
    }

    /// Restore through the open descriptor `fd`.
    fn restore(&self, fd: RawFd, test_mode: bool) -> io::Result<()> {
        if !test_mode {
            let uid = if self.p.is_empty() {
                self.uid
            } else {
                u32::MAX
            };
            // SAFETY: valid fd; (uid_t)-1 leaves the owner unchanged.
            if unsafe { libc::fchown(fd, uid, self.gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        match &self.acl {
            Some(value) => acl::write_raw(
                fd,
                acl::ACCESS_XATTR,
                &hex::decode(value).unwrap_or_default(),
            )?,
            None => acl::remove(fd, acl::ACCESS_XATTR)?,
        }
        if self.d {
            match &self.dacl {
                Some(value) => acl::write_raw(
                    fd,
                    acl::DEFAULT_XATTR,
                    &hex::decode(value).unwrap_or_default(),
                )?,
                None => acl::remove(fd, acl::DEFAULT_XATTR)?,
            }
        }
        // The mode last: setting an ACL rewrites the group bits.
        // SAFETY: valid fd.
        if unsafe { libc::fchmod(fd, self.restored_mode() as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Open (creating with `create`) the folders from the data directory down
/// to `<data_dir>/migrations/shares/<shareId>`. Every folder below the
/// data directory must belong to the helper (root) and be closed to
/// everyone else: the data directory belongs to the `rast` service, which
/// could otherwise plant a journal of its own.
fn open_journal_root(
    options: &NativeOptions,
    share_id: &str,
    create: bool,
) -> Result<Option<OwnedFd>, NativeError> {
    let data = &options.data_dir;
    let mut dir = open_dir(data).map_err(|error| tree_error("open", data, error))?;
    let owner = journal_owner();
    let mut path = data.clone();
    for part in ["migrations", "shares", share_id] {
        path.push(part);
        let name = c_name(part).map_err(|_| NativeError::invalid("invalid share id"))?;
        match lstat_at(dir.as_raw_fd(), &name) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) && create => {
                // SAFETY: valid fd and NUL-terminated name.
                if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(tree_error("create", &path, error));
                    }
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => return Err(tree_error("inspect", &path, error)),
        }
        let next = open_at(dir.as_raw_fd(), &name, DIR_FLAGS)
            .map_err(|error| tree_error("open", &path, error))?;
        let stat = fstat(next.as_raw_fd()).map_err(|error| tree_error("inspect", &path, error))?;
        if stat.st_uid != owner || stat.st_mode & 0o077 != 0 {
            return Err(NativeError::forbidden(format!(
                "{} must be a folder of root with mode 0700",
                path.display()
            )));
        }
        dir = next;
    }
    Ok(Some(dir))
}

/// The newest journal of a share that was not rolled back: its folder name
/// and descriptor.
fn active_journal(
    options: &NativeOptions,
    share_id: &str,
) -> Result<Option<(String, OwnedFd)>, NativeError> {
    let Some(root) = open_journal_root(options, share_id, false)? else {
        return Ok(None);
    };
    let names = dir_names(root.as_raw_fd(), MAX_TREE_ENTRIES)
        .map_err(|error| NativeError::command_failed(format!("cannot read the journals: {error}")))?
        .ok_or_else(|| NativeError::forbidden("too many shares journals"))?;
    let mut stamps: Vec<String> = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.len() == 12 && name.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    stamps.sort();
    let owner = journal_owner();
    while let Some(stamp) = stamps.pop() {
        let name = c_name(&stamp).expect("digits");
        let Ok(dir) = open_at(root.as_raw_fd(), &name, DIR_FLAGS) else {
            continue;
        };
        let stat = fstat(dir.as_raw_fd()).map_err(|error| {
            NativeError::command_failed(format!("cannot inspect a journal: {error}"))
        })?;
        if stat.st_uid != owner || stat.st_mode & 0o077 != 0 {
            return Err(NativeError::forbidden(
                "a shares journal folder is not a folder of root with mode 0700",
            ));
        }
        if lstat_at(dir.as_raw_fd(), &c_name(JOURNAL_FILE).expect("name")).is_ok() {
            return Ok(Some((stamp, dir)));
        }
    }
    Ok(None)
}

fn now_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    format!("{seconds:012}")
}

/// Write the journal of `records` and sync it: `journal.jsonl.part` first,
/// renamed to `journal.jsonl` once complete.
fn write_journal(
    options: &NativeOptions,
    share_id: &str,
    records: &[Record],
) -> Result<String, NativeError> {
    let root = open_journal_root(options, share_id, true)?.expect("created");
    let mut stamp = now_stamp();
    // Two runs in one second: the next free stamp.
    while lstat_at(root.as_raw_fd(), &c_name(&stamp).expect("digits")).is_ok() {
        let next: u64 = stamp.parse().unwrap_or_default();
        stamp = format!("{:012}", next + 1);
    }
    let stamp_name = c_name(&stamp).expect("digits");
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::mkdirat(root.as_raw_fd(), stamp_name.as_ptr(), 0o700) } != 0 {
        return Err(NativeError::command_failed(format!(
            "cannot create the journal folder: {}",
            io::Error::last_os_error()
        )));
    }
    let dir = open_at(root.as_raw_fd(), &stamp_name, DIR_FLAGS).map_err(|error| {
        NativeError::command_failed(format!("cannot open the journal folder: {error}"))
    })?;
    let part = c_name(&format!("{JOURNAL_FILE}.part")).expect("name");
    // SAFETY: valid fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            part.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(NativeError::command_failed(format!(
            "cannot create the journal: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut writer = io::BufWriter::new(file);
    let failed = |error: io::Error| {
        NativeError::command_failed(format!("cannot write the journal: {error}"))
    };
    for record in records {
        serde_json::to_writer(&mut writer, record)
            .map_err(|error| failed(io::Error::other(error)))?;
        writer.write_all(b"\n").map_err(failed)?;
    }
    let file = writer
        .into_inner()
        .map_err(|error| failed(error.into_error()))?;
    file.sync_all().map_err(failed)?;
    let full = c_name(JOURNAL_FILE).expect("name");
    // SAFETY: valid fds and NUL-terminated names.
    if unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            part.as_ptr(),
            dir.as_raw_fd(),
            full.as_ptr(),
        )
    } != 0
    {
        return Err(failed(io::Error::last_os_error()));
    }
    // SAFETY: valid fd.
    unsafe { libc::fsync(dir.as_raw_fd()) };
    Ok(stamp)
}

/// Read and validate every line of a journal.
fn read_journal(dir: &OwnedFd, rast_uid: u32, owner: u32) -> Result<Vec<Record>, NativeError> {
    let name = c_name(JOURNAL_FILE).expect("name");
    let fd = open_at(dir.as_raw_fd(), &name, libc::O_RDONLY).map_err(|error| {
        NativeError::command_failed(format!("cannot open the journal: {error}"))
    })?;
    let stat = fstat(fd.as_raw_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot inspect the journal: {error}"))
    })?;
    if !is_regular(&stat) || stat.st_uid != owner || stat.st_mode & 0o077 != 0 {
        return Err(NativeError::forbidden(
            "the shares journal is not a file of root with mode 0600",
        ));
    }
    let reader = io::BufReader::new(std::fs::File::from(fd));
    let mut records = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            NativeError::command_failed(format!("cannot read the journal: {error}"))
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if records.len() as u64 > MAX_TREE_ENTRIES {
            return Err(NativeError::forbidden("the shares journal is too large"));
        }
        let record: Record = serde_json::from_str(&line).map_err(|_| {
            NativeError::forbidden(format!(
                "line {} of the shares journal is invalid",
                index + 1
            ))
        })?;
        record.validate(rast_uid).map_err(|why| {
            NativeError::forbidden(format!(
                "line {} of the shares journal is refused: {why}",
                index + 1
            ))
        })?;
        records.push(record);
    }
    match records.first() {
        Some(first) if first.p.is_empty() => Ok(records),
        _ => Err(NativeError::forbidden(
            "the shares journal does not start with its container",
        )),
    }
}

pub(crate) async fn shares_migrate_scan(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_container(&op.container)?;
    let layout = Layout::for_share(options, &op.share_id).await?;
    let options = options.clone();
    let share_id = op.share_id.clone();
    let container = op.container.clone();
    let (report, journal) = blocking(move || {
        let (fd, stat) = checked_container(&options, &container, &layout)?;
        let report = prepare_tree(&layout, &fd, &stat, true, false)?;
        let journal = active_journal(&options, &share_id)?.map(|(stamp, _)| stamp);
        Ok((report, journal))
    })
    .await?;
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "journal": journal,
        "report": report,
    }))
}

pub(crate) async fn shares_migrate_apply(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_container(&op.container)?;
    let layout = Layout::for_share(options, &op.share_id).await?;
    let options = options.clone();
    let share_id = op.share_id.clone();
    let container = op.container.clone();
    let (report, journal, resumed) = blocking(move || {
        let (fd, stat) = checked_container(&options, &container, &layout)?;
        // A journal that was not rolled back is the state before the
        // migration: an earlier run that stopped half way resumes with it.
        let (journal, resumed) = match active_journal(&options, &share_id)? {
            Some((stamp, _)) => (stamp, true),
            None => {
                if stat.st_uid != layout.rast_uid {
                    return Err(NativeError::identity_conflict(format!(
                        "the container {container} is not a legacy shared folder and has no journal"
                    )));
                }
                let mut records = vec![record_of("", fd.as_raw_fd(), &stat).map_err(|error| {
                    NativeError::command_failed(format!("cannot record the container: {error}"))
                })?];
                let mut scan = TreeReport::default();
                walk(&fd, &layout.owners(), &mut scan, |found, report| {
                    let record = match found.dir {
                        Some(dir) => record_of(found.path, dir.as_raw_fd(), found.stat).ok(),
                        None => open_found_file(found).and_then(|(file, stat)| {
                            record_of(found.path, file.as_raw_fd(), &stat).ok()
                        }),
                    };
                    match record {
                        Some(record) => records.push(record),
                        None => report.changed_during_walk.add(found.path),
                    }
                    Ok(())
                })?;
                (write_journal(&options, &share_id, &records)?, false)
            }
        };
        let report = prepare_tree(&layout, &fd, &stat, true, true)?;
        Ok((report, journal, resumed))
    })
    .await?;
    tracing::info!(
        share = %op.share_id,
        changes = report.changes.count,
        resumed,
        "shared folder migrated"
    );
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "journal": journal,
        "resumed": resumed,
        "report": report,
    }))
}

/// Entries of a rollback's `droppedBits` listed in its reply; the full list
/// is written next to the journal.
pub const DROPPED_BITS_LISTED: usize = 2_000;
/// File name of the full list in the rolled-back journal folder.
pub const DROPPED_BITS_FILE: &str = "dropped-bits.txt";

/// An entry that a rollback restored without its set-uid bit, or without
/// the set-gid bit of a regular file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedBits {
    pub path: String,
    /// The recorded mode and the mode set, in octal.
    pub recorded: String,
    pub restored: String,
}

/// What a rollback did.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackReport {
    pub restored: u64,
    /// How many entries lost set-uid or set-gid bits, and the first
    /// [`DROPPED_BITS_LISTED`] of them (all of them are in
    /// `droppedBitsFile`).
    pub dropped_bits_count: u64,
    pub dropped_bits: Vec<DroppedBits>,
    /// The file in the journal folder that lists every one of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_bits_file: Option<String>,
    /// Recorded entries that now are another inode or have another owner.
    pub changed_since: Sample,
    /// Entries created after the migration (left as they are).
    pub not_recorded: Sample,
    /// Recorded entries that no longer exist.
    pub missing: Sample,
}

impl RollbackReport {
    /// Count (and list) `record` when its restored mode lost bits.
    fn note_dropped_bits(&mut self, record: &Record, all: &mut Vec<DroppedBits>) {
        let restored = record.restored_mode();
        if restored == record.mode {
            return;
        }
        let entry = DroppedBits {
            path: show(&record.p).to_string(),
            recorded: format!("{:04o}", record.mode),
            restored: format!("{restored:04o}"),
        };
        self.dropped_bits_count += 1;
        if self.dropped_bits.len() < DROPPED_BITS_LISTED {
            self.dropped_bits.push(entry.clone());
        }
        all.push(entry);
    }
}

/// Write the full list of entries that lost bits into the journal folder
/// (`<recorded> <restored> <path>` per line, root-only like the journal).
fn write_dropped_bits(dir: &OwnedFd, dropped: &[DroppedBits]) -> Result<(), NativeError> {
    let name = c_name(DROPPED_BITS_FILE).expect("name");
    // SAFETY: valid fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    let failed = |error: io::Error| {
        NativeError::command_failed(format!(
            "the folder was restored, but the list of dropped set-uid and set-gid bits could \
             not be written: {error}"
        ))
    };
    if fd < 0 {
        return Err(failed(io::Error::last_os_error()));
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut writer = io::BufWriter::new(file);
    for entry in dropped {
        writeln!(
            writer,
            "{} {} {}",
            entry.recorded, entry.restored, entry.path
        )
        .map_err(failed)?;
    }
    let file = writer
        .into_inner()
        .map_err(|error| failed(error.into_error()))?;
    file.sync_all().map_err(failed)
}

pub(crate) async fn shares_migrate_rollback(
    options: &NativeOptions,
    op: ShareDirOp,
) -> Result<Value, NativeError> {
    validate_share_id(&op.share_id)?;
    validate_share_container(&op.container)?;
    let options = options.clone();
    let share_id = op.share_id.clone();
    let container = op.container.clone();
    let (journal, report) = blocking(move || {
        let rast_uid = service_uid(&options)?;
        let owners = Owners::of(&options)?;
        let journal_uid = journal_owner();
        let Some((stamp, dir)) = active_journal(&options, &share_id)? else {
            return Err(NativeError::not_found(
                "this shared folder has no migration journal to roll back",
            ));
        };
        let records = read_journal(&dir, rast_uid, journal_uid)?;
        let (fd, stat) = open_container(&options, &container)?;
        let container_record = &records[0];
        if dev_of(&stat) != container_record.dev || ino_of(&stat) != container_record.ino {
            return Err(NativeError::forbidden(
                "the container is not the folder the journal recorded",
            ));
        }
        let mut by_path: BTreeMap<&str, &Record> = records[1..]
            .iter()
            .map(|record| (record.p.as_str(), record))
            .collect();
        let mut report = RollbackReport::default();
        let mut dropped = Vec::new();
        let mut walked = TreeReport::default();
        let test_mode = options.is_test_mode();
        let mut restore = |record: &Record,
                           fd: RawFd,
                           stat: &libc::stat,
                           path: &str,
                           report: &mut RollbackReport| {
            let same = dev_of(stat) == record.dev
                && ino_of(stat) == record.ino
                && stat.st_uid == record.uid
                && is_dir(stat) == record.d;
            if !same {
                report.changed_since.add(show(path));
                return Ok(());
            }
            record.restore(fd, test_mode).map_err(|error| {
                NativeError::command_failed(format!("cannot restore {}: {error}", show(path)))
            })?;
            report.restored += 1;
            report.note_dropped_bits(record, &mut dropped);
            Ok::<(), NativeError>(())
        };
        walk(&fd, &owners, &mut walked, |found, _| {
            let Some(record) = by_path.remove(found.path) else {
                report.not_recorded.add(found.path);
                return Ok(());
            };
            match found.dir {
                Some(dir) => restore(record, dir.as_raw_fd(), found.stat, found.path, &mut report),
                None => match open_found_file(found) {
                    Some((file, stat)) => {
                        restore(record, file.as_raw_fd(), &stat, found.path, &mut report)
                    }
                    None => {
                        report.changed_since.add(found.path);
                        Ok(())
                    }
                },
            }
        })?;
        for path in by_path.keys() {
            report.missing.add(path);
        }
        // The container last: it decides who can open the folder.
        container_record
            .restore(fd.as_raw_fd(), test_mode)
            .map_err(|error| {
                NativeError::command_failed(format!("cannot restore the container: {error}"))
            })?;
        report.restored += 1;
        report.note_dropped_bits(container_record, &mut dropped);
        if !dropped.is_empty() {
            write_dropped_bits(&dir, &dropped)?;
            report.dropped_bits_file = Some(format!(
                "{}/{stamp}{ROLLED_BACK_SUFFIX}/{DROPPED_BITS_FILE}",
                share_id
            ));
        }
        // Mark the journal as rolled back, so the next apply starts over.
        let root = open_journal_root(&options, &share_id, false)?.expect("exists");
        let from = c_name(&stamp).expect("digits");
        let to = c_name(&format!("{stamp}{ROLLED_BACK_SUFFIX}")).expect("name");
        drop(dir);
        // SAFETY: valid fds and NUL-terminated names.
        if unsafe {
            libc::renameat(
                root.as_raw_fd(),
                from.as_ptr(),
                root.as_raw_fd(),
                to.as_ptr(),
            )
        } != 0
        {
            return Err(NativeError::command_failed(format!(
                "the folder was restored, but its journal could not be marked: {}",
                io::Error::last_os_error()
            )));
        }
        Ok((stamp, report))
    })
    .await?;
    tracing::info!(share = %op.share_id, restored = report.restored, "shared folder rolled back");
    Ok(json!({
        "shareId": op.share_id,
        "container": op.container,
        "journal": journal,
        "report": report,
    }))
}

#[cfg(test)]
mod tests;
