//! Kernel quota for one-HOME accounts and shared folders (ADR 0005,
//! section 6): `quota_status`, `quota_set`, `share_quota_set` and
//! `quota_report`.
//!
//! The operations take ids only: a RAST user id (the uid of its identity is
//! derived) or a share id (the gid of its read-write group is looked up).
//! The filesystems are the fixed homes and shared roots of the helper's
//! configuration, opened as folders with `O_NOFOLLOW`; `quotactl_fd(2)`
//! acts on the filesystem of that descriptor. RAST never turns quota on or
//! changes `/etc/fstab`, and it never changes the grace times (they are
//! global per filesystem): `quota_status` reports them, and the runbook
//! sets them.
//!
//! Shared-folder limits use group quota on the rw gid (task 1.3, D3): the
//! server shows them as "filesystem, best effort". Project quota is
//! reported by `quota_status`; limits on it wait until `share_dir_prepare`
//! sets project ids.
//!
//! Everything kernel-facing sits behind [`QuotaFs`], so the operations are
//! tested with a fake.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native::protocol::{identity_ids, NativeError, MAX_USER_ID};
use crate::native::NativeOptions;

/// Most ids one `quota_report` accepts (users and shares together).
pub const MAX_REPORT_IDS: usize = 10_000;
/// Most shares one `quota_report` accepts (each needs a group lookup).
pub const MAX_REPORT_SHARES: usize = 1_000;
/// Quota block size of `struct dqblk` limits (`QIF_DQBLKSIZE`).
const QUOTA_BLOCK: u64 = 1024;

/// Which kind of id a quota is kept for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QuotaType {
    User,
    Group,
    Project,
}

impl QuotaType {
    pub const ALL: [QuotaType; 3] = [QuotaType::User, QuotaType::Group, QuotaType::Project];

    fn raw(self) -> i32 {
        match self {
            QuotaType::User => 0,
            QuotaType::Group => 1,
            QuotaType::Project => 2,
        }
    }

    fn key(self) -> &'static str {
        match self {
            QuotaType::User => "user",
            QuotaType::Group => "group",
            QuotaType::Project => "project",
        }
    }
}

/// What a filesystem does with one quota type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuotaState {
    /// No accounting: usage and limits are unknown.
    #[default]
    Off,
    /// Usage is kept, limits are not enforced.
    Accounting,
    /// Usage is kept and limits are enforced.
    Enforced,
}

/// The block limits of one id, in bytes (0: none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    pub soft: u64,
    pub hard: u64,
}

/// What the kernel keeps for one id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub used: u64,
    pub limits: Limits,
    /// When the grace period of a soft limit that is exceeded ends (Unix
    /// seconds; 0: not running).
    pub grace_ends: u64,
}

/// The kernel side of quota, on the filesystem of a descriptor.
pub trait QuotaFs: Send + Sync {
    /// The state of every quota type.
    fn states(&self, fd: BorrowedFd<'_>) -> io::Result<BTreeMap<QuotaType, QuotaState>>;
    /// The block grace time of a type, in seconds.
    fn grace(&self, fd: BorrowedFd<'_>, kind: QuotaType) -> io::Result<u64>;
    fn get(&self, fd: BorrowedFd<'_>, kind: QuotaType, id: u32) -> io::Result<Usage>;
    fn set(&self, fd: BorrowedFd<'_>, kind: QuotaType, id: u32, limits: Limits) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// The kernel: quotactl_fd(2)
// ---------------------------------------------------------------------------

/// `quotactl_fd(2)` on Linux 5.14 and later.
pub struct Kernel;

const SUBCMD_SHIFT: i32 = 8;
const Q_GETINFO: i32 = 0x80_0005;
const Q_GETQUOTA: i32 = 0x80_0007;
const Q_SETQUOTA: i32 = 0x80_0008;
/// `XQM_CMD(8)`: `('X' << 8) + 8`.
const Q_XGETQSTATV: i32 = (b'X' as i32) << 8 | 8;
const QIF_BLIMITS: u32 = 1;
const FS_QSTATV_VERSION1: i8 = 1;

/// `struct if_dqinfo`.
#[repr(C)]
#[derive(Default)]
struct DqInfo {
    bgrace: u64,
    igrace: u64,
    flags: u32,
    valid: u32,
}

/// `struct fs_qfilestatv`.
#[repr(C)]
#[derive(Default)]
struct QFileStatV {
    ino: u64,
    nblks: u64,
    nextents: u32,
    pad: u32,
}

/// `struct fs_quota_statv`.
#[repr(C)]
#[derive(Default)]
struct QuotaStatV {
    version: i8,
    pad1: u8,
    flags: u16,
    incoredqs: u32,
    uquota: QFileStatV,
    gquota: QFileStatV,
    pquota: QFileStatV,
    btimelimit: i32,
    itimelimit: i32,
    rtbtimelimit: i32,
    bwarnlimit: u16,
    iwarnlimit: u16,
    rtbwarnlimit: u16,
    pad3: u16,
    pad4: u32,
    pad2: [u64; 7],
}

const _: () = assert!(std::mem::size_of::<QuotaStatV>() == 160);
const _: () = assert!(std::mem::size_of::<DqInfo>() == 24);
const _: () = assert!(std::mem::size_of::<libc::dqblk>() == 72);

fn qcmd(command: i32, kind: QuotaType) -> i32 {
    (command << SUBCMD_SHIFT) | (kind.raw() & 0xff)
}

fn quotactl_fd(
    fd: BorrowedFd<'_>,
    command: i32,
    id: u32,
    addr: *mut libc::c_void,
) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor; `addr` points to the structure the
    // command reads or writes (or is null for none), owned by the caller.
    let result = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            fd.as_raw_fd(),
            command as libc::c_uint,
            id as libc::c_int,
            addr,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Errors that mean "no quota here" rather than a failure.
fn is_off(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::ENOTSUP | libc::ESRCH | libc::EINVAL | libc::ENOENT)
    )
}

/// Flags of `fs_quota_statv`: accounting and enforcement per type.
fn state_from_flags(flags: u16, kind: QuotaType) -> QuotaState {
    let shift = match kind {
        QuotaType::User => 0,
        QuotaType::Group => 2,
        QuotaType::Project => 4,
    };
    let accounting = flags & (1 << shift) != 0;
    let enforced = flags & (1 << (shift + 1)) != 0;
    match (accounting, enforced) {
        (true, true) => QuotaState::Enforced,
        (true, false) => QuotaState::Accounting,
        _ => QuotaState::Off,
    }
}

/// Bytes to quota blocks, rounded up.
fn blocks(bytes: u64) -> u64 {
    bytes.div_ceil(QUOTA_BLOCK)
}

impl QuotaFs for Kernel {
    fn states(&self, fd: BorrowedFd<'_>) -> io::Result<BTreeMap<QuotaType, QuotaState>> {
        let mut stat = QuotaStatV {
            version: FS_QSTATV_VERSION1,
            ..Default::default()
        };
        let mut out: BTreeMap<QuotaType, QuotaState> = QuotaType::ALL
            .into_iter()
            .map(|kind| (kind, QuotaState::Off))
            .collect();
        // The type does not matter for this command.
        let command = qcmd(Q_XGETQSTATV, QuotaType::User);
        match quotactl_fd(fd, command, 0, (&mut stat as *mut QuotaStatV).cast()) {
            Ok(()) => {
                for kind in QuotaType::ALL {
                    out.insert(kind, state_from_flags(stat.flags, kind));
                }
                Ok(out)
            }
            Err(error) if is_off(&error) => Ok(out),
            Err(error) => Err(error),
        }
    }

    fn grace(&self, fd: BorrowedFd<'_>, kind: QuotaType) -> io::Result<u64> {
        let mut info = DqInfo::default();
        quotactl_fd(
            fd,
            qcmd(Q_GETINFO, kind),
            0,
            (&mut info as *mut DqInfo).cast(),
        )?;
        Ok(info.bgrace)
    }

    fn get(&self, fd: BorrowedFd<'_>, kind: QuotaType, id: u32) -> io::Result<Usage> {
        // SAFETY: `dqblk` is plain integers; all zeroes is a valid value.
        let mut quota: libc::dqblk = unsafe { std::mem::zeroed() };
        quotactl_fd(
            fd,
            qcmd(Q_GETQUOTA, kind),
            id,
            (&mut quota as *mut libc::dqblk).cast(),
        )?;
        Ok(Usage {
            used: quota.dqb_curspace,
            limits: Limits {
                soft: quota.dqb_bsoftlimit.saturating_mul(QUOTA_BLOCK),
                hard: quota.dqb_bhardlimit.saturating_mul(QUOTA_BLOCK),
            },
            grace_ends: quota.dqb_btime,
        })
    }

    fn set(&self, fd: BorrowedFd<'_>, kind: QuotaType, id: u32, limits: Limits) -> io::Result<()> {
        // SAFETY: as above.
        let mut quota: libc::dqblk = unsafe { std::mem::zeroed() };
        quota.dqb_bsoftlimit = blocks(limits.soft);
        quota.dqb_bhardlimit = blocks(limits.hard);
        // Only the block limits: inode limits and usage stay as they are.
        quota.dqb_valid = QIF_BLIMITS;
        quotactl_fd(
            fd,
            qcmd(Q_SETQUOTA, kind),
            id,
            (&mut quota as *mut libc::dqblk).cast(),
        )
    }
}

// ---------------------------------------------------------------------------
// The fixed roots
// ---------------------------------------------------------------------------

/// Which root an operation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    Homes,
    Shared,
}

fn root_path(options: &NativeOptions, root: Root) -> std::path::PathBuf {
    match root {
        Root::Homes => options.homes_dir.clone(),
        Root::Shared => options.storage_dir.join("shared"),
    }
}

/// Open a root as a folder (never through a final symlink). `quotactl_fd`
/// needs a descriptor that is not `O_PATH`.
pub fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `name` is NUL-terminated; the flags open no file for writing.
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What `quota_status` reports for one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootStatus {
    /// `major:minor` of the filesystem.
    pub device: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub user: QuotaState,
    pub group: QuotaState,
    pub project: QuotaState,
    /// Block grace times in seconds, of the types that are on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_grace_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_grace_secs: Option<u64>,
}

impl RootStatus {
    pub fn state(&self, kind: QuotaType) -> QuotaState {
        match kind {
            QuotaType::User => self.user,
            QuotaType::Group => self.group,
            QuotaType::Project => self.project,
        }
    }
}

fn fs_type_name(magic: i64) -> String {
    match magic {
        0xEF53 => "ext4".into(),
        0x5846_5342 => "xfs".into(),
        0x9123_683E => "btrfs".into(),
        0x0102_1994 => "tmpfs".into(),
        0x2FC1_2FC1 => "zfs".into(),
        0x794C_7630 => "overlay".into(),
        0x6573_5546 => "fuse".into(),
        other => format!("0x{other:x}"),
    }
}

fn describe(fs: &dyn QuotaFs, fd: BorrowedFd<'_>) -> io::Result<RootStatus> {
    // SAFETY: plain structures filled by the kernel.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is live and the buffers are owned here.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0
        || unsafe { libc::fstatfs(fd.as_raw_fd(), &mut statfs) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let states = fs.states(fd)?;
    let state = |kind| states.get(&kind).copied().unwrap_or_default();
    let grace = |kind| {
        (state(kind) != QuotaState::Off)
            .then(|| fs.grace(fd, kind).ok())
            .flatten()
    };
    #[allow(clippy::unnecessary_cast)] // the field types differ between targets
    let total = (statfs.f_blocks as u64).saturating_mul(statfs.f_bsize as u64);
    #[allow(clippy::unnecessary_cast)]
    let magic = statfs.f_type as i64;
    Ok(RootStatus {
        device: format!("{}:{}", libc::major(stat.st_dev), libc::minor(stat.st_dev)),
        fs_type: fs_type_name(magic),
        total_bytes: total,
        user: state(QuotaType::User),
        group: state(QuotaType::Group),
        project: state(QuotaType::Project),
        user_grace_secs: grace(QuotaType::User),
        group_grace_secs: grace(QuotaType::Group),
    })
}

fn open_failed(root: Root, error: io::Error) -> NativeError {
    let name = match root {
        Root::Homes => "homes",
        Root::Shared => "shared",
    };
    NativeError::unavailable(format!("cannot open the {name} root: {error}"))
}

// ---------------------------------------------------------------------------
// The operations
// ---------------------------------------------------------------------------

/// `quotaSet` and `shareQuotaSet` byte limits: both 0 (no limit), or
/// 0 < soft ≤ hard ≤ the filesystem's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ByteLimits {
    pub soft_bytes: u64,
    pub hard_bytes: u64,
}

impl ByteLimits {
    pub fn validate(&self, total_bytes: u64) -> Result<Limits, NativeError> {
        let (soft, hard) = (self.soft_bytes, self.hard_bytes);
        if soft == 0 && hard == 0 {
            return Ok(Limits::default());
        }
        if soft == 0 || hard < soft {
            return Err(NativeError::invalid(
                "softBytes and hardBytes must both be 0, or 0 < softBytes <= hardBytes",
            ));
        }
        if total_bytes > 0 && hard > total_bytes {
            return Err(NativeError::invalid(
                "hardBytes is larger than the filesystem",
            ));
        }
        Ok(Limits { soft, hard })
    }
}

/// The homes and shared roots, as `quota_status` replies.
pub fn status(options: &NativeOptions, fs: &dyn QuotaFs) -> Result<Value, NativeError> {
    let mut out = serde_json::Map::new();
    for (root, key) in [(Root::Homes, "homes"), (Root::Shared, "shared")] {
        let path = root_path(options, root);
        let fd = open_root(&path).map_err(|error| open_failed(root, error))?;
        let status = describe(fs, fd.as_fd()).map_err(|error| {
            NativeError::command_failed(format!("cannot read the quota state: {error}"))
        })?;
        out.insert(
            key.into(),
            serde_json::to_value(status).map_err(|error| {
                NativeError::command_failed(format!("cannot encode the quota state: {error}"))
            })?,
        );
    }
    let same = out["homes"]["device"] == out["shared"]["device"];
    out.insert("sameFilesystem".into(), Value::Bool(same));
    Ok(Value::Object(out))
}

/// Open a root and check that quota of `kind` is on there.
fn quota_root(
    options: &NativeOptions,
    fs: &dyn QuotaFs,
    root: Root,
    kind: QuotaType,
) -> Result<(OwnedFd, RootStatus), NativeError> {
    let fd = open_root(&root_path(options, root)).map_err(|error| open_failed(root, error))?;
    let status = describe(fs, fd.as_fd()).map_err(|error| {
        NativeError::command_failed(format!("cannot read the quota state: {error}"))
    })?;
    if status.state(kind) == QuotaState::Off {
        return Err(NativeError::unavailable(format!(
            "{} quota is not on for this filesystem",
            kind.key()
        )));
    }
    Ok((fd, status))
}

fn set_failed(error: io::Error) -> NativeError {
    NativeError::command_failed(format!("cannot set the quota: {error}"))
}

/// `quota_set`: the limits of a user's identity on the homes filesystem.
pub fn set_user(
    options: &NativeOptions,
    fs: &dyn QuotaFs,
    user_id: i64,
    limits: ByteLimits,
) -> Result<Value, NativeError> {
    let ids = identity_ids(&options.validation_context(), user_id)?;
    let (fd, status) = quota_root(options, fs, Root::Homes, QuotaType::User)?;
    let limits = limits.validate(status.total_bytes)?;
    fs.set(fd.as_fd(), QuotaType::User, ids.uid, limits)
        .map_err(set_failed)?;
    Ok(json!({
        "userId": user_id,
        "uid": ids.uid,
        "softBytes": limits.soft,
        "hardBytes": limits.hard,
        "state": status.user,
    }))
}

/// `share_quota_set`: the limits of a shared folder's rw group on the
/// shared filesystem (group quota, "best effort": see the module docs).
pub fn set_share(
    options: &NativeOptions,
    fs: &dyn QuotaFs,
    share_id: &str,
    rw_gid: u32,
    limits: ByteLimits,
) -> Result<Value, NativeError> {
    let (fd, status) = quota_root(options, fs, Root::Shared, QuotaType::Group)?;
    let limits = limits.validate(status.total_bytes)?;
    fs.set(fd.as_fd(), QuotaType::Group, rw_gid, limits)
        .map_err(set_failed)?;
    Ok(json!({
        "shareId": share_id,
        "gid": rw_gid,
        "softBytes": limits.soft,
        "hardBytes": limits.hard,
        "backend": "group",
        "state": status.group,
    }))
}

/// One reported id: `[id, usedBytes, softBytes, hardBytes, graceEnds]`
/// (compact, so 10 000 ids fit in one reply).
fn row<T: Serialize>(id: T, usage: &Usage) -> Value {
    json!([
        id,
        usage.used,
        usage.limits.soft,
        usage.limits.hard,
        usage.grace_ends
    ])
}

/// `quota_report`: usage and limits of users (user quota on the homes
/// filesystem) and shares (group quota of their rw gid on the shared
/// filesystem). A type that is off is reported as such, without rows.
pub fn report(
    options: &NativeOptions,
    fs: &dyn QuotaFs,
    user_ids: &[i64],
    shares: &[(String, u32)],
) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let mut users = Vec::new();
    let homes_state = if user_ids.is_empty() {
        None
    } else {
        let fd = open_root(&root_path(options, Root::Homes))
            .map_err(|error| open_failed(Root::Homes, error))?;
        let states = fs.states(fd.as_fd()).map_err(|error| {
            NativeError::command_failed(format!("cannot read the quota state: {error}"))
        })?;
        let state = states.get(&QuotaType::User).copied().unwrap_or_default();
        if state != QuotaState::Off {
            for user_id in user_ids {
                let ids = identity_ids(&ctx, *user_id)?;
                let usage = fs
                    .get(fd.as_fd(), QuotaType::User, ids.uid)
                    .or_else(|error| {
                        // No usage recorded yet for this id.
                        if error.raw_os_error() == Some(libc::ESRCH) {
                            Ok(Usage::default())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|error| {
                        NativeError::command_failed(format!("cannot read the quota: {error}"))
                    })?;
                users.push(row(*user_id, &usage));
            }
        }
        Some(state)
    };
    let mut share_rows = Vec::new();
    let shared_state = if shares.is_empty() {
        None
    } else {
        let fd = open_root(&root_path(options, Root::Shared))
            .map_err(|error| open_failed(Root::Shared, error))?;
        let states = fs.states(fd.as_fd()).map_err(|error| {
            NativeError::command_failed(format!("cannot read the quota state: {error}"))
        })?;
        let state = states.get(&QuotaType::Group).copied().unwrap_or_default();
        if state != QuotaState::Off {
            for (share_id, gid) in shares {
                let usage = fs
                    .get(fd.as_fd(), QuotaType::Group, *gid)
                    .or_else(|error| {
                        if error.raw_os_error() == Some(libc::ESRCH) {
                            Ok(Usage::default())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|error| {
                        NativeError::command_failed(format!("cannot read the quota: {error}"))
                    })?;
                share_rows.push(row(share_id, &usage));
            }
        }
        Some(state)
    };
    let mut out = json!({ "users": users, "shares": share_rows });
    if let Some(state) = homes_state {
        out["userQuota"] = json!(state);
    }
    if let Some(state) = shared_state {
        out["groupQuota"] = json!(state);
    }
    Ok(out)
}

/// Check the ids of a `quota_report` request.
pub fn validate_report(user_ids: &[i64], share_ids: &[String]) -> Result<(), NativeError> {
    if user_ids.is_empty() && share_ids.is_empty() {
        return Err(NativeError::invalid(
            "`userIds` or `shareIds` must list an id",
        ));
    }
    if user_ids.len() + share_ids.len() > MAX_REPORT_IDS {
        return Err(NativeError::invalid(format!(
            "a report covers at most {MAX_REPORT_IDS} ids"
        )));
    }
    if share_ids.len() > MAX_REPORT_SHARES {
        return Err(NativeError::invalid(format!(
            "a report covers at most {MAX_REPORT_SHARES} shared folders"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in user_ids {
        if !(1..=MAX_USER_ID).contains(id) {
            return Err(NativeError::invalid(format!(
                "user ids must be between 1 and {MAX_USER_ID}"
            )));
        }
        if !seen.insert(*id) {
            return Err(NativeError::invalid(format!(
                "`userIds` lists user {id} twice"
            )));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in share_ids {
        crate::native::protocol::validate_share_id(id)?;
        if !seen.insert(id.as_str()) {
            return Err(NativeError::invalid(format!("`shareIds` lists {id} twice")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Quota as a test wants it: per root device, states and usage.
    #[derive(Default)]
    pub struct Fake {
        pub states: BTreeMap<QuotaType, QuotaState>,
        pub usage: Mutex<BTreeMap<(QuotaType, u32), Usage>>,
        pub sets: Mutex<Vec<(QuotaType, u32, Limits)>>,
        pub fail: Option<i32>,
    }

    impl Fake {
        fn with(states: &[(QuotaType, QuotaState)]) -> Self {
            Self {
                states: states.iter().copied().collect(),
                ..Default::default()
            }
        }
    }

    impl QuotaFs for Fake {
        fn states(&self, _fd: BorrowedFd<'_>) -> io::Result<BTreeMap<QuotaType, QuotaState>> {
            Ok(self.states.clone())
        }
        fn grace(&self, _fd: BorrowedFd<'_>, _kind: QuotaType) -> io::Result<u64> {
            Ok(7 * 86_400)
        }
        fn get(&self, _fd: BorrowedFd<'_>, kind: QuotaType, id: u32) -> io::Result<Usage> {
            if let Some(errno) = self.fail {
                return Err(io::Error::from_raw_os_error(errno));
            }
            self.usage
                .lock()
                .unwrap()
                .get(&(kind, id))
                .copied()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
        }
        fn set(
            &self,
            _fd: BorrowedFd<'_>,
            kind: QuotaType,
            id: u32,
            limits: Limits,
        ) -> io::Result<()> {
            self.sets.lock().unwrap().push((kind, id, limits));
            Ok(())
        }
    }

    fn options(temp: &tempfile::TempDir) -> NativeOptions {
        let mut options = NativeOptions::for_test(
            temp.path().join("h.sock"),
            temp.path().join("data"),
            temp.path().join("run"),
        );
        options.storage_dir = temp.path().join("storage");
        std::fs::create_dir_all(&options.homes_dir).unwrap();
        std::fs::create_dir_all(options.storage_dir.join("shared")).unwrap();
        options
    }

    #[test]
    fn statv_flags_map_to_states() {
        assert_eq!(state_from_flags(0, QuotaType::User), QuotaState::Off);
        assert_eq!(
            state_from_flags(0b01, QuotaType::User),
            QuotaState::Accounting
        );
        assert_eq!(
            state_from_flags(0b11, QuotaType::User),
            QuotaState::Enforced
        );
        assert_eq!(
            state_from_flags(0b1100, QuotaType::Group),
            QuotaState::Enforced
        );
        assert_eq!(state_from_flags(0b1100, QuotaType::User), QuotaState::Off);
        assert_eq!(
            state_from_flags(0b01_0000, QuotaType::Project),
            QuotaState::Accounting
        );
        assert_eq!(qcmd(Q_GETQUOTA, QuotaType::Group) as u32, 0x8000_0701);
        assert_eq!(qcmd(Q_XGETQSTATV, QuotaType::User), 0x58_0800);
        assert_eq!(blocks(1), 1);
        assert_eq!(blocks(1024), 1);
        assert_eq!(blocks(1025), 2);
    }

    #[test]
    fn limits_are_validated() {
        let limits = |soft, hard| ByteLimits {
            soft_bytes: soft,
            hard_bytes: hard,
        };
        assert_eq!(limits(0, 0).validate(100).unwrap(), Limits::default());
        assert_eq!(
            limits(10, 20).validate(100).unwrap(),
            Limits { soft: 10, hard: 20 }
        );
        for (soft, hard) in [(0, 10), (10, 0), (20, 10), (10, 101)] {
            assert!(limits(soft, hard).validate(100).is_err(), "{soft} {hard}");
        }
        let parsed: Result<ByteLimits, _> =
            serde_json::from_value(json!({"softBytes": 1, "hardBytes": 2, "path": "/"}));
        assert!(parsed.is_err());
        let parsed: Result<ByteLimits, _> =
            serde_json::from_value(json!({"softBytes": -1, "hardBytes": 2}));
        assert!(parsed.is_err());
    }

    #[test]
    fn the_kernel_reports_off_or_a_state_for_a_plain_folder() {
        // No quota needed: an unprivileged Q_XGETQSTATV either answers or
        // says "no quota here"; either way the call does not fail.
        let temp = tempfile::tempdir().unwrap();
        let fd = open_root(temp.path()).unwrap();
        let states = Kernel.states(fd.as_fd()).unwrap();
        assert_eq!(states.len(), 3);
        let status = describe(&Kernel, fd.as_fd()).unwrap();
        assert!(status.total_bytes > 0);
        assert!(status.device.contains(':'));
    }

    #[test]
    fn roots_are_never_opened_through_a_link() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("link")).unwrap();
        assert!(open_root(&temp.path().join("real")).is_ok());
        assert!(open_root(&temp.path().join("link")).is_err());
    }

    #[test]
    fn status_describes_both_roots() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(&temp);
        let fake = Fake::with(&[
            (QuotaType::User, QuotaState::Enforced),
            (QuotaType::Group, QuotaState::Accounting),
        ]);
        let value = status(&options, &fake).unwrap();
        assert_eq!(value["homes"]["user"], "enforced");
        assert_eq!(value["homes"]["group"], "accounting");
        assert_eq!(value["homes"]["project"], "off");
        assert_eq!(value["homes"]["userGraceSecs"], 604_800);
        assert!(value["homes"].get("projectGraceSecs").is_none());
        assert_eq!(value["sameFilesystem"], true);
        let parsed: RootStatus = serde_json::from_value(value["shared"].clone()).unwrap();
        assert_eq!(parsed.user, QuotaState::Enforced);
        // A missing root is an error, not "off".
        std::fs::remove_dir(options.storage_dir.join("shared")).unwrap();
        assert_eq!(status(&options, &fake).unwrap_err().code, "unavailable");
    }

    #[test]
    fn limits_go_to_the_identity_uid_and_the_rw_gid() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(&temp);
        let fake = Fake::with(&[
            (QuotaType::User, QuotaState::Enforced),
            (QuotaType::Group, QuotaState::Enforced),
        ]);
        let soft = 20 << 30;
        let hard = soft + (1 << 30);
        let limits = ByteLimits {
            soft_bytes: soft,
            hard_bytes: hard,
        };
        let total = describe(&fake, open_root(&options.homes_dir).unwrap().as_fd())
            .unwrap()
            .total_bytes;
        let reply = set_user(&options, &fake, 7, limits);
        if hard > total {
            assert_eq!(reply.unwrap_err().code, "invalid_request");
        } else {
            assert_eq!(reply.unwrap()["uid"], 200_007);
        }
        let small = ByteLimits {
            soft_bytes: 1000,
            hard_bytes: 2000,
        };
        set_user(&options, &fake, 7, small).unwrap();
        let reply = set_share(
            &options,
            &fake,
            "00000000-0000-4000-8000-000000000001",
            201_000_005,
            small,
        )
        .unwrap();
        assert_eq!(reply["backend"], "group");
        for user_id in [0, -1, MAX_USER_ID + 1] {
            assert!(set_user(&options, &fake, user_id, small).is_err());
        }
        let sets = fake.sets.lock().unwrap().clone();
        let small = Limits {
            soft: 1000,
            hard: 2000,
        };
        assert!(sets.contains(&(QuotaType::User, 200_007, small)));
        assert_eq!(sets.last(), Some(&(QuotaType::Group, 201_000_005, small)));
        // Quota off: nothing is set.
        let off = Fake::default();
        assert_eq!(
            set_user(
                &options,
                &off,
                7,
                ByteLimits {
                    soft_bytes: 0,
                    hard_bytes: 0
                }
            )
            .unwrap_err()
            .code,
            "unavailable"
        );
        assert!(off.sets.lock().unwrap().is_empty());
    }

    #[test]
    fn reports_list_usage_per_id_and_say_when_quota_is_off() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(&temp);
        let fake = Fake::with(&[(QuotaType::User, QuotaState::Accounting)]);
        fake.usage.lock().unwrap().insert(
            (QuotaType::User, 200_003),
            Usage {
                used: 4096,
                limits: Limits {
                    soft: 1024,
                    hard: 2048,
                },
                grace_ends: 99,
            },
        );
        let share = "00000000-0000-4000-8000-000000000001".to_string();
        let value = report(&options, &fake, &[3, 4], &[(share.clone(), 201_000_001)]).unwrap();
        assert_eq!(value["userQuota"], "accounting");
        assert_eq!(
            value["users"],
            json!([[3, 4096, 1024, 2048, 99], [4, 0, 0, 0, 0]])
        );
        assert_eq!(value["groupQuota"], "off");
        assert_eq!(value["shares"], json!([]));
        let failing = Fake {
            fail: Some(libc::EIO),
            ..Fake::with(&[(QuotaType::User, QuotaState::Enforced)])
        };
        assert!(report(&options, &failing, &[3], &[]).is_err());
        assert!(validate_report(&[], &[]).is_err());
        assert!(validate_report(&[1, 1], &[]).is_err());
        assert!(validate_report(&[0], &[]).is_err());
        assert!(validate_report(&[1], &[share.clone(), share.clone()]).is_err());
        assert!(validate_report(&[1], &["../x".into()]).is_err());
        let many: Vec<i64> = (1..=MAX_REPORT_IDS as i64 + 1).collect();
        assert!(validate_report(&many, &[]).is_err());
        assert!(validate_report(&many[..MAX_REPORT_IDS], &[]).is_ok());
        // 10 000 rows fit in one reply.
        let rows: Vec<Value> = (1..=MAX_REPORT_IDS as i64)
            .map(|id| {
                row(
                    id,
                    &Usage {
                        used: u64::MAX >> 11,
                        limits: Limits {
                            soft: u64::MAX >> 11,
                            hard: u64::MAX >> 11,
                        },
                        grace_ends: u32::MAX as u64,
                    },
                )
            })
            .collect();
        let line = serde_json::to_vec(&json!({"ok": true, "data": {"users": rows}})).unwrap();
        assert!(
            line.len() < crate::native::protocol::MAX_RESPONSE_BYTES,
            "{}",
            line.len()
        );
    }
}
