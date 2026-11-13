//! Privileged native Linux app helper (`rast native-helper`).
//!
//! RAST runs Docker-backed Linux desktop apps through `rast-worker.service`.
//! The native backend replaces the container with a real unprivileged Linux
//! identity per RAST user and an Xpra seamless session on the host:
//!
//! * the unprivileged web server (`rast.service`) never has root rights;
//! * this helper runs as root from `rast-native.service` and listens on a Unix
//!   socket ([`NativeOptions::socket_path`], default `/run/rast/native.sock`,
//!   mode 0660 root:rast, optional peer-uid allow-list);
//! * the protocol is JSON-lines ([`protocol`]) with strict per-operation
//!   validation; there is no shell anywhere and every external command is an
//!   absolute binary invoked with an argument vector;
//! * long operations are bounded by a semaphore and APT transactions are
//!   serialized so two installs never interleave.
//!
//! The helper is the *only* component allowed to:
//!
//! 1. provision per-user unprivileged Linux identities ([`identity`]),
//! 2. start/stop graphical sessions and application processes through systemd
//!    transient units in `rast-linux.slice`,
//! 3. run host APT transactions for admin-approved `.deb` packages,
//! 4. create/chown/remove managed profile paths under the RAST data directory.
//!
//! Run `rast native-helper --check` to validate the environment (root, systemd,
//! Xpra, required binaries, socket group) without binding the socket.

pub mod client;
pub mod daemon;
pub mod identity;
pub mod protocol;

pub use client::NativeClient;
pub use protocol::{
    ManagedRoots, NativeError, NativeOp, NativeRequest, NativeResponse, PathPurpose,
    ResourceLimits, ValidationContext,
};

use std::{
    ffi::{CString, OsString},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use tokio::process::Command;

use crate::config::Config;

/// Required system binaries. They are invoked by absolute path; the account
/// tools (shadow-utils) live in `/usr/sbin` on Debian/Ubuntu, so their
/// candidates are resolved at startup.
pub const SYSTEMD_RUN_BIN: &str = "/usr/bin/systemd-run";
pub const SYSTEMCTL_BIN: &str = "/usr/bin/systemctl";
pub const APT_GET_BIN: &str = "/usr/bin/apt-get";
pub const DPKG_DEB_BIN: &str = "/usr/bin/dpkg-deb";
pub const DPKG_QUERY_BIN: &str = "/usr/bin/dpkg-query";
pub const APT_CACHE_BIN: &str = "/usr/bin/apt-cache";
pub const GETENT_BIN: &str = "/usr/bin/getent";
pub const SETFACL_BIN: &str = "/usr/bin/setfacl";
pub const USERADD_CANDIDATES: &[&str] = &["/usr/sbin/useradd", "/usr/bin/useradd"];
pub const GROUPADD_CANDIDATES: &[&str] = &["/usr/sbin/groupadd", "/usr/bin/groupadd"];
pub const USERDEL_CANDIDATES: &[&str] = &["/usr/sbin/userdel", "/usr/bin/userdel"];
pub const GROUPDEL_CANDIDATES: &[&str] = &["/usr/sbin/groupdel", "/usr/bin/groupdel"];
pub const NOLOGIN_CANDIDATES: &[&str] = &["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"];

/// systemd slice that holds every transient native unit.
pub const LINUX_SLICE: &str = "rast-linux.slice";

/// Idle timeout of one helper connection (clients normally send one request).
pub const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum concurrent long-running helper operations.
pub const MAX_CONCURRENT_OPS: usize = 4;

/// Runtime options. Built from [`Config`] in production and directly in tests.
#[derive(Debug, Clone)]
pub struct NativeOptions {
    pub socket_path: PathBuf,
    /// Gid the socket is chowned to (group `rast` by default).
    pub socket_gid: Option<u32>,
    /// Uid of the `rast` service account, when it exists. Used to keep
    /// `<data_dir>/linux-apps` writable by the web server while making it
    /// traversable by the native app identities.
    pub rast_uid: Option<u32>,
    /// Optional numeric uid the connecting peer must have.
    pub allowed_peer_uid: Option<u32>,
    pub data_dir: PathBuf,
    /// Runtime root for per-user session directories (`/run/rast-linux`).
    pub run_dir: PathBuf,
    /// `<storage_dir>/homes`: only reachable through ACL/chmod path ops.
    pub homes_dir: PathBuf,
    pub xpra_bin: PathBuf,
    pub uid_base: u32,
    pub session_limits: ResourceLimits,
    pub app_limits: ResourceLimits,
    /// Free memory kept in reserve when admitting a new session or app.
    pub admission_reserve_mb: u32,
    pub idle_timeout: Duration,
    pub max_concurrent: usize,
    /// Refuse to serve when not running as root (always true in production).
    pub require_root: bool,
    pub systemd_run_bin: PathBuf,
    pub systemctl_bin: PathBuf,
    pub apt_get_bin: PathBuf,
    pub dpkg_deb_bin: PathBuf,
    pub dpkg_query_bin: PathBuf,
    pub apt_cache_bin: PathBuf,
    pub getent_bin: PathBuf,
    pub useradd_bin: PathBuf,
    pub groupadd_bin: PathBuf,
    pub userdel_bin: PathBuf,
    pub groupdel_bin: PathBuf,
    /// Present when the host has `setfacl` (package `acl`).
    pub setfacl_bin: Option<PathBuf>,
    pub nologin_shell: PathBuf,
    /// Test-only switch: permits running the daemon as a non-root user and
    /// skips `chown` calls that the test account cannot perform.
    #[cfg(test)]
    pub test_mode: bool,
}

impl NativeOptions {
    /// Resolve the helper configuration from the server configuration plus the
    /// helper's own environment (`RAST_NATIVE_SOCKET_GROUP`,
    /// `RAST_NATIVE_ALLOWED_UID`).
    pub fn from_config(cfg: &Config) -> Self {
        let rast_uid = resolve_user_uid(protocol::DEFAULT_SOCKET_GROUP);
        let socket_gid = std::env::var(protocol::SOCKET_GROUP_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| Some(protocol::DEFAULT_SOCKET_GROUP.to_string()))
            .and_then(|value| resolve_group_gid(&value));
        let allowed_peer_uid = std::env::var(protocol::ALLOWED_UID_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        let run_dir = PathBuf::from(protocol::DEFAULT_RUN_DIR);
        Self {
            socket_path: cfg.native_helper_socket.clone(),
            socket_gid,
            rast_uid,
            allowed_peer_uid,
            data_dir: cfg.data_dir.clone(),
            run_dir,
            homes_dir: cfg.homes_dir(),
            xpra_bin: cfg.linux_xpra_bin.clone(),
            uid_base: cfg.linux_uid_base,
            session_limits: ResourceLimits::from_config(
                cfg.linux_session_memory_mb,
                cfg.linux_session_cpus,
                cfg.linux_session_tasks,
            ),
            app_limits: ResourceLimits::from_config(
                cfg.linux_app_memory_mb,
                cfg.linux_app_cpus,
                cfg.linux_app_tasks,
            ),
            admission_reserve_mb: cfg.linux_admission_reserve_mb,
            idle_timeout: CONNECTION_IDLE_TIMEOUT,
            max_concurrent: MAX_CONCURRENT_OPS,
            require_root: true,
            systemd_run_bin: PathBuf::from(SYSTEMD_RUN_BIN),
            systemctl_bin: PathBuf::from(SYSTEMCTL_BIN),
            apt_get_bin: PathBuf::from(APT_GET_BIN),
            dpkg_deb_bin: PathBuf::from(DPKG_DEB_BIN),
            dpkg_query_bin: PathBuf::from(DPKG_QUERY_BIN),
            apt_cache_bin: PathBuf::from(APT_CACHE_BIN),
            getent_bin: PathBuf::from(GETENT_BIN),
            useradd_bin: resolve_binary(USERADD_CANDIDATES),
            groupadd_bin: resolve_binary(GROUPADD_CANDIDATES),
            userdel_bin: resolve_binary(USERDEL_CANDIDATES),
            groupdel_bin: resolve_binary(GROUPDEL_CANDIDATES),
            setfacl_bin: Some(PathBuf::from(SETFACL_BIN)).filter(|path| path.is_file()),
            nologin_shell: resolve_binary(NOLOGIN_CANDIDATES),
            #[cfg(test)]
            test_mode: false,
        }
    }

    /// Test constructor: a self-contained helper running as the current user.
    #[cfg(test)]
    pub fn for_test(socket_path: PathBuf, data_dir: PathBuf, run_dir: PathBuf) -> Self {
        let homes_dir = data_dir
            .parent()
            .map(|parent| parent.join("storage/homes"))
            .unwrap_or_else(|| data_dir.join("homes"));
        Self {
            socket_path,
            socket_gid: None,
            rast_uid: None,
            allowed_peer_uid: None,
            data_dir,
            run_dir,
            homes_dir,
            xpra_bin: PathBuf::from("/usr/bin/xpra"),
            uid_base: 200_000,
            session_limits: ResourceLimits::from_config(4096, 4, 2048),
            app_limits: ResourceLimits::from_config(2048, 2, 1024),
            admission_reserve_mb: 0,
            idle_timeout: Duration::from_secs(10),
            max_concurrent: MAX_CONCURRENT_OPS,
            require_root: false,
            systemd_run_bin: PathBuf::from("/usr/bin/false"),
            systemctl_bin: PathBuf::from("/usr/bin/false"),
            apt_get_bin: PathBuf::from("/usr/bin/false"),
            dpkg_deb_bin: PathBuf::from("/usr/bin/false"),
            dpkg_query_bin: PathBuf::from("/usr/bin/false"),
            apt_cache_bin: PathBuf::from("/usr/bin/false"),
            getent_bin: PathBuf::from("/usr/bin/false"),
            useradd_bin: PathBuf::from("/usr/bin/false"),
            groupadd_bin: PathBuf::from("/usr/bin/false"),
            userdel_bin: PathBuf::from("/usr/bin/false"),
            groupdel_bin: PathBuf::from("/usr/bin/false"),
            setfacl_bin: None,
            nologin_shell: PathBuf::from("/usr/sbin/nologin"),
            test_mode: true,
        }
    }

    /// Test-only switch (always false in production builds).
    pub fn is_test_mode(&self) -> bool {
        #[cfg(test)]
        {
            self.test_mode
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    pub fn linux_apps_dir(&self) -> PathBuf {
        self.data_dir.join("linux-apps")
    }

    pub fn linux_users_dir(&self) -> PathBuf {
        self.data_dir.join("linux-users")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.linux_apps_dir().join("profiles")
    }

    /// Gid of the `rast` group; an actionable error when it does not exist.
    pub fn require_rast_gid(&self) -> Result<u32, NativeError> {
        self.socket_gid.ok_or_else(|| {
            NativeError::unavailable(format!(
                "the '{}' group does not exist; the installer creates it, or set {} to an existing group name/gid",
                protocol::DEFAULT_SOCKET_GROUP,
                protocol::SOCKET_GROUP_ENV
            ))
        })
    }

    /// Gid used for managed roots and the socket. In tests the account may not
    /// have a `rast` group, so a harmless placeholder is used (ownership is
    /// skipped in test mode anyway).
    pub fn effective_rast_gid(&self) -> Result<u32, NativeError> {
        match self.socket_gid {
            Some(gid) => Ok(gid),
            None if self.is_test_mode() => Ok(0),
            None => self.require_rast_gid(),
        }
    }

    pub fn validation_context(&self) -> ValidationContext {
        let roots = ManagedRoots::new(self.data_dir.clone(), self.run_dir.clone())
            .with_homes(self.homes_dir.clone());
        ValidationContext {
            roots,
            xpra_bin: self.xpra_bin.clone(),
            uid_base: self.uid_base,
            session_limits: self.session_limits,
            app_limits: self.app_limits,
        }
    }

    /// Validate the parts of the environment the helper cannot run without.
    /// Returns `(errors, warnings)` — the caller decides how to present them.
    pub fn check_environment(&self) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        if self.require_root && !is_root() {
            errors.push(
                "must run as root: start rast-native.service (the unit runs as root) or run \
                 `sudo rast native-helper --check`"
                    .to_string(),
            );
        }
        if self.socket_gid.is_none() {
            errors.push(format!(
                "the '{}' system group does not exist; the installer creates it",
                protocol::DEFAULT_SOCKET_GROUP
            ));
        }
        if !Path::new("/run/systemd/system").is_dir() {
            errors.push("systemd is not running (no /run/systemd/system)".to_string());
        }
        for (label, path) in [
            ("systemd-run", &self.systemd_run_bin),
            ("systemctl", &self.systemctl_bin),
            ("apt-get", &self.apt_get_bin),
            ("dpkg-deb", &self.dpkg_deb_bin),
            ("dpkg-query", &self.dpkg_query_bin),
            ("apt-cache", &self.apt_cache_bin),
            ("getent", &self.getent_bin),
            ("useradd", &self.useradd_bin),
            ("groupadd", &self.groupadd_bin),
            ("userdel", &self.userdel_bin),
            ("groupdel", &self.groupdel_bin),
        ] {
            if !is_executable(path) {
                errors.push(format!(
                    "required binary {label} not found or not executable at {}",
                    path.display()
                ));
            }
        }
        if !is_executable(&self.xpra_bin) {
            errors.push(format!(
                "Xpra is not installed or not executable at {}: install it from the xpra.org \
                 apt repository — see docs/INSTALL.md, section 'Native Linux apps (Xpra)'",
                self.xpra_bin.display()
            ));
        }
        if self.setfacl_bin.is_none() {
            warnings.push(
                "setfacl is not installed; native apps cannot be granted access to RAST home \
                 folders with path_acl (install the 'acl' package)"
                    .to_string(),
            );
        }
        if !self.is_test_mode() {
            if let Ok(metadata) = std::fs::symlink_metadata(&self.data_dir) {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o001 == 0 {
                    warnings.push(format!(
                        "RAST data directory {} is not traversable by the native app identities \
                         (needs the 'others execute' bit, mode 0711); run `chmod 711 {}` or let \
                         rast.service create it with the documented mode",
                        self.data_dir.display(),
                        self.data_dir.display()
                    ));
                }
            }
        }
        if !self.run_dir.is_dir() {
            warnings.push(format!(
                "runtime root {} does not exist yet; it is created at startup (or by \
                 systemd-tmpfiles: deploy/rast-native.tmpfiles.conf)",
                self.run_dir.display()
            ));
        }
        match x11_socket_dir_status() {
            X11DirStatus::Ok => {}
            X11DirStatus::Fixable(message) => warnings.push(message),
        }
        (errors, warnings)
    }
}

/// State of `/tmp/.X11-unix`.
pub enum X11DirStatus {
    Ok,
    Fixable(String),
}

/// `/tmp/.X11-unix` must be mode 1777 so every identity can create its X
/// socket. Xpra/Xvfb fail with a confusing error otherwise.
pub fn x11_socket_dir_status() -> X11DirStatus {
    let path = Path::new("/tmp/.X11-unix");
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return X11DirStatus::Fixable(
                    "/tmp/.X11-unix exists but is not a directory; run \
                     `systemd-tmpfiles --create /etc/tmpfiles.d/rast-native.conf`"
                        .to_string(),
                );
            }
            if metadata.permissions().mode() & 0o1777 != 0o1777 {
                return X11DirStatus::Fixable(format!(
                    "/tmp/.X11-unix is mode {:04o}, expected 1777; run \
                     `systemd-tmpfiles --create /etc/tmpfiles.d/rast-native.conf`",
                    metadata.permissions().mode() & 0o7777
                ));
            }
            X11DirStatus::Ok
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => X11DirStatus::Fixable(
            "/tmp/.X11-unix is missing; run `systemd-tmpfiles --create \
             /etc/tmpfiles.d/rast-native.conf`"
                .to_string(),
        ),
        Err(error) => X11DirStatus::Fixable(format!("cannot inspect /tmp/.X11-unix: {error}")),
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn resolve_binary(candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(candidates[0]))
}

/// Resolve a group name (via `/etc/group`) or a numeric gid.
pub fn resolve_group_gid(value: &str) -> Option<u32> {
    if let Ok(gid) = value.parse::<u32>() {
        return Some(gid);
    }
    let text = std::fs::read_to_string("/etc/group").ok()?;
    for line in text.lines() {
        let mut parts = line.split(':');
        let name = parts.next()?;
        let _password = parts.next()?;
        let gid = parts.next()?;
        if name == value {
            return gid.parse::<u32>().ok();
        }
    }
    None
}

/// Resolve a user name to its uid through `/etc/passwd` (best effort; used for
/// the `rast` service account only).
pub fn resolve_user_uid(name: &str) -> Option<u32> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let mut parts = line.split(':');
        let entry_name = parts.next()?;
        let _password = parts.next()?;
        let uid = parts.next()?;
        if entry_name == name {
            return uid.parse::<u32>().ok();
        }
    }
    None
}

/// Ensure the managed directory tree exists with the documented ownership.
///
/// * `<data_dir>/linux-apps`         0750 rast:rast
/// * `<data_dir>/linux-apps/profiles` 0711 root:rast (traversable by identities)
/// * `<data_dir>/linux-users`        0711 root:rast (traversable by identities)
/// * `/run/rast-linux`               0755 root:root
///
/// Runs as root in production; in tests only the directories are created.
/// One managed directory to bootstrap: path, mode, and desired owner.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedRootPlan {
    path: PathBuf,
    mode: u32,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// Ownership/mode plan for [`ensure_managed_roots`].
///
/// Canonical layout (also installed by `rast-native.tmpfiles.conf` and the
/// installer):
///
/// * `<data_dir>/linux-apps` 0751 owner:<data-dir owner> — identities must be
///   able to traverse it; the web server owns it and keeps write access.
/// * `<data_dir>/linux-apps/apps` 0755 owner:<data-dir owner> — identities
///   read/execute managed installs.
/// * `<data_dir>/linux-apps/staging` 0750 owner:<data-dir owner> — private.
/// * `<data_dir>/linux-apps/profiles` 0711 root:<data-dir group>.
/// * `<data_dir>/linux-users` 0711 root:<data-dir group> (per-user homes are
///   0700 uid:gid and created by the identity code).
/// * `/run/rast-linux` 0755 root:root.
fn managed_root_plan(
    data_dir: &Path,
    run_dir: &Path,
    data_uid: u32,
    data_gid: u32,
) -> Vec<ManagedRootPlan> {
    let linux_apps = data_dir.join("linux-apps");
    vec![
        ManagedRootPlan {
            path: linux_apps.clone(),
            mode: 0o751,
            uid: Some(data_uid),
            gid: Some(data_gid),
        },
        ManagedRootPlan {
            path: linux_apps.join("apps"),
            mode: 0o755,
            uid: Some(data_uid),
            gid: Some(data_gid),
        },
        ManagedRootPlan {
            path: linux_apps.join("staging"),
            mode: 0o750,
            uid: Some(data_uid),
            gid: Some(data_gid),
        },
        ManagedRootPlan {
            path: linux_apps.join("profiles"),
            mode: 0o711,
            uid: Some(0),
            gid: Some(data_gid),
        },
        ManagedRootPlan {
            path: data_dir.join("linux-users"),
            mode: 0o711,
            uid: Some(0),
            gid: Some(data_gid),
        },
        ManagedRootPlan {
            path: run_dir.to_path_buf(),
            mode: 0o755,
            uid: Some(0),
            gid: Some(0),
        },
    ]
}

/// Owner of `<data_dir>` (the rast service account), or root:<socket group>
/// when it cannot be read. New `linux-apps` trees inherit this owner so the
/// unprivileged web server keeps write access.
pub(crate) fn data_dir_owner(
    options: &NativeOptions,
    rast_gid: u32,
) -> Result<(u32, u32), NativeError> {
    match std::fs::symlink_metadata(&options.data_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(NativeError::forbidden(format!(
                    "managed path {} must be a real directory",
                    options.data_dir.display()
                )));
            }
            Ok((metadata.uid(), metadata.gid()))
        }
        Err(_) => Ok((0, rast_gid)),
    }
}

/// Ensure the managed directory tree exists with the documented ownership.
///
/// Directories that already exist are only adjusted when their owner/mode
/// differ from the plan; in particular the helper never fights the installer
/// over an existing `<data_dir>/linux-apps` owner.
pub fn ensure_managed_roots(options: &NativeOptions, rast_gid: u32) -> Result<(), NativeError> {
    // The data directory is owned by the rast service account. In tests the
    // helper runs as an ordinary user, so the fixture dir is used directly.
    if !options.data_dir.is_dir() {
        if options.is_test_mode() {
            std::fs::create_dir_all(&options.data_dir).map_err(|error| {
                NativeError::command_failed(format!(
                    "cannot create data directory {}: {error}",
                    options.data_dir.display()
                ))
            })?;
        } else {
            return Err(NativeError::unavailable(format!(
                "RAST data directory {} does not exist; start rast.service first",
                options.data_dir.display()
            )));
        }
    }
    let (data_uid, data_gid) = data_dir_owner(options, rast_gid)?;
    // Native identities must traverse the data directory to reach their own
    // trees. The RAST server normally creates it with the o+x bit (and the
    // installer/tmpfiles keep it); add it here when missing so a restart of the
    // helper repairs an installation that was made before native support.
    // Ownership is never changed.
    if let Ok(current) = std::fs::metadata(&options.data_dir) {
        let mode = current.permissions().mode();
        if mode & 0o001 == 0 {
            std::fs::set_permissions(
                &options.data_dir,
                std::fs::Permissions::from_mode(mode | 0o001),
            )
            .map_err(|error| {
                NativeError::command_failed(format!(
                    "cannot make the data directory {} traversable for native app identities: \
                     {error}",
                    options.data_dir.display()
                ))
            })?;
        }
    }
    for plan in managed_root_plan(&options.data_dir, &options.run_dir, data_uid, data_gid) {
        std::fs::create_dir_all(&plan.path).map_err(|error| {
            NativeError::command_failed(format!(
                "cannot create managed directory {}: {error}",
                plan.path.display()
            ))
        })?;
        let current = std::fs::symlink_metadata(&plan.path).map_err(|error| {
            NativeError::unavailable(format!("cannot inspect {}: {error}", plan.path.display()))
        })?;
        if current.file_type().is_symlink() || !current.file_type().is_dir() {
            return Err(NativeError::forbidden(format!(
                "managed path {} must be a real directory",
                plan.path.display()
            )));
        }
        if current.permissions().mode() & 0o7777 != plan.mode {
            std::fs::set_permissions(&plan.path, std::fs::Permissions::from_mode(plan.mode))
                .map_err(|error| {
                    NativeError::command_failed(format!(
                        "cannot set permissions on {}: {error}",
                        plan.path.display()
                    ))
                })?;
        }
        if !options.is_test_mode() {
            let owner_changed = plan.uid.map(|uid| current.uid() != uid).unwrap_or(false)
                || plan.gid.map(|gid| current.gid() != gid).unwrap_or(false);
            if owner_changed {
                std::os::unix::fs::chown(&plan.path, plan.uid, plan.gid).map_err(|error| {
                    NativeError::command_failed(format!(
                        "cannot chown {}: {error}",
                        plan.path.display()
                    ))
                })?;
            }
        }
    }
    Ok(())
}

/// Remove a directory tree without ever following symbolic links (a symlink is
/// unlinked itself, including at the top level).
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return std::fs::remove_file(path);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        remove_tree(&entry.path())?;
    }
    std::fs::remove_dir(path)
}

/// `lchown`: change the owner of a symlink itself, never its target.
pub(crate) fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    let c_path = CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: c_path is a valid NUL-terminated string for the duration of the
    // call; lchown only reads it and has no other preconditions.
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Output of one bounded external command.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run one absolute binary with an argument vector, no shell, a bounded
/// deadline and capped output capture.
pub(crate) async fn run_command(
    binary: &Path,
    args: Vec<OsString>,
    env: &[(&str, &str)],
    limit: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<CommandOutput, NativeError> {
    run_command_in(binary, args, None, env, limit, max_stdout, max_stderr).await
}

/// [`run_command`] with an explicit working directory.
pub(crate) async fn run_command_in(
    binary: &Path,
    args: Vec<OsString>,
    workdir: Option<&Path>,
    env: &[(&str, &str)],
    limit: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<CommandOutput, NativeError> {
    let mut command = Command::new(binary);
    if let Some(workdir) = workdir {
        command.current_dir(workdir);
    }
    command
        .args(&args)
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        NativeError::unavailable(format!("cannot execute {}: {error}", binary.display()))
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_capped(stdout, max_stdout));
    let stderr_task = tokio::spawn(read_capped(stderr, max_stderr));
    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(NativeError::command_failed(format!(
                "cannot wait for {}: {error}",
                binary.display()
            )));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(NativeError::timeout(format!(
                "{} timed out after {limit:?}",
                binary.display()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&stdout_task.await.unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

async fn read_capped<R>(reader: Option<R>, limit: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if out.len() < limit {
                    let remaining = limit - out.len();
                    let take = read.min(remaining);
                    out.extend_from_slice(&buffer[..take]);
                }
            }
        }
    }
    out
}

/// Short, control-character-free summary of a failed command.
pub(crate) fn summarize_command(output: &CommandOutput) -> String {
    let raw = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if raw.is_empty() {
        return "command exited with a non-zero status".to_string();
    }
    protocol::truncate_message(raw, 800)
}

/// CLI entry point for `rast native-helper` (and `--check`).
pub async fn run(cfg: Config, check: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let options = NativeOptions::from_config(&cfg);
    if check {
        let (errors, warnings) = options.check_environment();
        for warning in &warnings {
            println!("WARN: {warning}");
        }
        for error in &errors {
            println!("FAIL: {error}");
        }
        if errors.is_empty() {
            println!(
                "native helper environment ok (socket {}, xpra {})",
                options.socket_path.display(),
                options.xpra_bin.display()
            );
            Ok(ExitCode::SUCCESS)
        } else {
            println!(
                "native helper environment check failed ({} problem(s))",
                errors.len()
            );
            Ok(ExitCode::FAILURE)
        }
    } else {
        if options.require_root && !is_root() {
            return Err(
                "the native helper must run as root (systemd runs rast-native.service as root)"
                    .into(),
            );
        }
        tracing::info!(
            socket = %options.socket_path.display(),
            run_dir = %options.run_dir.display(),
            xpra = %options.xpra_bin.display(),
            "starting privileged native helper"
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = rx.await;
        };
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = tx.send(());
        });
        daemon::serve(options, shutdown)
            .await
            .map_err(|error| -> Box<dyn std::error::Error> {
                format!("native helper failed: {error}").into()
            })?;
        tracing::info!("native helper stopped");
        Ok(ExitCode::SUCCESS)
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Result of the `rast doctor` native section.
#[derive(Debug, Default, Clone)]
pub struct DoctorReport {
    pub lines: Vec<String>,
    pub warnings: usize,
    pub failures: usize,
}

/// Build the `rast doctor` lines for the native helper. Never reports a
/// *failure*: the native backend is optional, and a broken helper must not make
/// the whole installation look unhealthy.
pub async fn doctor_report(cfg: &Config) -> DoctorReport {
    let mut report = DoctorReport::default();
    report.lines.push("[INFO] native Linux apps".to_string());
    if !cfg.native_enabled {
        report
            .lines
            .push("        disabled (native backend off; Docker backend still works)".to_string());
        return report;
    }
    let options = NativeOptions::from_config(cfg);
    let warn = |report: &mut DoctorReport, message: String| {
        report.warnings += 1;
        report.lines.push(format!("[WARN] {message}"));
    };

    report.lines.push(format!(
        "        xpra binary: {}",
        options.xpra_bin.display()
    ));
    if is_executable(&options.xpra_bin) {
        match xpra_version(&options.xpra_bin).await {
            Some(version) => report
                .lines
                .push(format!("        xpra version: {version}")),
            None => report
                .lines
                .push("        xpra version: unknown".to_string()),
        }
    } else {
        warn(
            &mut report,
            format!(
                "Xpra is not installed at {}; native Linux apps cannot start. Install it from the \
                 xpra.org apt repository — see docs/INSTALL.md, section 'Native Linux apps (Xpra)'",
                options.xpra_bin.display()
            ),
        );
    }

    let helper_state = std::process::Command::new("systemctl")
        .args(["is-active", "rast-native.service"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| "not installed".to_string());
    report
        .lines
        .push(format!("        helper unit: {helper_state}"));
    let client = NativeClient::new(options.socket_path.clone());
    match tokio::time::timeout(Duration::from_secs(3), client.ping()).await {
        Ok(Ok(ping)) => {
            report.lines.push(format!(
                "        helper socket: {} (version {}, xpra {})",
                options.socket_path.display(),
                ping.get("version")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                if ping
                    .get("xpra")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    "detected"
                } else {
                    "missing"
                }
            ));
            if !ping
                .get("xpra")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                warn(
                    &mut report,
                    "the helper cannot see Xpra; start rast-native.service after installing Xpra"
                        .to_string(),
                );
            }
        }
        Ok(Err(error)) => warn(
            &mut report,
            format!(
                "helper socket {} is not reachable ({error}); start rast-native.service or run \
                 `rast native-helper --check`",
                options.socket_path.display()
            ),
        ),
        Err(_) => warn(
            &mut report,
            format!(
                "helper socket {} did not answer within 3 s; check `systemctl status \
                 rast-native.service`",
                options.socket_path.display()
            ),
        ),
    }

    let slice_state = std::process::Command::new("systemctl")
        .args(["show", LINUX_SLICE, "--property=LoadState", "--no-pager"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if slice_state.contains("LoadState=loaded") {
        report
            .lines
            .push(format!("        slice: {LINUX_SLICE} loaded"));
    } else {
        warn(
            &mut report,
            format!(
                "slice {LINUX_SLICE} is not loaded; install deploy/rast-linux.slice and run \
                 `systemctl daemon-reload`"
            ),
        );
    }

    match x11_socket_dir_status() {
        X11DirStatus::Ok => report
            .lines
            .push("        /tmp/.X11-unix: 1777 ok".to_string()),
        X11DirStatus::Fixable(message) => warn(&mut report, message),
    }
    let (errors, warnings) = options.check_environment();
    for warning in warnings {
        warn(&mut report, warning);
    }
    for error in errors {
        // `rast doctor` is normally run as the unprivileged service account;
        // the root requirement belongs to the helper unit (checked above),
        // not to the diagnostic itself.
        if error.starts_with("must run as root") {
            continue;
        }
        // Translated to WARN: the native backend being down is not fatal for
        // the rest of RAST.
        warn(&mut report, error);
    }
    report
}

async fn xpra_version(xpra_bin: &Path) -> Option<String> {
    let output = run_command(
        xpra_bin,
        vec![OsString::from("--version")],
        &[],
        Duration::from_secs(5),
        32 * 1024,
        32 * 1024,
    )
    .await
    .ok()?;
    let text = if output.stdout.trim().is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| protocol::truncate_message(line, 120))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(data_dir: &Path, storage_dir: &Path) -> Config {
        use crate::config::{Config, FileConfig};
        Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(data_dir.to_path_buf()),
                storage_dir: Some(storage_dir.to_path_buf()),
                native_enabled: Some(true),
                native_helper_socket: Some(data_dir.join("native.sock")),
                linux_uid_base: Some(300_000),
                linux_session_memory_mb: Some(8192),
                linux_session_cpus: Some(6),
                linux_session_tasks: Some(4096),
                linux_app_memory_mb: Some(1024),
                linux_app_cpus: Some(3),
                linux_app_tasks: Some(512),
                linux_admission_reserve_mb: Some(2048),
                linux_xpra_bin: Some(data_dir.join("xpra")),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap()
    }

    #[test]
    fn options_are_built_from_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = config(&temp.path().join("data"), &temp.path().join("storage"));
        let options = NativeOptions::from_config(&cfg);
        assert_eq!(options.socket_path, cfg.native_helper_socket);
        assert_eq!(options.run_dir, PathBuf::from(protocol::DEFAULT_RUN_DIR));
        assert_eq!(options.uid_base, 300_000);
        assert_eq!(options.xpra_bin, cfg.linux_xpra_bin);
        assert_eq!(options.homes_dir, cfg.homes_dir());
        assert_eq!(options.session_limits.memory_max_mb, 8192);
        assert_eq!(options.session_limits.cpu_quota_percent, 600);
        assert_eq!(options.session_limits.tasks_max, 4096);
        assert_eq!(options.app_limits.memory_max_mb, 1024);
        assert_eq!(options.app_limits.cpu_quota_percent, 300);
        assert_eq!(options.app_limits.tasks_max, 512);
        assert_eq!(options.admission_reserve_mb, 2048);
        assert_eq!(options.linux_users_dir(), cfg.data_dir.join("linux-users"));
        assert_eq!(
            options.profiles_dir(),
            cfg.linux_apps_dir().join("profiles")
        );
        assert!(options.require_root);
    }

    #[test]
    fn group_resolution_accepts_names_and_numeric_gids() {
        assert_eq!(resolve_group_gid("0"), Some(0));
        assert_eq!(resolve_group_gid("definitely-not-a-group-xyz"), None);
        let _ = resolve_group_gid("root");
    }

    #[test]
    fn managed_roots_are_created_with_traverse_bits() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let run = temp.path().join("run");
        std::fs::create_dir_all(&data).unwrap();
        // Deterministic: the test data directory is 0750 (no others execute),
        // so the app tree is created 0750 as well.
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o750)).unwrap();
        let options = NativeOptions::for_test(data.join("native.sock"), data.clone(), run.clone());
        ensure_managed_roots(&options, 4242).unwrap();
        let users = options.linux_users_dir();
        let profiles = options.profiles_dir();
        assert!(options.linux_apps_dir().is_dir());
        assert!(users.is_dir());
        assert!(profiles.is_dir());
        assert!(run.is_dir());
        let mode = |dir: &Path| std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        // data_dir gained the o+x bit (native identities must traverse it).
        assert_eq!(mode(&data), 0o751);
        assert_eq!(mode(&options.linux_apps_dir()), 0o751);
        assert_eq!(mode(&options.linux_apps_dir().join("apps")), 0o755);
        assert_eq!(mode(&options.linux_apps_dir().join("staging")), 0o750);
        for dir in [&users, &profiles] {
            assert_eq!(
                mode(dir),
                0o711,
                "{} must stay traversable, not listable",
                dir.display()
            );
        }
        assert_eq!(mode(&run), 0o755);

        // Ownership plan: linux-apps tree inherits the data-dir owner;
        // linux-users/profiles are root:<data-dir group>; runtime root is root.
        let plan = managed_root_plan(&options.data_dir, &options.run_dir, 993, 980);
        let expected: Vec<(PathBuf, u32, Option<u32>, Option<u32>)> = vec![
            (options.linux_apps_dir(), 0o751, Some(993), Some(980)),
            (
                options.linux_apps_dir().join("apps"),
                0o755,
                Some(993),
                Some(980),
            ),
            (
                options.linux_apps_dir().join("staging"),
                0o750,
                Some(993),
                Some(980),
            ),
            (options.profiles_dir(), 0o711, Some(0), Some(980)),
            (options.linux_users_dir(), 0o711, Some(0), Some(980)),
            (options.run_dir.clone(), 0o755, Some(0), Some(0)),
        ];
        let got: Vec<(PathBuf, u32, Option<u32>, Option<u32>)> = plan
            .into_iter()
            .map(|plan| (plan.path, plan.mode, plan.uid, plan.gid))
            .collect();
        assert_eq!(got, expected);
        // Idempotent (a second bootstrap changes nothing).
        ensure_managed_roots(&options, 4242).unwrap();
        assert_eq!(mode(&options.linux_apps_dir()), 0o751);
        assert_eq!(mode(&options.linux_apps_dir().join("apps")), 0o755);
    }

    #[test]
    fn remove_tree_never_follows_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::fs::write(root.join("nested/file"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        remove_tree(&root).unwrap();
        assert!(!root.exists());
        assert!(outside.join("keep.txt").exists(), "symlink target survives");
    }

    #[test]
    fn x11_socket_dir_status_probes_the_mode() {
        // The host may have any state; only check that the function does not
        // panic and returns one of the two variants.
        match x11_socket_dir_status() {
            X11DirStatus::Ok | X11DirStatus::Fixable(_) => {}
        }
    }

    #[tokio::test]
    async fn command_runner_captures_output_and_failures() {
        let output = run_command(
            Path::new("/bin/sh"),
            vec![
                OsString::from("-c"),
                OsString::from("echo out; echo err >&2"),
            ],
            &[],
            Duration::from_secs(5),
            1024,
            1024,
        )
        .await
        .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");

        let output = run_command(
            Path::new("/bin/sh"),
            vec![
                OsString::from("-c"),
                OsString::from("echo boom >&2; exit 3"),
            ],
            &[],
            Duration::from_secs(5),
            1024,
            1024,
        )
        .await
        .unwrap();
        assert!(!output.success);
        assert_eq!(summarize_command(&output), "boom");

        let missing = run_command(
            Path::new("/definitely/not/a/binary"),
            Vec::new(),
            &[],
            Duration::from_secs(1),
            16,
            16,
        )
        .await
        .unwrap_err();
        assert_eq!(missing.code, "unavailable");
    }

    #[tokio::test]
    async fn command_runner_times_out() {
        let error = run_command(
            Path::new("/bin/sh"),
            vec![OsString::from("-c"), OsString::from("sleep 5")],
            &[],
            Duration::from_millis(100),
            16,
            16,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "timeout");
    }

    #[tokio::test]
    async fn doctor_reports_disabled_without_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = config(&temp.path().join("data"), &temp.path().join("storage"));
        cfg.native_enabled = false;
        let report = doctor_report(&cfg).await;
        assert_eq!(report.warnings, 0);
        assert_eq!(report.failures, 0);
        assert_eq!(report.lines[0], "[INFO] native Linux apps");
        assert!(report.lines[1].contains("disabled"));
    }

    #[tokio::test]
    async fn doctor_reports_warnings_when_enabled_but_broken() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = config(&temp.path().join("data"), &temp.path().join("storage"));
        // No socket, no xpra: doctor must warn, never fail.
        let report = doctor_report(&cfg).await;
        assert_eq!(report.failures, 0);
        assert!(report.warnings > 0, "{report:?}");
        assert!(report
            .lines
            .iter()
            .any(|line| line.contains("Xpra is not installed") || line.contains("not reachable")));
    }

    #[test]
    fn check_environment_reports_actionable_errors() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mut options = NativeOptions::for_test(
            data.join("native.sock"),
            data.clone(),
            temp.path().join("run"),
        );
        options.require_root = false;
        options.socket_gid = None;
        options.systemd_run_bin = PathBuf::from("/definitely/missing");
        options.xpra_bin = PathBuf::from("/definitely/missing-xpra");
        let (errors, _warnings) = options.check_environment();
        let joined = errors.join("\n");
        assert!(joined.contains("group"), "{joined}");
        assert!(joined.contains("systemd-run"), "{joined}");
        assert!(joined.contains("Xpra"), "{joined}");
    }
}
