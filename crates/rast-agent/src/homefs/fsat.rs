//! File operations relative to directory descriptors, for the per-user file
//! agent (ADR 0005, section 2).
//!
//! The agent runs as the user, and the other members of a shared folder can
//! change entries in it while the agent works. A path-based call looks every
//! component up again, so a member could swap a folder or a file for a
//! symbolic link between a check and the call and point the agent at files
//! of its own user: a copy that reads `~/.ssh/id_ed25519` into the share, or
//! one that writes into `~/.config/autostart` (a confused deputy; the
//! kernel allows both, because the agent's user may do them). Here every
//! operation starts from a descriptor of a namespace root (HOME or a share
//! container):
//!
//! * paths below a root are opened with `openat2` and `RESOLVE_BENEATH |
//!   RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS` (on kernels without
//!   `openat2`, one component at a time with `O_NOFOLLOW`), so no symbolic
//!   link is followed and nothing outside the root is reached;
//! * everything else acts on one name in a folder the agent holds open
//!   (`*at` calls with `AT_SYMLINK_NOFOLLOW` or `O_NOFOLLOW`), so a renamed
//!   or replaced ancestor cannot redirect it;
//! * new files are created with `O_CREAT | O_EXCL | O_NOFOLLOW`, new folders
//!   with `mkdirat` and then used only while they are still the empty folder
//!   just made, and renames that must not replace use `RENAME_NOREPLACE`;
//! * copies, moves, sizes and removals walk trees through descriptors.
//!
//! A member can still change entries in folders they may write themselves;
//! the agent then acts on what is there, never on anything elsewhere.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{AppError, AppResult};
use crate::fileagent::check;
use crate::storage::MAX_TREE_ENTRIES;

/// Deepest folder nesting that copies and removals descend into: every level
/// keeps descriptors open.
pub const MAX_DEPTH: usize = 128;

/// What the agent needs from a `stat`, whatever the layout of the platform's
/// `struct stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub kind: Kind,
    pub len: u64,
    /// Modification time in milliseconds since 1970 (0 before that), as
    /// `ops::modified_ms` reports it.
    pub modified_ms: i64,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Info {
    // The field types of `struct stat` differ between architectures.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &rfs::Stat) -> Self {
        let mode = stat.st_mode as u32;
        let kind = match FileType::from_raw_mode(mode as _) {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Dir,
            FileType::Symlink => Kind::Symlink,
            _ => Kind::Other,
        };
        let seconds = stat.st_mtime as i64;
        let nanos = stat.st_mtime_nsec as i64;
        Self {
            kind,
            len: (stat.st_size as i64).max(0) as u64,
            modified_ms: if seconds < 0 {
                0
            } else {
                seconds
                    .saturating_mul(1000)
                    .saturating_add(nanos / 1_000_000)
            },
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            uid: stat.st_uid as u32,
            gid: stat.st_gid as u32,
            mode,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn same_inode(&self, other: &Info) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

/// An I/O error of these operations as the File Station error the agent
/// answers with.
pub fn fs_error(error: io::Error) -> AppError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => AppError::Forbidden("symbolic links are not supported"),
        // `RESOLVE_BENEATH` refused to leave the root.
        Some(libc::EXDEV) => AppError::Forbidden("path escapes the permitted storage area"),
        Some(libc::ENOENT) | Some(libc::ENOTDIR) => AppError::NotFound("path not found"),
        _ => error.into(),
    }
}

fn errno(error: Errno) -> AppError {
    fs_error(error.into())
}

fn changed() -> AppError {
    AppError::Forbidden("file changed while it was being opened")
}

/// Points in the middle of an operation where tests change the tree, as
/// another member of a shared folder could (no-ops outside tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Before a source entry is opened for reading.
    OpenSource,
    /// Before a target file is created.
    CreateTarget,
    /// After a target folder was made, before it is opened.
    MadeDir,
}

#[cfg(test)]
pub(crate) mod race {
    use std::sync::Mutex;

    use super::Point;

    type Hook = Box<dyn FnOnce() + Send>;

    static HOOKS: Mutex<Vec<(Point, Vec<u8>, Hook)>> = Mutex::new(Vec::new());

    /// Run `hook` once, the next time an operation reaches `point` for an
    /// entry named `name` (names are unique per test).
    pub fn at(point: Point, name: &str, hook: impl FnOnce() + Send + 'static) {
        HOOKS
            .lock()
            .unwrap()
            .push((point, name.as_bytes().to_vec(), Box::new(hook)));
    }

    pub(crate) fn reach(point: Point, name: &[u8]) {
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|(at, hooked, _)| *at == point && hooked == name)
                .map(|index| hooks.remove(index).2)
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
use race::reach;

#[cfg(not(test))]
#[inline(always)]
fn reach(_point: Point, _name: &[u8]) {}

/// Flags of a handle on a folder, used for `*at` calls only.
fn handle_flags() -> OFlags {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = OFlags::PATH | OFlags::DIRECTORY;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = OFlags::RDONLY | OFlags::DIRECTORY;
    flags | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// One component of a relative path. `LogicalPath` already guarantees this;
/// it is checked again so no caller can pass a path.
fn component_ok(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

fn invalid_component() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid path component")
}

#[cfg(target_os = "linux")]
static OPENAT2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Open the folder `path` as a namespace root (once, at start-up).
pub fn open_root(path: &Path) -> io::Result<(OwnedFd, Info)> {
    let fd = rfs::open(path, handle_flags(), Mode::empty())?;
    let info = stat_fd(&fd)?;
    if !info.is_dir() {
        return Err(Errno::NOTDIR.into());
    }
    Ok((fd, info))
}

/// Open `segments` below `root` with `flags` (`O_NOFOLLOW` and `O_CLOEXEC`
/// added), following no symbolic link on the way or at the end, and return
/// the descriptor with its `stat`. No segments opens the root itself.
pub fn open_beneath(
    root: BorrowedFd<'_>,
    segments: &[String],
    flags: OFlags,
) -> io::Result<(OwnedFd, Info)> {
    if !segments.iter().all(|segment| component_ok(segment)) {
        return Err(invalid_component());
    }
    let fd = match open_path(root, segments, flags | OFlags::NOFOLLOW | OFlags::CLOEXEC) {
        Ok(fd) => fd,
        // `O_DIRECTORY` on a final link fails with `ENOTDIR`.
        Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
            return Err(if last_is_link(root, segments) {
                Errno::LOOP.into()
            } else {
                error
            });
        }
        Err(error) => return Err(error),
    };
    let info = stat_fd(&fd)?;
    // `O_PATH | O_NOFOLLOW` opens a final link itself instead of refusing it.
    if info.kind == Kind::Symlink {
        return Err(Errno::LOOP.into());
    }
    Ok((fd, info))
}

/// Whether the last of `segments` below `root` is a symbolic link.
fn last_is_link(root: BorrowedFd<'_>, segments: &[String]) -> bool {
    let Some((last, parents)) = segments.split_last() else {
        return false;
    };
    let Ok(parent) = open_path(root, parents, handle_flags()) else {
        return false;
    };
    matches!(stat_at(parent.as_fd(), last), Ok(Some(info)) if info.kind == Kind::Symlink)
}

fn open_path(root: BorrowedFd<'_>, segments: &[String], flags: OFlags) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    if OPENAT2.load(std::sync::atomic::Ordering::Relaxed) {
        let path = if segments.is_empty() {
            ".".to_string()
        } else {
            segments.join("/")
        };
        let resolve = rfs::ResolveFlags::BENEATH
            | rfs::ResolveFlags::NO_MAGICLINKS
            | rfs::ResolveFlags::NO_SYMLINKS;
        let mut attempts = 0;
        loop {
            match rfs::openat2(root, path.as_str(), flags, Mode::empty(), resolve) {
                // A rename elsewhere on the system raced the lookup.
                Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
                Err(Errno::NOSYS) => {
                    OPENAT2.store(false, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                other => return other.map_err(io::Error::from),
            }
        }
    }
    open_walk(root, segments, flags)
}

/// [`open_path`] one component at a time (kernels without `openat2`): every
/// component is opened with `O_NOFOLLOW` relative to the one before.
fn open_walk(root: BorrowedFd<'_>, segments: &[String], flags: OFlags) -> io::Result<OwnedFd> {
    let Some((last, parents)) = segments.split_last() else {
        return Ok(rfs::openat(root, ".", flags, Mode::empty())?);
    };
    let mut dir: Option<OwnedFd> = None;
    for name in parents {
        let at = dir.as_ref().map_or(root, |fd| fd.as_fd());
        dir = Some(openat_nofollow(
            at,
            name.as_str(),
            handle_flags(),
            Mode::empty(),
        )?);
    }
    let at = dir.as_ref().map_or(root, |fd| fd.as_fd());
    Ok(openat_nofollow(at, last.as_str(), flags, Mode::empty())?)
}

/// `openat` of one name with `O_NOFOLLOW | O_CLOEXEC` added. `O_DIRECTORY`
/// on a link fails with `ENOTDIR`; it is reported as `ELOOP`, like
/// `RESOLVE_NO_SYMLINKS`.
fn openat_nofollow<P: rustix::path::Arg + Copy>(
    dir: BorrowedFd<'_>,
    name: P,
    flags: OFlags,
    mode: Mode,
) -> Result<OwnedFd, Errno> {
    match rfs::openat(dir, name, flags | OFlags::NOFOLLOW | OFlags::CLOEXEC, mode) {
        Err(Errno::NOTDIR)
            if flags.contains(OFlags::DIRECTORY)
                && matches!(
                    rfs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW),
                    Ok(stat) if Info::from_stat(&stat).kind == Kind::Symlink
                ) =>
        {
            Err(Errno::LOOP)
        }
        other => other,
    }
}

/// A handle on the folder `segments` below `root`, for `*at` calls.
pub fn open_dir(root: BorrowedFd<'_>, segments: &[String]) -> io::Result<(OwnedFd, Info)> {
    open_beneath(root, segments, handle_flags())
}

pub fn stat_fd(fd: impl AsFd) -> io::Result<Info> {
    Ok(Info::from_stat(&rfs::fstat(fd)?))
}

/// The `stat` of `name` in `dir`, not following a link (`None`: no entry).
pub fn stat_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<Option<Info>> {
    if !component_ok(name) {
        return Err(invalid_component());
    }
    match rfs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(Info::from_stat(&stat))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Open `name` in `dir` with `flags` (`O_NOFOLLOW` and `O_CLOEXEC` added).
pub fn open_at(dir: BorrowedFd<'_>, name: &str, flags: OFlags) -> io::Result<OwnedFd> {
    if !component_ok(name) {
        return Err(invalid_component());
    }
    Ok(openat_nofollow(dir, name, flags, Mode::empty())?)
}

/// A handle on the folder `name` in `dir`, which must still be `expected`.
pub fn open_dir_at(dir: BorrowedFd<'_>, name: &str, expected: &Info) -> AppResult<OwnedFd> {
    let fd = open_at(dir, name, handle_flags()).map_err(fs_error)?;
    let info = stat_fd(&fd).map_err(fs_error)?;
    if !info.is_dir() || !info.same_inode(expected) {
        return Err(changed());
    }
    Ok(fd)
}

/// Open the folder `name` in `dir` (or `dir` itself) to read its entries; it
/// must still be `expected`.
pub fn open_listing(
    dir: BorrowedFd<'_>,
    name: Option<&str>,
    expected: &Info,
) -> AppResult<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY;
    let fd = match name {
        Some(name) => open_at(dir, name, flags),
        None => rfs::openat(dir, ".", flags | OFlags::CLOEXEC, Mode::empty()).map_err(Into::into),
    }
    .map_err(fs_error)?;
    let info = stat_fd(&fd).map_err(fs_error)?;
    if !info.same_inode(expected) {
        return Err(changed());
    }
    Ok(fd)
}

/// Open the regular file `name` in `dir` for reading; it must still be
/// `expected`. A FIFO or device swapped in is refused without blocking.
pub fn open_file_at(dir: BorrowedFd<'_>, name: &str, expected: &Info) -> AppResult<File> {
    let fd = open_at(
        dir,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
    )
    .map_err(fs_error)?;
    let info = stat_fd(&fd).map_err(fs_error)?;
    if info.kind != Kind::File || !info.same_inode(expected) {
        return Err(changed());
    }
    let flags = rfs::fcntl_getfl(&fd).map_err(errno)?;
    rfs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK).map_err(errno)?;
    Ok(File::from(fd))
}

/// Whether this process may write `name` in `dir` (`.` for `dir` itself).
/// Real and effective ids of the agent are the same.
pub fn writable_at(dir: BorrowedFd<'_>, name: &str) -> bool {
    rfs::accessat(dir, name, rfs::Access::WRITE_OK, AtFlags::empty()).is_ok()
}

/// The entries of the folder `dir` (opened for reading) with their `stat`s,
/// in directory order. Names that are not UTF-8 (the API cannot address
/// them) and entries that vanished meanwhile are left out.
pub fn entries(dir: BorrowedFd<'_>) -> io::Result<Vec<(String, Info)>> {
    let reader = Dir::read_from(dir)?;
    let mut out = Vec::new();
    for entry in reader {
        let entry = entry?;
        let Some(name) = utf8_name(entry.file_name()) else {
            continue;
        };
        if let Ok(Some(info)) = stat_at(dir, name) {
            out.push((name.to_string(), info));
        }
    }
    Ok(out)
}

/// A directory entry's name, unless it is `.`, `..` or not UTF-8.
fn utf8_name(name: &CStr) -> Option<&str> {
    let name = name.to_str().ok()?;
    (name != "." && name != "..").then_some(name)
}

fn is_dot(name: &CStr) -> bool {
    matches!(name.to_bytes(), b"." | b"..")
}

/// Make the folder `name` in `dir` (the umask applies).
pub fn mkdir_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    if !component_ok(name) {
        return Err(invalid_component());
    }
    Ok(rfs::mkdirat(dir, name, Mode::from_raw_mode(0o777))?)
}

/// Open the folder `name` that this process has just made in `dir`. It must
/// still be an empty folder of this process's user: a folder renamed into
/// its place since is refused, so nothing is written into it.
pub fn open_created_dir<P: rustix::path::Arg + Copy>(
    dir: BorrowedFd<'_>,
    name: P,
) -> AppResult<OwnedFd> {
    let fd = openat_nofollow(dir, name, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .map_err(errno)?;
    let info = stat_fd(&fd).map_err(fs_error)?;
    let mut reader = Dir::read_from(&fd).map_err(errno)?;
    let empty = reader.all(|entry| entry.is_ok_and(|entry| is_dot(entry.file_name())));
    // SAFETY: geteuid has no preconditions.
    if info.uid != unsafe { libc::geteuid() } || !empty {
        return Err(AppError::conflict(
            "the new folder was replaced before it could be used",
        ));
    }
    Ok(fd)
}

/// Create the file `name` in `dir` with `mode`: nothing may exist under that
/// name, not even a link.
pub fn create_at(dir: BorrowedFd<'_>, name: &str, mode: u32) -> io::Result<File> {
    if !component_ok(name) {
        return Err(invalid_component());
    }
    let fd = rfs::openat(
        dir,
        name,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY
            | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode),
    )?;
    Ok(File::from(fd))
}

/// Rename `from` in `from_dir` to `to` in `to_dir`, never replacing an
/// entry (`EEXIST`). On file systems without `RENAME_NOREPLACE` the check
/// and the rename are two steps.
pub fn rename_noreplace(
    from_dir: BorrowedFd<'_>,
    from: &str,
    to_dir: BorrowedFd<'_>,
    to: &str,
) -> io::Result<()> {
    if !component_ok(from) || !component_ok(to) {
        return Err(invalid_component());
    }
    #[cfg(target_os = "linux")]
    match rfs::renameat_with(from_dir, from, to_dir, to, rfs::RenameFlags::NOREPLACE) {
        Err(Errno::INVAL) | Err(Errno::NOSYS) => {}
        other => return other.map_err(io::Error::from),
    }
    if stat_at(to_dir, to)?.is_some() {
        return Err(Errno::EXIST.into());
    }
    Ok(rfs::renameat(from_dir, from, to_dir, to)?)
}

/// Rename `from` in `from_dir` over `to` in `to_dir` (a file there is
/// replaced, a link there is replaced, not followed).
pub fn rename_over(
    from_dir: BorrowedFd<'_>,
    from: &str,
    to_dir: BorrowedFd<'_>,
    to: &str,
) -> io::Result<()> {
    if !component_ok(from) || !component_ok(to) {
        return Err(invalid_component());
    }
    Ok(rfs::renameat(from_dir, from, to_dir, to)?)
}

/// Remove the file (not a folder) `name` from `dir`.
pub fn unlink_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    if !component_ok(name) {
        return Err(invalid_component());
    }
    Ok(rfs::unlinkat(dir, name, AtFlags::empty())?)
}

/// Whether the folder `dir` is `ancestor` or lies below it. The walk up
/// (through `..`) stops at `root`, the namespace root.
pub fn is_within(dir: BorrowedFd<'_>, ancestor: &Info, root: &Info) -> io::Result<bool> {
    let mut current = rfs::openat(dir, ".", handle_flags(), Mode::empty())?;
    for _ in 0..4096 {
        let info = stat_fd(&current)?;
        if info.same_inode(ancestor) {
            return Ok(true);
        }
        if info.same_inode(root) {
            return Ok(false);
        }
        let parent = rfs::openat(&current, "..", handle_flags(), Mode::empty())?;
        if stat_fd(&parent)?.same_inode(&info) {
            return Ok(false);
        }
        current = parent;
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

/// What a walk does after visiting an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Go on, into the entry if it is a folder.
    Into,
    /// Go on, but not into this entry.
    Over,
    /// End the walk.
    Stop,
}

/// One entry of a walk.
pub struct Visit<'a> {
    /// 1 for the entries of the folder the walk started in.
    pub depth: usize,
    /// The folder that holds the entry.
    pub dir: BorrowedFd<'a>,
    pub name: &'a str,
    /// Names from the start folder to the entry, the entry's included.
    pub path: &'a [String],
    pub info: &'a Info,
}

/// Walk the tree below the folder `base` (opened for reading) depth first,
/// parents before their entries, through descriptors and never through a
/// link. Entries are visited up to `max_depth`. Names that are not UTF-8,
/// entries that vanish and folders that cannot be opened are left out.
pub fn walk(
    base: OwnedFd,
    max_depth: usize,
    mut visit: impl FnMut(&Visit<'_>) -> Step,
) -> io::Result<()> {
    struct Frame {
        dir: Dir,
        path: Vec<String>,
        depth: usize,
    }
    let mut stack = vec![Frame {
        dir: Dir::new(base)?,
        path: Vec::new(),
        depth: 0,
    }];
    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.dir.next() else {
            stack.pop();
            continue;
        };
        let Ok(entry) = entry else {
            stack.pop();
            continue;
        };
        let Some(name) = utf8_name(entry.file_name()) else {
            continue;
        };
        let Ok(dir) = frame.dir.fd() else {
            stack.pop();
            continue;
        };
        let Ok(Some(info)) = stat_at(dir, name) else {
            continue;
        };
        let mut path = frame.path.clone();
        path.push(name.to_string());
        let depth = frame.depth + 1;
        let step = visit(&Visit {
            depth,
            dir,
            name,
            path: &path,
            info: &info,
        });
        match step {
            Step::Stop => return Ok(()),
            Step::Over => {}
            Step::Into if info.is_dir() && depth < max_depth => {
                let opened = open_at(dir, name, OFlags::RDONLY | OFlags::DIRECTORY)
                    .ok()
                    .and_then(|fd| Dir::new(fd).ok());
                if let Some(dir) = opened {
                    stack.push(Frame { dir, path, depth });
                }
            }
            Step::Into => {}
        }
    }
    Ok(())
}

/// Bytes `name` in `dir` occupies as the server's quota counts them
/// (`quota::dir_used_bytes_blocking`): the regular files of a folder, 64
/// levels deep, without links and names that start with `skip_prefix`.
pub fn tree_bytes(dir: BorrowedFd<'_>, name: &str, info: &Info, skip_prefix: &str) -> u64 {
    match info.kind {
        Kind::File => info.len,
        Kind::Dir => {
            let Ok(fd) = open_listing(dir, Some(name), info) else {
                return 0;
            };
            let mut total = 0u64;
            let _ = walk(fd, 64, |visit| {
                if visit.info.kind == Kind::File && !visit.name.starts_with(skip_prefix) {
                    total = total.saturating_add(visit.info.len);
                }
                Step::Into
            });
            total
        }
        _ => 0,
    }
}

/// Totals of a copy.
#[derive(Debug, Default)]
pub struct Copied {
    pub bytes: u64,
    pub entries: usize,
}

/// Copy the entry `name` of `src`, whose `stat` was `info`, to the new entry
/// `name` in `dst`. Folders are copied through descriptors; links, special
/// files and names that start with `skip_prefix` inside them are left out.
/// `created` is set once the top entry exists, so the caller can remove a
/// partial copy (and nothing it did not create).
#[allow(clippy::too_many_arguments)]
pub fn copy_at(
    src: BorrowedFd<'_>,
    name: &str,
    info: &Info,
    dst: BorrowedFd<'_>,
    cancel: &AtomicBool,
    skip_prefix: &str,
    totals: &mut Copied,
    created: &mut Option<Info>,
) -> AppResult<()> {
    check(cancel)?;
    if !component_ok(name) {
        return Err(AppError::bad_request("invalid path component"));
    }
    match info.kind {
        Kind::Symlink => Err(AppError::Forbidden("symbolic links are not supported")),
        Kind::Other => Err(AppError::bad_request("special files cannot be copied")),
        Kind::File => {
            let name = std::ffi::CString::new(name)
                .map_err(|_| AppError::bad_request("invalid path component"))?;
            totals.bytes += copy_file(src, &name, info, dst, &name, created)?;
            Ok(())
        }
        Kind::Dir => copy_tree(src, name, info, dst, cancel, skip_prefix, totals, created),
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_tree(
    src: BorrowedFd<'_>,
    name: &str,
    info: &Info,
    dst: BorrowedFd<'_>,
    cancel: &AtomicBool,
    skip_prefix: &str,
    totals: &mut Copied,
    created: &mut Option<Info>,
) -> AppResult<()> {
    reach(Point::OpenSource, name.as_bytes());
    let source = open_listing(src, Some(name), info)?;
    mkdir_at(dst, name).map_err(fs_error)?;
    reach(Point::MadeDir, name.as_bytes());
    let target = open_created_dir(dst, name)?;
    *created = Some(stat_fd(&target).map_err(fs_error)?);
    let mut stack = vec![(Dir::new(source).map_err(errno)?, target)];
    loop {
        let depth = stack.len();
        let Some((dir, target)) = stack.last_mut() else {
            break;
        };
        check(cancel)?;
        let Some(entry) = dir.next() else {
            stack.pop();
            continue;
        };
        let entry = entry.map_err(errno)?;
        let child = entry.file_name();
        if is_dot(child) || child.to_bytes().starts_with(skip_prefix.as_bytes()) {
            continue;
        }
        totals.entries += 1;
        if totals.entries > MAX_TREE_ENTRIES {
            return Err(AppError::bad_request(
                "folder is too large for a single copy or move operation",
            ));
        }
        let at = dir.fd().map_err(errno)?;
        let child_info = match rfs::statat(at, child, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Info::from_stat(&stat),
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(errno(error)),
        };
        let target = target.as_fd();
        match child_info.kind {
            Kind::File => {
                totals.bytes += copy_file(at, child, &child_info, target, child, &mut None)?;
            }
            Kind::Dir => {
                if depth >= MAX_DEPTH {
                    return Err(AppError::bad_request(
                        "folder is nested too deeply for a copy or move",
                    ));
                }
                reach(Point::OpenSource, child.to_bytes());
                let source =
                    openat_nofollow(at, child, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
                        .map_err(errno)?;
                if !stat_fd(&source).map_err(fs_error)?.same_inode(&child_info) {
                    return Err(changed());
                }
                rfs::mkdirat(target, child, Mode::from_raw_mode(0o777)).map_err(errno)?;
                reach(Point::MadeDir, child.to_bytes());
                let made = open_created_dir(target, child)?;
                let frame = (Dir::new(source).map_err(errno)?, made);
                stack.push(frame);
            }
            // Links and special files inside a folder are left out.
            Kind::Symlink | Kind::Other => {}
        }
    }
    Ok(())
}

/// Copy the regular file `name` of `src` (still `expected`) to the new file
/// `to` in `dst`, with its permission bits (never set-uid or set-gid).
fn copy_file(
    src: BorrowedFd<'_>,
    name: &CStr,
    expected: &Info,
    dst: BorrowedFd<'_>,
    to: &CStr,
    created: &mut Option<Info>,
) -> AppResult<u64> {
    reach(Point::OpenSource, name.to_bytes());
    let input = rfs::openat(
        src,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno)?;
    let info = stat_fd(&input).map_err(fs_error)?;
    if info.kind != Kind::File || !info.same_inode(expected) {
        return Err(changed());
    }
    let flags = rfs::fcntl_getfl(&input).map_err(errno)?;
    rfs::fcntl_setfl(&input, flags - OFlags::NONBLOCK).map_err(errno)?;
    reach(Point::CreateTarget, to.to_bytes());
    let output = rfs::openat(
        dst,
        to,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY
            | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(errno)?;
    *created = Some(stat_fd(&output).map_err(fs_error)?);
    let mut input = File::from(input);
    let mut output = File::from(output);
    let bytes = io::copy(&mut input, &mut output).map_err(fs_error)?;
    rfs::fchmod(&output, Mode::from_raw_mode(info.mode & 0o777)).map_err(errno)?;
    Ok(bytes)
}

/// Remove `name` from `dir`, a folder with everything in it, never following
/// a link. A missing entry is not an error.
pub fn remove_at(dir: BorrowedFd<'_>, name: &str) -> AppResult<()> {
    let Some(info) = stat_at(dir, name).map_err(fs_error)? else {
        return Ok(());
    };
    if !info.is_dir() {
        return match unlink_at(dir, name) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            other => other.map_err(fs_error),
        };
    }
    struct Frame {
        dir: Dir,
        name: std::ffi::CString,
    }
    let top = open_listing(dir, Some(name), &info)?;
    let mut stack = vec![Frame {
        dir: Dir::new(top).map_err(errno)?,
        name: std::ffi::CString::new(name)
            .map_err(|_| AppError::bad_request("invalid path component"))?,
    }];
    loop {
        let depth = stack.len();
        let Some(frame) = stack.last_mut() else {
            break;
        };
        let Some(entry) = frame.dir.next() else {
            let done = stack.pop().expect("a frame");
            let parent = match stack.last() {
                Some(frame) => frame.dir.fd().map_err(errno)?,
                None => dir,
            };
            match rfs::unlinkat(parent, done.name.as_c_str(), AtFlags::REMOVEDIR) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(errno(error)),
            }
            continue;
        };
        let entry = entry.map_err(errno)?;
        let child = entry.file_name();
        if is_dot(child) {
            continue;
        }
        let at = frame.dir.fd().map_err(errno)?;
        let child_info = match rfs::statat(at, child, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Info::from_stat(&stat),
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(errno(error)),
        };
        if child_info.is_dir() {
            if depth >= MAX_DEPTH {
                return Err(AppError::bad_request(
                    "folder is nested too deeply to be removed",
                ));
            }
            let fd = openat_nofollow(at, child, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
                .map_err(errno)?;
            let frame = Frame {
                dir: Dir::new(fd).map_err(errno)?,
                name: child.to_owned(),
            };
            stack.push(frame);
        } else {
            match rfs::unlinkat(at, child, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(errno(error)),
            }
        }
    }
    Ok(())
}

/// Remove what a failed copy created at `name` in `dir`, if it is still
/// that entry.
pub fn remove_created(dir: BorrowedFd<'_>, name: &str, created: &Info) {
    if matches!(stat_at(dir, name), Ok(Some(info)) if info.same_inode(created)) {
        let _ = remove_at(dir, name);
    }
}

/// Set the permission bits of an open file.
pub fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    Ok(rfs::fchmod(file, Mode::from_raw_mode(mode))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn segments(path: &str) -> Vec<String> {
        path.split('/').map(str::to_string).collect()
    }

    /// A root with `docs/a.txt`, an outside folder with a secret, and links
    /// to it in the middle and at the end of paths.
    fn tree() -> (tempfile::TempDir, OwnedFd) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.txt"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("outside")).unwrap();
        std::fs::write(dir.path().join("outside/secret.txt"), b"secret").unwrap();
        symlink(dir.path().join("outside"), root.join("away")).unwrap();
        symlink(
            dir.path().join("outside/secret.txt"),
            root.join("docs/note.txt"),
        )
        .unwrap();
        symlink("a.txt", root.join("docs/inside.txt")).unwrap();
        let (fd, _) = open_root(&root).unwrap();
        (dir, fd)
    }

    fn assert_refused(result: io::Result<(OwnedFd, Info)>, errno: i32, what: &str) {
        match result {
            Err(error) => assert_eq!(error.raw_os_error(), Some(errno), "{what}: {error}"),
            Ok(_) => panic!("{what} was opened"),
        }
    }

    #[test]
    fn no_link_is_followed_and_nothing_outside_is_reached() {
        let (_dir, root) = tree();
        let root = root.as_fd();
        let (_, info) = open_beneath(root, &segments("docs/a.txt"), OFlags::RDONLY).unwrap();
        assert_eq!(info.kind, Kind::File);
        for (path, flags) in [
            ("away", handle_flags()),
            ("away/secret.txt", OFlags::RDONLY),
            ("docs/note.txt", OFlags::RDONLY),
            ("docs/note.txt", handle_flags()),
            // Even a link that stays inside the root.
            ("docs/inside.txt", OFlags::RDONLY),
        ] {
            assert_refused(
                open_beneath(root, &segments(path), flags),
                libc::ELOOP,
                path,
            );
            // The same without openat2.
            let walked =
                open_walk(root, &segments(path), flags | OFlags::NOFOLLOW).and_then(|fd| {
                    let info = stat_fd(&fd)?;
                    if info.kind == Kind::Symlink {
                        return Err(Errno::LOOP.into());
                    }
                    Ok((fd, info))
                });
            assert_refused(walked, libc::ELOOP, path);
        }
        for path in ["..", "docs/../..", "/etc", "docs//a.txt", "."] {
            let parts: Vec<String> = path.split('/').map(str::to_string).collect();
            let error = open_beneath(root, &parts, OFlags::RDONLY).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{path}");
        }
        assert!(matches!(
            fs_error(Errno::LOOP.into()),
            AppError::Forbidden("symbolic links are not supported")
        ));
    }

    #[test]
    fn new_entries_never_replace_or_follow_anything() {
        let (dir, root) = tree();
        let (docs, _) = open_dir(root.as_fd(), &segments("docs")).unwrap();
        // A link at the name is not followed: the file is not created.
        let error = create_at(docs.as_fd(), "note.txt", 0o600).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(dir.path().join("outside/secret.txt")).unwrap(),
            b"secret"
        );
        let error =
            rename_noreplace(docs.as_fd(), "a.txt", docs.as_fd(), "inside.txt").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        // A folder renamed into the place of a new one is not used.
        mkdir_at(docs.as_fd(), "made").unwrap();
        assert!(open_created_dir(docs.as_fd(), "made").is_ok());
        std::fs::write(dir.path().join("root/docs/made/x"), b"x").unwrap();
        assert!(open_created_dir(docs.as_fd(), "made").is_err());
    }

    #[test]
    fn trees_are_walked_copied_and_removed_without_following_links() {
        let (dir, root) = tree();
        let root_path = dir.path().join("root");
        std::fs::create_dir_all(root_path.join("docs/sub")).unwrap();
        std::fs::write(root_path.join("docs/sub/b.txt"), b"bb").unwrap();
        std::fs::write(root_path.join("docs/.rast-tmp-x"), b"tmp").unwrap();
        symlink(dir.path().join("outside"), root_path.join("docs/sub/away")).unwrap();
        let (docs_info, root_info) = (
            stat_at(root.as_fd(), "docs").unwrap().unwrap(),
            stat_fd(&root).unwrap(),
        );
        // Sizes count regular files only (not links, not temporary files).
        assert_eq!(
            tree_bytes(root.as_fd(), "docs", &docs_info, ".rast-tmp-"),
            3
        );
        // A copy leaves links and temporary files out.
        mkdir_at(root.as_fd(), "copy").unwrap();
        let (copy, _) = open_dir(root.as_fd(), &segments("copy")).unwrap();
        let (mut totals, mut created) = (Copied::default(), None);
        copy_at(
            root.as_fd(),
            "docs",
            &docs_info,
            copy.as_fd(),
            &AtomicBool::new(false),
            ".rast-tmp-",
            &mut totals,
            &mut created,
        )
        .unwrap();
        assert_eq!(totals.bytes, 3);
        assert!(created.is_some());
        assert_eq!(
            std::fs::read(root_path.join("copy/docs/sub/b.txt")).unwrap(),
            b"bb"
        );
        for left_out in ["note.txt", "inside.txt", "sub/away", ".rast-tmp-x"] {
            assert!(
                std::fs::symlink_metadata(root_path.join("copy/docs").join(left_out)).is_err(),
                "{left_out}"
            );
        }
        // A copy into itself is recognised through the descriptors.
        let (sub, _) = open_dir(root.as_fd(), &segments("docs/sub")).unwrap();
        assert!(is_within(sub.as_fd(), &docs_info, &root_info).unwrap());
        assert!(!is_within(copy.as_fd(), &docs_info, &root_info).unwrap());
        // Removing a tree removes links, never what they point to.
        remove_at(root.as_fd(), "docs").unwrap();
        assert!(!root_path.join("docs").exists());
        assert_eq!(
            std::fs::read(dir.path().join("outside/secret.txt")).unwrap(),
            b"secret"
        );
    }
}
