//! Keep cron and `at` away from identities in one-HOME mode (review F3,
//! decision A7).
//!
//! cron and `at` start jobs outside RAST units, with the supplementary
//! groups of the moment: such a job keeps the groups of shared folders its
//! user loses, and can start while `rast migrate home` walks the HOME. So
//! `home_migrate_apply` lists the identity (`rast-u<id>`) in
//! `/etc/cron.deny` and `/etc/at.deny` when it finishes, and
//! `home_migrate_rollback` takes it out again. Legacy identities and
//! identities outside the native pilot are never listed (the server only
//! migrates accounts in the pilot).
//!
//! Each edit adds or removes only the exact line `rast-u<id>`. It is
//! atomic: the new file is written next to the old one with the old
//! owner, group and mode, synced, and renamed over it. A missing
//! `cron.deny` is created as `root:root 0644` (the mode distributions ship
//! it with). A missing `at.deny` is never created: without `at.allow` and
//! `at.deny` only root may use `at`, and an `at.deny` would let every
//! other account use it.
//!
//! A crontab the identity installed before stays in the cron spool, and
//! cron may still run it: the reply says whether one exists, so the
//! administrator can remove it (`crontab -r -u rast-u<id>`).

use std::ffi::CStr;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::native::homes::{fchmod, fchown, fstat, open_dir};
use crate::native::identity::Identity;
use crate::native::protocol::NativeError;
use crate::native::shares::{is_regular, lstat_at, open_at};
use crate::native::NativeOptions;

/// The deny file of cron, below the etc folder.
pub const CRON_DENY: &str = "cron.deny";
/// The deny file of `at`.
pub const AT_DENY: &str = "at.deny";
/// A deny file larger than this is refused (it is a list of names).
const MAX_DENY_BYTES: u64 = 1 << 20;

/// Edits of the deny files never overlap within the helper.
static EDITS: Mutex<()> = Mutex::new(());

/// What changed in one deny file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Added,
    Removed,
    /// The line was already as wanted.
    Unchanged,
    /// The file does not exist and is not created (`at.deny`).
    Absent,
}

impl Edit {
    fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
            Self::Absent => "absent",
        }
    }
}

/// List (`deny`) or unlist the identity in `cron.deny` and `at.deny`.
pub fn set(options: &NativeOptions, identity: &Identity, deny: bool) -> Result<Value, NativeError> {
    let name = identity.username.as_str();
    if !valid_identity_name(name) {
        return Err(NativeError::invalid("not the name of a RAST identity"));
    }
    let _guard = EDITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let etc = open_dir(&options.etc_dir).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot open {}: {error}",
            options.etc_dir.display()
        ))
    })?;
    let cron = edit(options, &etc, CRON_DENY, name, deny, true)?;
    let at = edit(options, &etc, AT_DENY, name, deny, false)?;
    let crontab = lstat_at_path(&options.cron_spool_dir, name);
    Ok(json!({
        "cronDeny": cron.as_str(),
        "atDeny": at.as_str(),
        "crontab": crontab,
    }))
}

/// Whether `name` is in the deny file `text` (exact lines, surrounding
/// blanks ignored as cron and `at` do).
pub fn lists(text: &str, name: &str) -> bool {
    text.lines().any(|line| line.trim() == name)
}

/// `rast-u<digits>`.
fn valid_identity_name(name: &str) -> bool {
    name.strip_prefix("rast-u")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the file `name` exists in the folder `dir` (never followed).
fn lstat_at_path(dir: &std::path::Path, name: &str) -> bool {
    let Ok(dir) = open_dir(dir) else {
        return false;
    };
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    lstat_at(dir.as_raw_fd(), &name).is_ok()
}

fn cname(name: &str) -> Result<std::ffi::CString, NativeError> {
    std::ffi::CString::new(name).map_err(|_| NativeError::invalid("name"))
}

fn edit(
    options: &NativeOptions,
    etc: &OwnedFd,
    file: &str,
    name: &str,
    deny: bool,
    create: bool,
) -> Result<Edit, NativeError> {
    let path = options.etc_dir.join(file);
    let failed = |what: &str, error: io::Error| {
        NativeError::command_failed(format!("cannot {what} {}: {error}", path.display()))
    };
    let c_file = cname(file)?;
    let (text, owner) = match open_at(etc.as_raw_fd(), &c_file, libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(fd) => {
            let stat = fstat(fd.as_raw_fd()).map_err(|error| failed("inspect", error))?;
            if !is_regular(&stat) || stat.st_nlink != 1 {
                return Err(NativeError::forbidden(format!(
                    "{} is not a regular file with one link; it is left as it is",
                    path.display()
                )));
            }
            let mut text = String::new();
            std::fs::File::from(fd)
                .take(MAX_DENY_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|error| failed("read", error))?;
            if text.len() as u64 > MAX_DENY_BYTES {
                return Err(NativeError::forbidden(format!(
                    "{} is too large",
                    path.display()
                )));
            }
            (
                Some(text),
                (stat.st_uid, stat.st_gid, stat.st_mode & 0o7777),
            )
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => (None, (0, 0, 0o644)),
        Err(error) => return Err(failed("open", error)),
    };
    let listed = text.as_deref().is_some_and(|text| lists(text, name));
    let new_text = match (&text, deny, listed) {
        (_, true, true) | (None, false, _) | (Some(_), false, false) => return Ok(Edit::Unchanged),
        (None, true, _) if !create => return Ok(Edit::Absent),
        (None, true, _) => format!("{name}\n"),
        (Some(text), true, false) => {
            let mut text = text.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(name);
            text.push('\n');
            text
        }
        (Some(text), false, true) => text
            .split_inclusive('\n')
            .filter(|line| line.trim() != name)
            .collect(),
    };
    replace(options, etc, &c_file, &new_text, owner).map_err(|error| failed("write", error))?;
    Ok(if deny { Edit::Added } else { Edit::Removed })
}

/// Write `text` to a new file next to `file` with `owner` (uid, gid,
/// mode), sync it, and rename it over `file`.
fn replace(
    options: &NativeOptions,
    etc: &OwnedFd,
    file: &CStr,
    text: &str,
    (uid, gid, mode): (u32, u32, u32),
) -> io::Result<()> {
    let part = std::ffi::CString::new(format!(
        ".{}.rast-part",
        file.to_str()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?
    ))?;
    // A part file left by a run that stopped half way is replaced.
    // SAFETY: valid fd and NUL-terminated name.
    unsafe { libc::unlinkat(etc.as_raw_fd(), part.as_ptr(), 0) };
    // SAFETY: valid fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            etc.as_raw_fd(),
            part.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let written = (|| {
        if !options.is_test_mode() {
            fchown(fd.as_raw_fd(), uid, gid)?;
        }
        fchmod(fd.as_raw_fd(), mode as libc::mode_t)?;
        let mut out = std::fs::File::from(fd.try_clone()?);
        out.write_all(text.as_bytes())?;
        out.sync_all()?;
        // SAFETY: valid fds and NUL-terminated names.
        if unsafe {
            libc::renameat(
                etc.as_raw_fd(),
                part.as_ptr(),
                etc.as_raw_fd(),
                file.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if written.is_err() {
        // SAFETY: valid fd and NUL-terminated name.
        unsafe { libc::unlinkat(etc.as_raw_fd(), part.as_ptr(), 0) };
        return written;
    }
    // The rename is durable once the folder is synced.
    std::fs::File::from(etc.try_clone()?).sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn options(dir: &std::path::Path) -> NativeOptions {
        let mut options =
            NativeOptions::for_test(dir.join("native.sock"), dir.join("data"), dir.join("run"));
        options.etc_dir = dir.join("etc");
        options.cron_spool_dir = dir.join("crontabs");
        std::fs::create_dir_all(&options.etc_dir).unwrap();
        options
    }

    #[test]
    fn identities_are_listed_and_unlisted_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let identity = Identity::derive(&options, 7).unwrap();
        let cron = options.etc_dir.join(CRON_DENY);
        let at = options.etc_dir.join(AT_DENY);
        // An at.deny as the distribution ships it, without a final newline,
        // with a name that only starts like the identity's.
        std::fs::write(&at, "daemon\nrast-u70\n  nobody").unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o640)).unwrap();

        let reply = set(&options, &identity, true).unwrap();
        assert_eq!(
            reply,
            json!({"cronDeny": "added", "atDeny": "added", "crontab": false})
        );
        // cron.deny did not exist: created with the distribution's mode.
        assert_eq!(std::fs::read_to_string(&cron).unwrap(), "rast-u7\n");
        assert_eq!(
            std::fs::metadata(&cron).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            std::fs::read_to_string(&at).unwrap(),
            "daemon\nrast-u70\n  nobody\nrast-u7\n"
        );
        assert_eq!(
            std::fs::metadata(&at).unwrap().permissions().mode() & 0o7777,
            0o640,
            "the mode stays"
        );
        // Idempotent.
        let again = set(&options, &identity, true).unwrap();
        assert_eq!(again["cronDeny"], "unchanged");
        assert_eq!(again["atDeny"], "unchanged");

        // A crontab installed before is reported.
        std::fs::create_dir_all(&options.cron_spool_dir).unwrap();
        std::fs::write(options.cron_spool_dir.join("rast-u7"), "* * * * * x\n").unwrap();
        let removed = set(&options, &identity, false).unwrap();
        assert_eq!(
            removed,
            json!({"cronDeny": "removed", "atDeny": "removed", "crontab": true})
        );
        assert_eq!(std::fs::read_to_string(&cron).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(&at).unwrap(),
            "daemon\nrast-u70\n  nobody\n"
        );
        assert_eq!(
            set(&options, &identity, false).unwrap()["cronDeny"],
            "unchanged"
        );
        assert!(std::fs::read_dir(&options.etc_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with("rast-part")));
    }

    #[test]
    fn a_missing_at_deny_is_never_created_and_links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let identity = Identity::derive(&options, 7).unwrap();
        let reply = set(&options, &identity, true).unwrap();
        assert_eq!(reply["atDeny"], "absent");
        assert!(!options.etc_dir.join(AT_DENY).exists());

        // A link in place of a deny file is never followed or replaced.
        std::fs::remove_file(options.etc_dir.join(CRON_DENY)).unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "x\n").unwrap();
        std::os::unix::fs::symlink(&target, options.etc_dir.join(CRON_DENY)).unwrap();
        let error = set(&options, &identity, true).unwrap_err();
        assert_eq!(error.code, "command_failed", "{error:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x\n");
        assert!(std::fs::symlink_metadata(options.etc_dir.join(CRON_DENY))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn deny_lists_match_whole_lines() {
        assert!(lists("a\n rast-u7 \n", "rast-u7"));
        assert!(!lists("rast-u70\n#rast-u7\n", "rast-u7"));
        assert!(valid_identity_name("rast-u12"));
        for bad in ["rast-u", "rast-u1x", "root", "rast-u1\n"] {
            assert!(!valid_identity_name(bad), "{bad:?}");
        }
    }
}
