//! The one HOME of a RAST user and its per-user file agent (ADR 0005).
//!
//! * `home_ensure` creates or repairs `<homes>/<username>`, owned by the
//!   identity `rast-u<id>` with mode 0700, and makes it the identity's passwd
//!   home (`usermod --home`, never `--move-home`). A HOME still owned by the
//!   `rast` service is a legacy home that only the migration may take over;
//!   any other owner, or a symlink in its place, is a conflict.
//! * `files_agent_start` starts the file agent (`rast file-agent`) as the
//!   identity in a socket-activated transient unit `rast-files-u<uid>`.
//!   systemd creates the socket (`root:rast 0660`) in a `root:rast 0750`
//!   directory, so the user's own processes can neither reach nor replace it.
//!   The agent gets its token through `LoadCredential=` and also checks the
//!   peer uid of every connection.
//! * `files_agent_stop` stops the agent and its socket.
//!
//! No operation takes a path: the helper builds every path from its
//! configuration, the validated user id and the validated username. Work
//! inside a HOME goes through directory file descriptors opened with
//! `O_NOFOLLOW`, so a symlink the user planted is never followed by root.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::native::daemon::{random_token, run_systemd_unit, stop_unit, unit_active};
use crate::native::identity::{getent_passwd, Identity, PasswdEntry};
use crate::native::protocol::{
    validate_home_username, validate_unit, FilesAgentOp, HomeEnsureOp, NativeError,
};
use crate::native::{run_command, NativeOptions, LINUX_SLICE};

/// Unit name prefix of the per-user file agents (`rast-files-u<uid>`).
pub const FILES_UNIT_PREFIX: &str = "rast-files-u";
/// The agent stops itself after this long without a request (and without
/// any other process of the user).
pub const FILES_AGENT_IDLE_SECONDS: u64 = 900;
/// Memory and task limits of one file agent.
pub const FILES_AGENT_MEMORY_MAX: &str = "512M";
pub const FILES_AGENT_TASKS_MAX: u32 = 64;
/// Folders `home_ensure` creates in a HOME (ADR 0004: `~/.local/bin` is on
/// the session `PATH`). Parents come before their children.
pub const SKELETON: &[&str] = &[
    ".config",
    ".cache",
    ".local",
    ".local/share",
    ".local/state",
    ".local/bin",
];
/// Timeout of `usermod`.
const USERMOD_TIMEOUT: Duration = Duration::from_secs(45);
/// Folder in HOME that holds the `~/Shared/<name>` links to shared folders.
/// They are for apps only: the agent never follows them (ADR 0005).
pub const SHARED_DIR: &str = "Shared";

/// The container a `~/Shared/<name>` link of RAST names: the link's
/// `target` must be a direct child of `shared_root` (the canonical shared
/// root, as the links name it). The file agent only creates and removes
/// such links; anything else in `~/Shared` is the user's.
pub fn share_link_container<'a>(target: &'a Path, shared_root: &Path) -> Option<&'a OsStr> {
    (target.parent() == Some(shared_root))
        .then(|| target.file_name())
        .flatten()
}

/// Who owns a HOME directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Identity,
    Legacy,
    Other(u32),
}

/// The uid that owns a HOME in one-HOME mode. Tests run as one account and
/// cannot chown, so they name the owner that stands for the identity.
fn identity_owner_uid(options: &NativeOptions, identity: &Identity) -> u32 {
    #[cfg(test)]
    if let Some(uid) = options.test_identity_uid {
        return uid;
    }
    let _ = options;
    identity.uid
}

fn classify(options: &NativeOptions, identity: &Identity, uid: u32) -> Owner {
    if uid == identity_owner_uid(options, identity) {
        Owner::Identity
    } else if Some(uid) == options.rast_uid {
        Owner::Legacy
    } else {
        Owner::Other(uid)
    }
}

/// The uid allowed to connect to a file agent: the `rast` service.
fn service_uid(options: &NativeOptions) -> Result<u32, NativeError> {
    if let Some(uid) = options.rast_uid {
        return Ok(uid);
    }
    if options.is_test_mode() {
        // SAFETY: geteuid has no preconditions.
        return Ok(unsafe { libc::geteuid() });
    }
    Err(NativeError::unavailable(
        "the rast service account does not exist; reinstall RAST",
    ))
}

// ---------------------------------------------------------------------------
// Directory file descriptors (no symlink is ever followed)
// ---------------------------------------------------------------------------

pub(crate) fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

pub(crate) fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

pub(crate) const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Open a configured directory (its last component must not be a symlink).
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let path = c_path(path)?;
    // SAFETY: `path` is NUL-terminated; the returned fd is owned below.
    let fd = unsafe { libc::open(path.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `name` below `dir`, refusing a symlink.
pub(crate) fn open_dir_at(dir: RawFd, name: &str) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: `dir` is a valid directory fd and `name` is NUL-terminated.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `lstat` of `name` below `dir`.
pub(crate) fn stat_at(dir: RawFd, name: &str) -> io::Result<libc::stat> {
    let name = c_name(name)?;
    // SAFETY: zeroed `stat` is a valid out parameter.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd, NUL-terminated name, valid out pointer.
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

pub(crate) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed `stat` is a valid out parameter.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out pointer.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

pub(crate) fn mkdir_at(dir: RawFd, name: &str, mode: libc::mode_t) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    after_mkdir(dir, name.to_str().unwrap_or_default());
    Ok(())
}

#[cfg(test)]
pub(crate) type MkdirHook = Box<dyn FnMut(RawFd, &str)>;

#[cfg(test)]
thread_local! {
    /// Test hook that runs right after `mkdirat`, so a test can rename
    /// another directory into the place of the one just created.
    static AFTER_MKDIR: std::cell::RefCell<Option<MkdirHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Set (or clear) this thread's [`AFTER_MKDIR`] hook, for the tests of
/// other modules that create folders with [`mkdir_at`].
#[cfg(test)]
pub(crate) fn set_after_mkdir(hook: Option<MkdirHook>) {
    AFTER_MKDIR.with(|slot| *slot.borrow_mut() = hook);
}

fn after_mkdir(_dir: RawFd, _name: &str) {
    #[cfg(test)]
    AFTER_MKDIR.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(_dir, _name);
        }
    });
}

/// Clear `errno`, so a NULL from `readdir` tells the end of a directory
/// apart from an error (elsewhere a stale value fails closed).
pub(crate) fn clear_errno() {
    // SAFETY: errno is thread-local and always writable.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
    // SAFETY: as above.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    unsafe {
        *libc::__error() = 0;
    }
}

/// Whether the directory `fd` has no entries besides `.` and `..`.
pub(crate) fn dir_is_empty(fd: RawFd) -> io::Result<bool> {
    // A new open file description, so reading it moves no shared offset.
    let own = open_dir_at(fd, ".")?;
    // SAFETY: `own` is a valid directory fd; `fdopendir` takes it over and
    // `closedir` below closes it.
    let dir = unsafe { libc::fdopendir(own.as_raw_fd()) };
    if dir.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(own);
    let mut empty = true;
    loop {
        clear_errno();
        // SAFETY: `dir` is a valid, open DIR stream.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `dir` is valid and closed exactly once.
            unsafe { libc::closedir(dir) };
            return match error.raw_os_error() {
                Some(0) | None => Ok(empty),
                Some(_) => Err(error),
            };
        }
        // SAFETY: `d_name` of a returned entry is NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            empty = false;
        }
    }
}

/// Open the directory `name` that this process has just created below
/// `dir`, and check that it is still that directory: owned by the effective
/// uid and empty. The parent belongs to someone else (the user's HOME, or
/// the service's homes root), who may have renamed another directory into
/// its place after `mkdirat`; root must never chown that one.
pub(crate) fn open_created_dir(dir: RawFd, name: &str) -> io::Result<OwnedFd> {
    let fd = open_dir_at(dir, name)?;
    let stat = fstat(fd.as_raw_fd())?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if stat.st_uid != euid || !dir_is_empty(fd.as_raw_fd())? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the new folder was replaced before it could be taken over",
        ));
    }
    Ok(fd)
}

pub(crate) fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: valid fd.
    if unsafe { libc::fchown(fd, uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn fchmod(fd: RawFd, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: valid fd.
    if unsafe { libc::fchmod(fd, mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn is_dir(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}

pub(crate) fn is_symlink(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFLNK
}

pub(crate) fn failed(what: &str, path: &Path, error: io::Error) -> NativeError {
    NativeError::command_failed(format!("cannot {what} {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// home_ensure
// ---------------------------------------------------------------------------

/// The identity's passwd entry, which must exist with the derived ids.
pub(crate) async fn identity_entry(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<PasswdEntry, NativeError> {
    let entry = getent_passwd(options, &identity.username)
        .await?
        .ok_or_else(|| {
            NativeError::not_found(format!(
                "the Linux account {} does not exist; run identity_ensure first",
                identity.username
            ))
        })?;
    if entry.uid != identity.uid || entry.gid != identity.gid {
        return Err(NativeError::identity_conflict(format!(
            "user {} has uid/gid {}/{} but RAST expects {}/{}",
            identity.username, entry.uid, entry.gid, identity.uid, identity.gid
        )));
    }
    Ok(entry)
}

/// Refuse a HOME that is not a real directory owned by the identity.
fn check_owner(
    options: &NativeOptions,
    identity: &Identity,
    home: &Path,
    stat: &libc::stat,
) -> Result<(), NativeError> {
    if is_symlink(stat) || !is_dir(stat) {
        return Err(NativeError::identity_conflict(format!(
            "{} is not a real directory",
            home.display()
        )));
    }
    match classify(options, identity, stat.st_uid) {
        Owner::Identity => Ok(()),
        Owner::Legacy => Err(NativeError::new(
            "legacy_home",
            format!(
                "{} still belongs to the RAST service; `rast migrate home` moves it to {}",
                home.display(),
                identity.username
            ),
        )),
        Owner::Other(uid) => Err(NativeError::identity_conflict(format!(
            "{} is owned by uid {uid}, not by {}",
            home.display(),
            identity.username
        ))),
    }
}

/// Create the skeleton folders that are missing. Existing entries are never
/// changed; a symlink, a non-directory or a folder owned by someone else is
/// reported and not descended into.
fn ensure_skeleton(
    options: &NativeOptions,
    identity: &Identity,
    home: &OwnedFd,
) -> (Vec<String>, Vec<String>) {
    let mut created = Vec::new();
    let mut skipped = Vec::new();
    'entries: for relative in SKELETON {
        let parts: Vec<&str> = relative.split('/').collect();
        let (name, parents) = parts.split_last().expect("skeleton entries are not empty");
        // Walk to the parent through no-follow directory descriptors.
        let mut owned: Vec<OwnedFd> = Vec::new();
        for parent in parents {
            let base = owned
                .last()
                .map(AsRawFd::as_raw_fd)
                .unwrap_or(home.as_raw_fd());
            match stat_at(base, parent) {
                Ok(stat)
                    if is_dir(&stat)
                        && classify(options, identity, stat.st_uid) == Owner::Identity => {}
                _ => continue 'entries,
            }
            match open_dir_at(base, parent) {
                Ok(fd) => owned.push(fd),
                Err(_) => continue 'entries,
            }
        }
        let base = owned
            .last()
            .map(AsRawFd::as_raw_fd)
            .unwrap_or(home.as_raw_fd());
        match stat_at(base, name) {
            Ok(stat) => {
                if is_symlink(&stat)
                    || !is_dir(&stat)
                    || classify(options, identity, stat.st_uid) != Owner::Identity
                {
                    skipped.push((*relative).to_string());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if mkdir_at(base, name, 0o700).is_err() {
                    skipped.push((*relative).to_string());
                    continue;
                }
                // Chown through a descriptor of the folder just created. A
                // symlink swapped in between is refused by the open, another
                // directory by the owner and emptiness check.
                match open_created_dir(base, name) {
                    Ok(fd) => {
                        if !options.is_test_mode() {
                            let _ = fchown(fd.as_raw_fd(), identity.uid, identity.gid);
                        }
                        created.push((*relative).to_string());
                    }
                    Err(_) => skipped.push((*relative).to_string()),
                }
            }
            Err(_) => skipped.push((*relative).to_string()),
        }
    }
    (created, skipped)
}

/// The one HOME of `identity`, when its passwd entry names one: a folder
/// directly below the homes root, with a valid HOME name, that is a real
/// directory (no link followed) owned by the identity. Anything else keeps
/// the identity on its legacy home.
pub(crate) fn one_home_of(
    options: &NativeOptions,
    identity: &Identity,
    entry: &PasswdEntry,
) -> Option<PathBuf> {
    let home = &entry.home;
    if home.parent() != Some(options.homes_dir.as_path()) {
        return None;
    }
    let name = home.file_name()?.to_str()?;
    validate_home_username(name).ok()?;
    let homes = open_dir(&options.homes_dir).ok()?;
    let stat = stat_at(homes.as_raw_fd(), name).ok()?;
    (is_dir(&stat) && classify(options, identity, stat.st_uid) == Owner::Identity)
        .then(|| home.clone())
}

/// Check (and repair the mode of) the one HOME of an identity in one-HOME
/// mode: a real directory owned by the identity, mode 0700. Opened below
/// the homes root with `O_NOFOLLOW`, changed through that descriptor.
pub(crate) fn ensure_one_home(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<(), NativeError> {
    let home = &identity.home;
    let name = home
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|_| home.parent() == Some(options.homes_dir.as_path()))
        .ok_or_else(|| {
            NativeError::identity_conflict(format!(
                "{} is not a HOME below {}",
                home.display(),
                options.homes_dir.display()
            ))
        })?;
    let homes =
        open_dir(&options.homes_dir).map_err(|error| failed("open", &options.homes_dir, error))?;
    let fd = open_dir_at(homes.as_raw_fd(), name).map_err(|error| failed("open", home, error))?;
    let stat = fstat(fd.as_raw_fd()).map_err(|error| failed("inspect", home, error))?;
    check_owner(options, identity, home, &stat)?;
    if stat.st_mode & 0o7777 != 0o700 {
        fchmod(fd.as_raw_fd(), 0o700).map_err(|error| failed("chmod", home, error))?;
        tracing::info!(
            user_id = identity.user_id,
            home = %home.display(),
            previous_mode = format!("{:04o}", stat.st_mode & 0o7777),
            "repaired the mode of a one HOME"
        );
    }
    Ok(())
}

pub(crate) async fn home_ensure(
    options: &NativeOptions,
    op: HomeEnsureOp,
) -> Result<Value, NativeError> {
    let identity = Identity::derive(options, op.user_id)?;
    validate_home_username(&op.username)?;
    let entry = identity_entry(options, &identity).await?;
    let homes = options.homes_dir.clone();
    let home = homes.join(&op.username);
    let (created, skeleton, skipped) = {
        let homes_fd = open_dir(&homes).map_err(|error| failed("open", &homes, error))?;
        let created = match stat_at(homes_fd.as_raw_fd(), &op.username) {
            Ok(stat) => {
                check_owner(options, &identity, &home, &stat)?;
                false
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                mkdir_at(homes_fd.as_raw_fd(), &op.username, 0o700)
                    .map_err(|error| failed("create", &home, error))?;
                true
            }
            Err(error) => return Err(failed("inspect", &home, error)),
        };
        let home_fd = if created {
            open_created_dir(homes_fd.as_raw_fd(), &op.username)
        } else {
            open_dir_at(homes_fd.as_raw_fd(), &op.username)
        }
        .map_err(|error| failed("open", &home, error))?;
        if created && !options.is_test_mode() {
            fchown(home_fd.as_raw_fd(), identity.uid, identity.gid)
                .map_err(|error| failed("chown", &home, error))?;
        }
        // What was opened must be what was checked (or just created).
        let stat = fstat(home_fd.as_raw_fd()).map_err(|error| failed("inspect", &home, error))?;
        if !created {
            check_owner(options, &identity, &home, &stat)?;
        }
        fchmod(home_fd.as_raw_fd(), 0o700).map_err(|error| failed("chmod", &home, error))?;
        let (skeleton, skipped) = ensure_skeleton(options, &identity, &home_fd);
        (created, skeleton, skipped)
    };
    let passwd_updated = entry.home != home;
    if passwd_updated {
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
    }
    tracing::info!(
        user_id = identity.user_id,
        created,
        passwd_updated,
        "one HOME ensured"
    );
    Ok(json!({
        "userId": identity.user_id,
        "username": identity.username,
        "uid": identity.uid,
        "gid": identity.gid,
        "home": home.display().to_string(),
        "created": created,
        "passwdUpdated": passwd_updated,
        "skeleton": skeleton,
        "skipped": skipped,
    }))
}

// ---------------------------------------------------------------------------
// files_agent_start / files_agent_stop
// ---------------------------------------------------------------------------

fn files_unit(identity: &Identity) -> Result<String, NativeError> {
    let unit = format!("{FILES_UNIT_PREFIX}{}", identity.uid);
    validate_unit(&unit)?;
    Ok(unit)
}

fn files_dir(options: &NativeOptions) -> PathBuf {
    options.run_dir.join("files")
}

/// The HOME the agent serves: the identity's passwd home, which must be a
/// direct child of the homes root and owned by the identity.
async fn agent_home(options: &NativeOptions, identity: &Identity) -> Result<PathBuf, NativeError> {
    let entry = identity_entry(options, identity).await?;
    let home = entry.home;
    let name = home
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let Some(name) = name.filter(|_| home.parent() == Some(options.homes_dir.as_path())) else {
        return Err(NativeError::new(
            "legacy_home",
            format!(
                "{} has no one HOME yet (its home is {}); run home_ensure first",
                identity.username,
                home.display()
            ),
        ));
    };
    validate_home_username(&name)?;
    let homes_fd =
        open_dir(&options.homes_dir).map_err(|error| failed("open", &options.homes_dir, error))?;
    let stat =
        stat_at(homes_fd.as_raw_fd(), &name).map_err(|error| failed("inspect", &home, error))?;
    check_owner(options, identity, &home, &stat)?;
    Ok(home)
}

/// `<run_dir>/files`, `root:rast 0750`: the service group reaches the
/// sockets, the users' own processes do not.
fn prepare_files_dir(options: &NativeOptions, gid: u32) -> Result<PathBuf, NativeError> {
    use std::os::unix::fs::PermissionsExt;
    let dir = files_dir(options);
    std::fs::create_dir_all(&dir).map_err(|error| failed("create", &dir, error))?;
    let meta = std::fs::symlink_metadata(&dir).map_err(|error| failed("inspect", &dir, error))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(NativeError::forbidden(
            "the file agent runtime directory must be a real directory",
        ));
    }
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&dir, Some(0), Some(gid))
            .map_err(|error| failed("chown", &dir, error))?;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))
        .map_err(|error| failed("chmod", &dir, error))?;
    Ok(dir)
}

fn read_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (token.len() >= 32 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_string())
}

fn write_token(path: &Path, token: &str) -> Result<(), NativeError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| failed("write", path, error))?;
    file.write_all(token.as_bytes())
        .map_err(|error| failed("write", path, error))
}

/// The `systemd-run` arguments of one file agent.
#[allow(clippy::too_many_arguments)]
pub fn files_agent_args(
    unit: &str,
    identity: &Identity,
    home: &Path,
    shared_root: &Path,
    socket: &Path,
    socket_gid: u32,
    token_file: &Path,
    peer_uid: u32,
    exe: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |value: String| args.push(OsString::from(value));
    push(format!("--unit={unit}"));
    push("--collect".into());
    push("--quiet".into());
    push(format!("--slice={LINUX_SLICE}"));
    // systemd (root) creates the socket; the agent inherits it.
    push(format!(
        "--socket-property=ListenStream={}",
        socket.display()
    ));
    push("--socket-property=SocketUser=root".into());
    push(format!("--socket-property=SocketGroup={socket_gid}"));
    push("--socket-property=SocketMode=0660".into());
    push("--socket-property=RemoveOnStop=yes".into());
    push("--property=Type=exec".into());
    push(format!("--property=User={}", identity.username));
    push(format!("--property=Group={}", identity.username));
    push(format!("--property=WorkingDirectory={}", home.display()));
    push("--property=UMask=0022".into());
    push(format!(
        "--property=LoadCredential=token:{}",
        token_file.display()
    ));
    push("--property=NoNewPrivileges=yes".into());
    push("--property=ProtectSystem=strict".into());
    push(format!("--property=ReadWritePaths={}", home.display()));
    push(format!(
        "--property=ReadWritePaths={}",
        shared_root.display()
    ));
    push("--property=PrivateTmp=yes".into());
    push("--property=PrivateNetwork=yes".into());
    push("--property=RestrictAddressFamilies=AF_UNIX".into());
    push("--property=CapabilityBoundingSet=".into());
    push("--property=AmbientCapabilities=".into());
    push(format!("--property=MemoryMax={FILES_AGENT_MEMORY_MAX}"));
    push(format!("--property=TasksMax={FILES_AGENT_TASKS_MAX}"));
    push("--property=KillMode=control-group".into());
    push("--".into());
    args.push(exe.as_os_str().to_owned());
    for value in [
        "file-agent".to_string(),
        "--home".into(),
        home.display().to_string(),
        "--shared-root".into(),
        shared_root.display().to_string(),
        "--peer-uid".into(),
        peer_uid.to_string(),
        "--idle-seconds".into(),
        FILES_AGENT_IDLE_SECONDS.to_string(),
    ] {
        args.push(OsString::from(value));
    }
    args
}

pub(crate) async fn files_agent_start(
    options: &NativeOptions,
    op: FilesAgentOp,
) -> Result<Value, NativeError> {
    let identity = Identity::derive(options, op.user_id)?;
    let unit = files_unit(&identity)?;
    let home = agent_home(options, &identity).await?;
    let peer_uid = service_uid(options)?;
    let gid = options.effective_rast_gid()?;
    let dir = prepare_files_dir(options, gid)?;
    let socket = dir.join(format!("u{}.sock", identity.uid));
    let token_file = dir.join(format!("u{}.token", identity.uid));
    let reply = |token: &str| {
        json!({
            "unit": unit,
            "socket": socket.display().to_string(),
            "token": token,
        })
    };
    let socket_unit = format!("{unit}.socket");
    if socket.exists() && unit_active(options, &socket_unit).await? {
        if let Some(token) = read_token(&token_file) {
            return Ok(reply(&token));
        }
    }
    let _ = stop_unit(options, &socket_unit).await;
    let _ = stop_unit(options, &format!("{unit}.service")).await;
    let _ = std::fs::remove_file(&socket);
    let token = random_token();
    write_token(&token_file, &token)?;
    let exe = std::env::current_exe().map_err(|error| {
        NativeError::command_failed(format!(
            "cannot locate the RAST binary for the file agent: {error}"
        ))
    })?;
    let shared_root = options.storage_dir.join("shared");
    let args = files_agent_args(
        &unit,
        &identity,
        &home,
        &shared_root,
        &socket,
        gid,
        &token_file,
        peer_uid,
        &exe,
    );
    run_systemd_unit(options, &unit, args).await?;
    for _ in 0..50 {
        if socket.exists() {
            tracing::info!(unit = %unit, user_id = identity.user_id, "file agent socket ready");
            return Ok(reply(&token));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop_unit(options, &socket_unit).await;
    Err(NativeError::command_failed(format!(
        "the file agent socket did not appear; check `journalctl -u {unit}`"
    )))
}

/// Stop the file agent of a uid and remove its token (identity removal).
pub(crate) async fn stop_files_agent(options: &NativeOptions, uid: u32) {
    let unit = format!("{FILES_UNIT_PREFIX}{uid}");
    if validate_unit(&unit).is_err() {
        return;
    }
    let _ = stop_unit(options, &format!("{unit}.socket")).await;
    let _ = stop_unit(options, &format!("{unit}.service")).await;
    let dir = files_dir(options);
    let _ = std::fs::remove_file(dir.join(format!("u{uid}.token")));
}

pub(crate) async fn files_agent_stop(
    options: &NativeOptions,
    op: FilesAgentOp,
) -> Result<Value, NativeError> {
    let identity = Identity::derive(options, op.user_id)?;
    let unit = files_unit(&identity)?;
    stop_unit(options, &format!("{unit}.socket")).await?;
    stop_unit(options, &format!("{unit}.service")).await?;
    let dir = files_dir(options);
    let _ = std::fs::remove_file(dir.join(format!("u{}.token", identity.uid)));
    Ok(json!({ "unit": unit, "stopped": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_share_link_names_a_direct_child_of_the_shared_root() {
        let root = Path::new("/srv/rast/shared");
        let container = |target: &'static str| share_link_container(Path::new(target), root);
        assert_eq!(container("/srv/rast/shared/team"), Some(OsStr::new("team")));
        assert_eq!(container("/srv/rast/shared/team/sub"), None);
        assert_eq!(container("/srv/rast/shared"), None);
        assert_eq!(container("/srv/rast/other/team"), None);
        assert_eq!(container("shared/team"), None);
        assert_eq!(container("/srv/rast/shared/.."), None);
    }

    struct Fixture {
        dir: tempfile::TempDir,
        options: NativeOptions,
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn logged(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('\u{1f}').map(str::to_string).collect())
            .collect()
    }

    /// A script that logs its argv (unit separators) to `<name>.log`.
    fn logging_script(dir: &Path, name: &str, extra: &str) -> PathBuf {
        let log = dir.join(format!("{name}.log"));
        write_script(
            dir,
            name,
            &format!(
                "#!/bin/sh\nline=\"\"\nsep=\"\"\nfor arg in \"$@\"; do line=\"$line$sep$arg\"; \
                 sep=$(printf '\\037'); done\nprintf '%s\\n' \"$line\" >> \"{log}\"\n{extra}\nexit 0\n",
                log = log.display()
            ),
        )
    }

    /// Helper options with fake `getent` (rast-u7 whose passwd home is
    /// `passwd_home`), `usermod`, `systemctl` and `systemd-run`.
    fn fixture(passwd_home: impl Fn(&Path) -> PathBuf) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let data = root.join("data");
        let run = root.join("run");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let mut options = NativeOptions::for_test(root.join("native.sock"), data, run);
        options.homes_dir = root.join("storage/homes");
        options.storage_dir = root.join("storage");
        std::fs::create_dir_all(&options.homes_dir).unwrap();
        std::fs::create_dir_all(root.join("storage/shared")).unwrap();
        options.test_identity_uid = Some(euid());
        options.rast_uid = Some(euid().wrapping_add(1));
        let home = passwd_home(root);
        options.getent_bin = write_script(
            root,
            "getent",
            &format!(
                "#!/bin/sh\n[ -f '{root}/no-user' ] && exit 2\n\
                 echo 'rast-u7:x:200007:200007:RAST Linux app:{home}:/usr/sbin/nologin'\n",
                root = root.display(),
                home = home.display()
            ),
        );
        options.usermod_bin = logging_script(root, "usermod", "");
        options.systemctl_bin = logging_script(
            root,
            "systemctl",
            &format!(
                "[ \"$1\" = show ] && [ -f '{root}/active' ] && \
                 printf 'Id=%s\\nActiveState=active\\nLoadState=loaded\\nMainPID=0\\n' \"$2\"",
                root = root.display()
            ),
        );
        // systemd creates the socket: the fake touches the ListenStream path.
        options.systemd_run_bin = logging_script(
            root,
            "systemd-run",
            "for arg in \"$@\"; do case \"$arg\" in --socket-property=ListenStream=*) \
             touch \"${arg#--socket-property=ListenStream=}\";; esac; done",
        );
        Fixture { dir, options }
    }

    fn op(username: &str) -> HomeEnsureOp {
        HomeEnsureOp {
            user_id: 7,
            username: username.into(),
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[tokio::test]
    async fn home_ensure_creates_repairs_and_is_idempotent() {
        let fx = fixture(|root| root.join("data/linux-users/7"));
        let home = fx.options.homes_dir.join("alice");
        let first = home_ensure(&fx.options, op("alice")).await.unwrap();
        assert_eq!(first["created"], true);
        assert_eq!(first["home"], home.display().to_string());
        assert_eq!(first["passwdUpdated"], true);
        assert_eq!(mode(&home), 0o700);
        for relative in SKELETON {
            assert!(home.join(relative).is_dir(), "{relative}");
            assert_eq!(mode(&home.join(relative)), 0o700, "{relative}");
        }
        assert_eq!(first["skeleton"].as_array().unwrap().len(), SKELETON.len());
        // `usermod --home`, never `--move-home`.
        let usermod = logged(&fx.dir.path().join("usermod.log"));
        assert_eq!(
            usermod,
            vec![vec![
                "--home".to_string(),
                home.display().to_string(),
                "rast-u7".to_string()
            ]]
        );

        // A loose mode is repaired; nothing is created twice.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let second = home_ensure(&fx.options, op("alice")).await.unwrap();
        assert_eq!(second["created"], false);
        assert_eq!(second["skeleton"].as_array().unwrap().len(), 0);
        assert_eq!(second["skipped"].as_array().unwrap().len(), 0);
        assert_eq!(mode(&home), 0o700);
    }

    #[tokio::test]
    async fn home_ensure_never_follows_planted_symlinks() {
        let fx = fixture(|root| root.join("storage/homes/alice"));
        let home = fx.options.homes_dir.join("alice");
        std::fs::create_dir(&home).unwrap();
        let outside = fx.dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(".local")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(".cache")).unwrap();
        std::fs::write(home.join(".config"), b"a file").unwrap();

        let reply = home_ensure(&fx.options, op("alice")).await.unwrap();
        let skipped: Vec<&str> = reply["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(skipped, vec![".config", ".cache", ".local"]);
        // Nothing was created through the links, and their target is untouched.
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(mode(&outside), 0o755);
        // The passwd home is already right: no usermod.
        assert_eq!(reply["passwdUpdated"], false);
        assert!(logged(&fx.dir.path().join("usermod.log")).is_empty());
    }

    /// Run `swap` right after the helper creates the folder `name`.
    fn on_mkdir(name: &'static str, swap: impl FnMut() + 'static) {
        let mut swap = Box::new(swap);
        AFTER_MKDIR.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_: RawFd, created: &str| {
                if created == name {
                    swap();
                }
            }));
        });
    }

    fn clear_hook() {
        AFTER_MKDIR.with(|hook| hook.borrow_mut().take());
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_folder_swapped_in_after_mkdir_is_never_taken_over() {
        // In the HOME: the user renames one of their own folders (with
        // content) into the place of a skeleton folder the helper just made.
        let fx = fixture(|root| root.join("storage/homes/alice"));
        let home = fx.options.homes_dir.join("alice");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(home.join("stash")).unwrap();
        std::fs::write(home.join("stash/data.txt"), b"x").unwrap();
        let swap_home = home.clone();
        on_mkdir(".config", move || {
            std::fs::rename(swap_home.join(".config"), swap_home.join("made")).unwrap();
            std::fs::rename(swap_home.join("stash"), swap_home.join(".config")).unwrap();
        });
        let reply = home_ensure(&fx.options, op("alice")).await;
        clear_hook();
        let reply = reply.unwrap();
        assert!(strings(&reply["skipped"]).contains(&".config".to_string()));
        let created = strings(&reply["skeleton"]);
        assert!(!created.contains(&".config".to_string()), "{created:?}");
        assert!(created.contains(&".cache".to_string()), "{created:?}");
        assert!(home.join(".config/data.txt").exists());

        // In the homes root: a folder with content renamed into the place of
        // the HOME just created is refused, and its mode is left alone.
        let fx = fixture(|root| root.join("storage/homes/bob"));
        let homes = fx.options.homes_dir.clone();
        let decoy = homes.join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        std::fs::write(decoy.join("data.txt"), b"x").unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let swap_homes = homes.clone();
        on_mkdir("bob", move || {
            std::fs::rename(swap_homes.join("bob"), swap_homes.join("made")).unwrap();
            std::fs::rename(swap_homes.join("decoy"), swap_homes.join("bob")).unwrap();
        });
        let error = home_ensure(&fx.options, op("bob")).await;
        clear_hook();
        let error = error.unwrap_err();
        assert_eq!(error.code, "command_failed", "{error:?}");
        assert_eq!(mode(&homes.join("bob")), 0o755);
        assert!(!homes.join("bob/.config").exists());
        assert!(logged(&fx.dir.path().join("usermod.log")).is_empty());
    }

    #[test]
    fn only_an_empty_folder_of_this_process_counts_as_just_created() {
        let dir = tempfile::tempdir().unwrap();
        let base = open_dir(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("fresh")).unwrap();
        assert!(open_created_dir(base.as_raw_fd(), "fresh").is_ok());
        std::fs::create_dir_all(dir.path().join("used/sub")).unwrap();
        std::fs::create_dir(dir.path().join("files")).unwrap();
        std::fs::write(dir.path().join("files/a"), b"x").unwrap();
        for name in ["used", "files"] {
            let error = open_created_dir(base.as_raw_fd(), name).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{name}");
        }
        std::os::unix::fs::symlink(dir.path().join("fresh"), dir.path().join("link")).unwrap();
        assert!(open_created_dir(base.as_raw_fd(), "link").is_err());
    }

    #[tokio::test]
    async fn home_ensure_refuses_symlinks_legacy_and_foreign_owners() {
        // A symlink in the HOME's place.
        let fx = fixture(|root| root.join("data/linux-users/7"));
        let outside = fx.dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fx.options.homes_dir.join("alice")).unwrap();
        let error = home_ensure(&fx.options, op("alice")).await.unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        assert_eq!(mode(&outside) & 0o777, 0o755);

        // A HOME still owned by the rast service: the migration's job.
        let mut fx = fixture(|root| root.join("data/linux-users/7"));
        std::fs::create_dir(fx.options.homes_dir.join("bob")).unwrap();
        fx.options.test_identity_uid = Some(euid().wrapping_add(2));
        fx.options.rast_uid = Some(euid());
        let error = home_ensure(&fx.options, op("bob")).await.unwrap_err();
        assert_eq!(error.code, "legacy_home");
        // Someone else's folder.
        fx.options.rast_uid = Some(euid().wrapping_add(3));
        let error = home_ensure(&fx.options, op("bob")).await.unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        // A plain file.
        std::fs::write(fx.options.homes_dir.join("carol"), b"x").unwrap();
        fx.options.test_identity_uid = Some(euid());
        let error = home_ensure(&fx.options, op("carol")).await.unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        assert!(logged(&fx.dir.path().join("usermod.log")).is_empty());
    }

    #[tokio::test]
    async fn home_ensure_validates_ids_names_and_the_account() {
        let fx = fixture(|root| root.join("data/linux-users/7"));
        for name in ["Alice", "../etc", "a/b", "rast-u7", "", "@root"] {
            let error = home_ensure(&fx.options, op(name)).await.unwrap_err();
            assert_eq!(error.code, "invalid_request", "{name:?}");
        }
        for user_id in [0, -1, 1_000_001] {
            let error = home_ensure(
                &fx.options,
                HomeEnsureOp {
                    user_id,
                    username: "alice".into(),
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "invalid_request", "{user_id}");
        }
        std::fs::write(fx.dir.path().join("no-user"), b"").unwrap();
        let error = home_ensure(&fx.options, op("alice")).await.unwrap_err();
        assert_eq!(error.code, "not_found");
        assert!(!fx.options.homes_dir.join("alice").exists());
    }

    #[tokio::test]
    async fn files_agent_start_builds_a_hardened_socket_activated_unit() {
        let fx = fixture(|root| root.join("storage/homes/alice"));
        let home = fx.options.homes_dir.join("alice");
        std::fs::create_dir(&home).unwrap();
        let reply = files_agent_start(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap();
        let dir = fx.options.run_dir.join("files");
        assert_eq!(reply["unit"], "rast-files-u200007");
        assert_eq!(
            reply["socket"],
            dir.join("u200007.sock").display().to_string()
        );
        let token = reply["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        let token_file = dir.join("u200007.token");
        assert_eq!(std::fs::read_to_string(&token_file).unwrap(), token);
        assert_eq!(mode(&token_file), 0o600);
        assert_eq!(mode(&dir), 0o750);

        let runs = logged(&fx.dir.path().join("systemd-run.log"));
        assert_eq!(runs.len(), 1);
        let args = &runs[0];
        let has = |value: String| assert!(args.contains(&value), "missing {value}: {args:?}");
        has("--unit=rast-files-u200007".into());
        has("--slice=rast-linux.slice".into());
        has(format!(
            "--socket-property=ListenStream={}",
            dir.join("u200007.sock").display()
        ));
        has("--socket-property=SocketUser=root".into());
        has("--socket-property=SocketGroup=0".into());
        has("--socket-property=SocketMode=0660".into());
        has("--property=User=rast-u7".into());
        has("--property=Group=rast-u7".into());
        has(format!(
            "--property=LoadCredential=token:{}",
            token_file.display()
        ));
        for property in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "PrivateTmp=yes",
            "PrivateNetwork=yes",
            "RestrictAddressFamilies=AF_UNIX",
            "CapabilityBoundingSet=",
            "MemoryMax=512M",
            "TasksMax=64",
        ] {
            has(format!("--property={property}"));
        }
        has(format!("--property=ReadWritePaths={}", home.display()));
        has(format!(
            "--property=ReadWritePaths={}",
            fx.options.storage_dir.join("shared").display()
        ));
        // The token never appears on the command line.
        assert!(!args.iter().any(|arg| arg.contains(token)));
        let command = &args[args.iter().position(|arg| arg == "--").unwrap() + 1..];
        assert_eq!(command[1], "file-agent");
        assert_eq!(
            command[2..4],
            ["--home".to_string(), home.display().to_string()]
        );
        assert_eq!(
            command[6..8],
            ["--peer-uid".to_string(), euid().wrapping_add(1).to_string()]
        );

        // A running agent is reused with its token.
        std::fs::write(fx.dir.path().join("active"), b"").unwrap();
        let again = files_agent_start(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap();
        assert_eq!(again["token"], token);
        assert_eq!(logged(&fx.dir.path().join("systemd-run.log")).len(), 1);

        // Stop: both units, and the token is gone.
        let stopped = files_agent_stop(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap();
        assert_eq!(stopped["stopped"], true);
        let calls = logged(&fx.dir.path().join("systemctl.log"));
        assert!(calls.contains(&vec!["stop".into(), "rast-files-u200007.socket".into()]));
        assert!(calls.contains(&vec!["stop".into(), "rast-files-u200007.service".into()]));
        assert!(!token_file.exists());
    }

    #[tokio::test]
    async fn files_agent_start_needs_a_one_home() {
        // The passwd home is still the identity home: no one HOME yet.
        let fx = fixture(|root| root.join("data/linux-users/7"));
        let error = files_agent_start(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap_err();
        assert_eq!(error.code, "legacy_home");
        // A one HOME that the service still owns.
        let mut fx = fixture(|root| root.join("storage/homes/alice"));
        std::fs::create_dir(fx.options.homes_dir.join("alice")).unwrap();
        fx.options.test_identity_uid = Some(euid().wrapping_add(2));
        fx.options.rast_uid = Some(euid());
        let error = files_agent_start(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap_err();
        assert_eq!(error.code, "legacy_home");
        // A passwd home outside the homes root is never served.
        let fx = fixture(|_| PathBuf::from("/etc"));
        let error = files_agent_start(&fx.options, FilesAgentOp { user_id: 7 })
            .await
            .unwrap_err();
        assert_eq!(error.code, "legacy_home");
        assert!(logged(&fx.dir.path().join("systemd-run.log")).is_empty());
        let error = files_agent_start(&fx.options, FilesAgentOp { user_id: 0 })
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }

    fn passwd(home: PathBuf) -> PasswdEntry {
        PasswdEntry {
            username: "rast-u7".into(),
            uid: 200_007,
            gid: 200_007,
            home,
        }
    }

    #[tokio::test]
    async fn the_one_home_comes_from_passwd_and_the_owner_of_the_folder() {
        let fx = fixture(|root| root.join("storage/homes/alice"));
        let homes = fx.options.homes_dir.clone();
        let identity = Identity::derive(&fx.options, 7).unwrap();
        let legacy = identity.home.clone();
        std::fs::create_dir(homes.join("alice")).unwrap();

        let resolved = identity
            .clone()
            .resolved(&fx.options, &passwd(homes.join("alice")));
        assert!(resolved.one_home);
        assert_eq!(resolved.home, homes.join("alice"));

        // Not a one HOME: the legacy identity home, a folder below a HOME,
        // a name that is not a HOME name, a missing folder, a link.
        for home in [
            legacy.clone(),
            homes.join("alice/sub"),
            homes.join("Alice"),
            homes.join("bob"),
            PathBuf::from("/etc"),
        ] {
            let kept = identity
                .clone()
                .resolved(&fx.options, &passwd(home.clone()));
            assert!(!kept.one_home, "{}", home.display());
            assert_eq!(kept.home, legacy);
        }
        std::os::unix::fs::symlink(homes.join("alice"), homes.join("carol")).unwrap();
        assert!(
            !identity
                .clone()
                .resolved(&fx.options, &passwd(homes.join("carol")))
                .one_home
        );

        // A HOME that the service (or anyone but the identity) owns.
        let mut other = fixture(|root| root.join("storage/homes/alice"));
        std::fs::create_dir(other.options.homes_dir.join("alice")).unwrap();
        other.options.test_identity_uid = Some(euid().wrapping_add(2));
        let identity = Identity::derive(&other.options, 7).unwrap();
        assert!(
            !identity
                .resolved(
                    &other.options,
                    &passwd(other.options.homes_dir.join("alice"))
                )
                .one_home
        );
    }

    #[tokio::test]
    async fn ensure_one_home_repairs_the_mode_through_a_descriptor() {
        let fx = fixture(|root| root.join("storage/homes/alice"));
        let home = fx.options.homes_dir.join("alice");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let identity = Identity::derive(&fx.options, 7)
            .unwrap()
            .resolved(&fx.options, &passwd(home.clone()));
        assert!(identity.one_home);
        // `ensure_home` takes the one-HOME path for such an identity.
        crate::native::identity::ensure_home(&fx.options, &identity)
            .await
            .unwrap();
        assert_eq!(mode(&home), 0o700);
        assert!(
            !fx.options.linux_users_dir().join("7").exists(),
            "no legacy home is made for a one-HOME account"
        );

        // A link swapped in after the resolution is refused, not followed.
        std::fs::rename(&home, fx.options.homes_dir.join("real")).unwrap();
        std::os::unix::fs::symlink(fx.options.homes_dir.join("real"), &home).unwrap();
        std::fs::set_permissions(
            fx.options.homes_dir.join("real"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(ensure_one_home(&fx.options, &identity).is_err());
        assert_eq!(mode(&fx.options.homes_dir.join("real")), 0o755);
    }
}
