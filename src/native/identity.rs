//! Provisioning of unprivileged Linux identities for native RAST users.
//!
//! Every RAST user that runs native Linux desktop applications gets a dedicated
//! unprivileged host account:
//!
//! * username `rast-u<userId>`;
//! * uid = gid = `linux_uid_base + userId` (never in the system account range);
//! * home `<data_dir>/linux-users/<userId>` (0700);
//! * shell `/usr/sbin/nologin`, no password, no supplementary groups.
//!
//! The helper is the only component allowed to create or remove these
//! identities. Every command is executed with an argument vector (`useradd`,
//! `groupadd`, `userdel`, `groupdel`, `getent`) — no shell is ever involved —
//! with a bounded timeout and capped output.
//!
//! Idempotency: `ensure` re-checks the host account database through `getent`
//! and only creates what is missing. An existing account whose uid/gid do not
//! match the expected derivation is reported as `identity_conflict` instead of
//! being changed, so a compromised mapping cannot silently take over another
//! account.

use std::{
    ffi::OsString,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::native::{
    protocol::{
        identity_ids, username_for, validate_username, IdentityIds, NativeError, MAX_USER_ID,
    },
    run_command, NativeOptions,
};

/// Timeout for account database commands. `groupadd`/`useradd`/`userdel` can
/// take ~8 s each on hosts with slow NSS/audit setups (measured on the RAST
/// development host), and longer under load; the client deadline for identity
/// operations is 120 s, so 45 s per command is a safe bound.
const ACCOUNT_CMD_TIMEOUT: Duration = Duration::from_secs(45);
/// Timeout for `systemctl` queries while removing an identity.
const SYSTEMCTL_CMD_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum stdout kept per account command.
const MAX_ACCOUNT_OUTPUT: usize = 64 * 1024;

/// Comment stored in the GECOS field of a RAST app identity.
pub const IDENTITY_COMMENT: &str = "RAST Linux app";

/// Candidate nologin shells, in order of preference.
pub const NOLOGIN_CANDIDATES: &[&str] = &["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"];

/// One provisioned identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub user_id: i64,
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl Identity {
    /// Derive the identity of a RAST user id from configuration.
    pub fn derive(options: &NativeOptions, user_id: i64) -> Result<Self, NativeError> {
        let ids = identity_ids(&options.validation_context(), user_id)?;
        Self::from_ids(options, &ids)
    }

    fn from_ids(options: &NativeOptions, ids: &IdentityIds) -> Result<Self, NativeError> {
        let username = username_for(ids.user_id);
        validate_username(&username)?;
        Ok(Self {
            user_id: ids.user_id,
            username,
            uid: ids.uid,
            gid: ids.gid,
            home: options.linux_users_dir().join(ids.user_id.to_string()),
        })
    }

    /// Home directory path relative to `<data_dir>/linux-users`.
    pub fn home_relative(&self) -> String {
        self.user_id.to_string()
    }
}

/// Account database entry parsed from `getent passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// Parse one `getent passwd <name>` line (`name:x:uid:gid:gecos:home:shell`).
pub fn parse_getent_passwd(text: &str) -> Result<Option<PasswdEntry>, NativeError> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = line else {
        return Ok(None);
    };
    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() < 7 {
        return Err(NativeError::command_failed(
            "unexpected output from getent passwd",
        ));
    }
    let uid = fields[2]
        .parse::<u32>()
        .map_err(|_| NativeError::command_failed("unexpected uid from getent passwd"))?;
    let gid = fields[3]
        .parse::<u32>()
        .map_err(|_| NativeError::command_failed("unexpected gid from getent passwd"))?;
    Ok(Some(PasswdEntry {
        username: fields[0].to_string(),
        uid,
        gid,
        home: PathBuf::from(fields[5]),
    }))
}

/// Parse one `getent group <name>` line (`name:x:gid:members`).
pub fn parse_getent_group(text: &str) -> Result<Option<u32>, NativeError> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = line else {
        return Ok(None);
    };
    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() < 4 {
        return Err(NativeError::command_failed(
            "unexpected output from getent group",
        ));
    }
    fields[2]
        .parse::<u32>()
        .map(Some)
        .map_err(|_| NativeError::command_failed("unexpected gid from getent group"))
}

/// Ensure `groupadd` and `useradd` are only asked for a consistent identity.
pub fn check_existing_identity(
    identity: &Identity,
    passwd: Option<&PasswdEntry>,
    group_gid: Option<u32>,
) -> Result<(), NativeError> {
    if let Some(gid) = group_gid {
        if gid != identity.gid {
            return Err(NativeError::identity_conflict(format!(
                "group {} already exists with gid {gid}, expected {}; \
                 refusing to reuse it (remove it manually if it really belongs to RAST)",
                identity.username, identity.gid
            )));
        }
    }
    if let Some(entry) = passwd {
        if entry.uid != identity.uid || entry.gid != identity.gid {
            return Err(NativeError::identity_conflict(format!(
                "user {} already exists with uid/gid {}/{} but RAST expects {}/{}; \
                 refusing to reuse it",
                identity.username, entry.uid, entry.gid, identity.uid, identity.gid
            )));
        }
    }
    Ok(())
}

/// Arguments of `groupadd` for one identity.
pub fn groupadd_args(identity: &Identity) -> Vec<OsString> {
    vec![
        OsString::from("--gid"),
        OsString::from(identity.gid.to_string()),
        OsString::from(&identity.username),
    ]
}

/// Arguments of `useradd` for one identity.
pub fn useradd_args(identity: &Identity, nologin: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--uid"),
        OsString::from(identity.uid.to_string()),
        OsString::from("--gid"),
        OsString::from(identity.gid.to_string()),
        OsString::from("--home-dir"),
        identity.home.clone().into_os_string(),
        OsString::from("--shell"),
        nologin.as_os_str().to_os_string(),
        OsString::from("--no-create-home"),
        OsString::from("--comment"),
        OsString::from(IDENTITY_COMMENT),
        OsString::from(&identity.username),
    ]
}

/// Arguments of `userdel` for one identity (never `--force`).
pub fn userdel_args(identity: &Identity) -> Vec<OsString> {
    vec![OsString::from(&identity.username)]
}

/// Arguments of `groupdel` for one identity.
pub fn groupdel_args(identity: &Identity) -> Vec<OsString> {
    vec![OsString::from(&identity.username)]
}

pub(crate) async fn getent_passwd(
    options: &NativeOptions,
    username: &str,
) -> Result<Option<PasswdEntry>, NativeError> {
    let output = run_command(
        &options.getent_bin,
        vec![OsString::from("passwd"), OsString::from(username)],
        &[],
        ACCOUNT_CMD_TIMEOUT,
        MAX_ACCOUNT_OUTPUT,
        MAX_ACCOUNT_OUTPUT,
    )
    .await?;
    if !output.success {
        // `getent` exits 2 when the entry does not exist.
        if output.stdout.trim().is_empty() {
            return Ok(None);
        }
        return Err(NativeError::command_failed(format!(
            "cannot look up user {username}: {}",
            crate::native::summarize_command(&output)
        )));
    }
    parse_getent_passwd(&output.stdout)
}

async fn getent_group(options: &NativeOptions, name: &str) -> Result<Option<u32>, NativeError> {
    let output = run_command(
        &options.getent_bin,
        vec![OsString::from("group"), OsString::from(name)],
        &[],
        ACCOUNT_CMD_TIMEOUT,
        MAX_ACCOUNT_OUTPUT,
        MAX_ACCOUNT_OUTPUT,
    )
    .await?;
    if !output.success {
        if output.stdout.trim().is_empty() {
            return Ok(None);
        }
        return Err(NativeError::command_failed(format!(
            "cannot look up group {name}: {}",
            crate::native::summarize_command(&output)
        )));
    }
    parse_getent_group(&output.stdout)
}

/// Ensure a RAST user's host identity exists. Returns the identity and whether
/// something was created (`created == false` when the identity already existed
/// and matched).
pub async fn ensure_identity(
    options: &NativeOptions,
    user_id: i64,
) -> Result<(Identity, bool), NativeError> {
    if !(1..=MAX_USER_ID).contains(&user_id) {
        return Err(NativeError::invalid(format!(
            "userId must be between 1 and {MAX_USER_ID}"
        )));
    }
    let identity = Identity::derive(options, user_id)?;
    let rast_gid = options.effective_rast_gid()?;
    crate::native::ensure_managed_roots(options, rast_gid)?;

    let group_gid = getent_group(options, &identity.username).await?;
    let passwd = getent_passwd(options, &identity.username).await?;
    check_existing_identity(&identity, passwd.as_ref(), group_gid)?;

    let mut created = false;
    if group_gid.is_none() {
        let output = run_command(
            &options.groupadd_bin,
            groupadd_args(&identity),
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            MAX_ACCOUNT_OUTPUT,
        )
        .await?;
        if !output.success {
            // A concurrent ensure may have created it; re-check before failing.
            match getent_group(options, &identity.username).await? {
                Some(gid) if gid == identity.gid => {}
                Some(gid) => {
                    return Err(NativeError::identity_conflict(format!(
                        "group {} exists with gid {gid}, expected {}",
                        identity.username, identity.gid
                    )))
                }
                None => {
                    return Err(NativeError::command_failed(format!(
                        "cannot create group {}: {}",
                        identity.username,
                        crate::native::summarize_command(&output)
                    )))
                }
            }
        }
        created = true;
    }

    if passwd.is_none() {
        let output = run_command(
            &options.useradd_bin,
            useradd_args(&identity, &options.nologin_shell),
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            MAX_ACCOUNT_OUTPUT,
        )
        .await?;
        if !output.success {
            match getent_passwd(options, &identity.username).await? {
                Some(entry) if entry.uid == identity.uid && entry.gid == identity.gid => {}
                Some(_) => {
                    return Err(NativeError::identity_conflict(format!(
                        "user {} already exists with a different uid/gid",
                        identity.username
                    )))
                }
                None => {
                    return Err(NativeError::command_failed(format!(
                        "cannot create user {}: {}",
                        identity.username,
                        crate::native::summarize_command(&output)
                    )))
                }
            }
        }
        created = true;
    }

    ensure_home(options, &identity).await?;
    Ok((identity, created))
}

/// Whether a home/runtime directory needs repair (mode 0700 + expected owner).
fn needs_repair(metadata: &std::fs::Metadata, uid: u32, gid: u32) -> bool {
    metadata.permissions().mode() & 0o7777 != 0o700
        || metadata.uid() != uid
        || metadata.gid() != gid
}

/// Create/repair the identity's home directory (0700 identity:identity).
///
/// Also self-heals a home left with the wrong owner/mode by an older helper or
/// an interrupted migration (e.g. `bin:bin` instead of `uid:gid`), which would
/// otherwise make Xpra unable to (re)write `$HOME/.Xauthority`. Repairs are
/// logged at INFO so verification/ops logs show when a stale state was healed.
pub(crate) async fn ensure_home(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<(), NativeError> {
    std::fs::create_dir_all(&identity.home).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot create home {}: {error}",
            identity.home.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&identity.home).map_err(|error| {
        NativeError::unavailable(format!(
            "cannot inspect home {}: {error}",
            identity.home.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(format!(
            "home {} must be a real directory",
            identity.home.display()
        )));
    }
    let repaired = needs_repair(&metadata, identity.uid, identity.gid);
    std::fs::set_permissions(&identity.home, std::fs::Permissions::from_mode(0o700)).map_err(
        |error| {
            NativeError::command_failed(format!(
                "cannot set permissions on {}: {error}",
                identity.home.display()
            ))
        },
    )?;
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&identity.home, Some(identity.uid), Some(identity.gid)).map_err(
            |error| {
                NativeError::command_failed(format!(
                    "cannot chown {}: {error}",
                    identity.home.display()
                ))
            },
        )?;
    }
    if repaired {
        tracing::info!(
            user_id = identity.user_id,
            uid = identity.uid,
            gid = identity.gid,
            home = %identity.home.display(),
            previous_uid = metadata.uid(),
            previous_gid = metadata.gid(),
            previous_mode = format!("{:04o}", metadata.permissions().mode() & 0o7777),
            "repaired native identity home ownership/permissions"
        );
    }
    Ok(())
}

/// Ensure the runtime directory of one identity exists (`/run/rast-linux/<uid>`,
/// 0700 identity:identity) and return it.
pub(crate) fn ensure_runtime_dir(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<PathBuf, NativeError> {
    let dir = options.run_dir.join(identity.uid.to_string());
    if !dir.starts_with(&options.run_dir) {
        return Err(NativeError::forbidden(
            "runtime directory escapes the run root",
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot create runtime directory {}: {error}",
            dir.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&dir).map_err(|error| {
        NativeError::unavailable(format!(
            "cannot inspect runtime directory {}: {error}",
            dir.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(format!(
            "runtime path {} must be a real directory",
            dir.display()
        )));
    }
    let repaired = needs_repair(&metadata, identity.uid, identity.gid);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot set permissions on {}: {error}",
            dir.display()
        ))
    })?;
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&dir, Some(identity.uid), Some(identity.gid)).map_err(
            |error| NativeError::command_failed(format!("cannot chown {}: {error}", dir.display())),
        )?;
    }
    if repaired {
        tracing::info!(
            user_id = identity.user_id,
            uid = identity.uid,
            runtime_dir = %dir.display(),
            "repaired native identity runtime directory ownership/permissions"
        );
    }
    Ok(dir)
}

/// Stop the session and application units whose main process runs as `uid`.
async fn stop_units_for_uid(options: &NativeOptions, uid: u32) -> Result<(), NativeError> {
    let output = run_command(
        &options.systemctl_bin,
        vec![
            OsString::from("show"),
            OsString::from("rast-xpra-*.service"),
            OsString::from("rast-app-*.service"),
            OsString::from("--property=Id"),
            OsString::from("--property=MainPID"),
            OsString::from("--property=ActiveState"),
            OsString::from("--no-pager"),
        ],
        &[],
        SYSTEMCTL_CMD_TIMEOUT,
        MAX_ACCOUNT_OUTPUT,
        MAX_ACCOUNT_OUTPUT,
    )
    .await?;
    if !output.success {
        // No managed units at all is not an error for identity removal.
        return Ok(());
    }
    for unit in owned_units(&output.stdout, uid) {
        if !crate::native::protocol::is_managed_unit(&unit) {
            continue;
        }
        let _ = run_command(
            &options.systemctl_bin,
            vec![OsString::from("stop"), OsString::from(&unit)],
            &[],
            SYSTEMCTL_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            MAX_ACCOUNT_OUTPUT,
        )
        .await;
    }
    Ok(())
}

/// Parse `systemctl show` blocks and return the units whose MainPID belongs to
/// `uid`. Used to stop all native work of a user before deleting the account.
pub fn owned_units(show_output: &str, uid: u32) -> Vec<String> {
    let mut units = Vec::new();
    let mut id: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut active = false;
    let mut flush = |id: &mut Option<String>, pid: &mut Option<u32>, active: &mut bool| {
        let owns = match (*pid, *active) {
            (Some(pid), true) if pid > 0 => process_uid(pid) == Some(uid),
            _ => false,
        };
        if owns {
            if let Some(unit) = id.take() {
                units.push(unit);
            }
        }
        *id = None;
        *pid = None;
        *active = false;
    };
    for line in show_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut id, &mut pid, &mut active);
            continue;
        }
        if let Some(value) = line.strip_prefix("Id=") {
            id = Some(value.trim_end_matches(".service").to_string());
        } else if let Some(value) = line.strip_prefix("MainPID=") {
            pid = value.parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("ActiveState=") {
            active = value == "active";
        }
    }
    flush(&mut id, &mut pid, &mut active);
    units
}

fn process_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    let uid = line.split_whitespace().nth(1)?;
    uid.parse().ok()
}

/// Remove a RAST user's host identity. Idempotent: missing accounts still
/// return `Ok(true)`. `remove_home` also deletes `<data_dir>/linux-users/<id>`
/// (never following symlinks) and the user's runtime directory.
pub async fn remove_identity(
    options: &NativeOptions,
    user_id: i64,
    remove_home: bool,
) -> Result<bool, NativeError> {
    if !(1..=MAX_USER_ID).contains(&user_id) {
        return Err(NativeError::invalid(format!(
            "userId must be between 1 and {MAX_USER_ID}"
        )));
    }
    let identity = Identity::derive(options, user_id)?;
    let passwd = getent_passwd(options, &identity.username).await?;
    let group_gid = getent_group(options, &identity.username).await?;

    if passwd.is_some() {
        // Stop all native processes of this identity first; otherwise userdel
        // refuses and, worse, a stray app keeps running without an account.
        stop_units_for_uid(options, identity.uid).await?;
        let output = run_command(
            &options.userdel_bin,
            userdel_args(&identity),
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            MAX_ACCOUNT_OUTPUT,
        )
        .await?;
        if !output.success {
            let still = getent_passwd(options, &identity.username).await?;
            if still.is_some() {
                return Err(NativeError::identity_in_use(format!(
                    "cannot remove user {} while it still has running processes; \
                     stop its native apps and retry: {}",
                    identity.username,
                    crate::native::summarize_command(&output)
                )));
            }
        }
    }

    if group_gid.is_some() {
        let output = run_command(
            &options.groupdel_bin,
            groupdel_args(&identity),
            &[],
            ACCOUNT_CMD_TIMEOUT,
            MAX_ACCOUNT_OUTPUT,
            MAX_ACCOUNT_OUTPUT,
        )
        .await?;
        if !output.success && getent_group(options, &identity.username).await?.is_some() {
            return Err(NativeError::command_failed(format!(
                "cannot remove group {}: {}",
                identity.username,
                crate::native::summarize_command(&output)
            )));
        }
    }

    if remove_home {
        let users_root = options.linux_users_dir();
        if !identity.home.starts_with(&users_root) || identity.home == users_root {
            return Err(NativeError::forbidden(
                "refusing to remove an unexpected home path",
            ));
        }
        match std::fs::symlink_metadata(&identity.home) {
            Ok(_) => crate::native::remove_tree(&identity.home).map_err(|error| {
                NativeError::command_failed(format!(
                    "cannot remove home {}: {error}",
                    identity.home.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(NativeError::unavailable(format!(
                    "cannot inspect home {}: {error}",
                    identity.home.display()
                )))
            }
        }
    }

    let runtime_dir = options.run_dir.join(identity.uid.to_string());
    if runtime_dir.starts_with(&options.run_dir)
        && runtime_dir != options.run_dir
        && std::fs::symlink_metadata(&runtime_dir).is_ok()
    {
        let _ = crate::native::remove_tree(&runtime_dir);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            user_id: 7,
            username: "rast-u7".into(),
            uid: 200_007,
            gid: 200_007,
            home: PathBuf::from("/var/lib/rast/linux-users/7"),
        }
    }

    #[test]
    fn getent_passwd_lines_are_parsed() {
        let entry = parse_getent_passwd(
            "rast-u7:x:200007:200007:RAST Linux app:/var/lib/rast/linux-users/7:/usr/sbin/nologin\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(entry.username, "rast-u7");
        assert_eq!(entry.uid, 200_007);
        assert_eq!(entry.gid, 200_007);
        assert_eq!(entry.home, PathBuf::from("/var/lib/rast/linux-users/7"));
        assert_eq!(parse_getent_passwd("").unwrap(), None);
        assert_eq!(parse_getent_passwd("\n").unwrap(), None);
        assert!(parse_getent_passwd("garbage").is_err());
    }

    #[test]
    fn getent_group_lines_are_parsed() {
        assert_eq!(
            parse_getent_group("rast-u7:x:200007:\n").unwrap(),
            Some(200_007)
        );
        assert_eq!(parse_getent_group("").unwrap(), None);
        assert!(parse_getent_group("garbage").is_err());
        assert!(parse_getent_group("a:b:notanumber:c").is_err());
    }

    #[test]
    fn account_commands_use_structured_arguments_and_no_shell() {
        let identity = identity();
        let args = groupadd_args(&identity);
        assert_eq!(
            args,
            vec![
                OsString::from("--gid"),
                OsString::from("200007"),
                OsString::from("rast-u7")
            ]
        );
        let args = useradd_args(&identity, Path::new("/usr/sbin/nologin"));
        let text: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            vec![
                "--uid",
                "200007",
                "--gid",
                "200007",
                "--home-dir",
                "/var/lib/rast/linux-users/7",
                "--shell",
                "/usr/sbin/nologin",
                "--no-create-home",
                "--comment",
                "RAST Linux app",
                "rast-u7",
            ]
        );
        assert_eq!(userdel_args(&identity), vec![OsString::from("rast-u7")]);
        assert_eq!(groupdel_args(&identity), vec![OsString::from("rast-u7")]);
    }

    #[test]
    fn existing_accounts_are_verified_not_reused() {
        let identity = identity();
        // Matching account: fine.
        let passwd = PasswdEntry {
            username: "rast-u7".into(),
            uid: 200_007,
            gid: 200_007,
            home: PathBuf::from("/var/lib/rast/linux-users/7"),
        };
        assert!(check_existing_identity(&identity, Some(&passwd), Some(200_007)).is_ok());
        assert!(check_existing_identity(&identity, None, None).is_ok());
        // A different group with the expected name is a conflict.
        let error = check_existing_identity(&identity, None, Some(1234)).unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        // A host account with the expected name but another uid is a conflict.
        let stolen = PasswdEntry {
            uid: 0,
            gid: 0,
            ..passwd.clone()
        };
        let error = check_existing_identity(&identity, Some(&stolen), Some(200_007)).unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        assert!(error.message.contains("refusing"));
    }

    #[test]
    fn owned_units_reads_systemctl_show_blocks() {
        // Blocks for other uids/units are ignored; this only parses formatting.
        let output = "Id=rast-xpra-u7.service\nActiveState=active\nMainPID=0\n\n\
                      Id=rast-app-demo.service\nActiveState=inactive\nMainPID=0\n\n";
        assert!(owned_units(output, 200_007).is_empty());
        // A unit with a non-running MainPID is never reported.
        let output = "Id=rast-xpra-u7.service\nActiveState=active\nMainPID=1\n";
        assert!(owned_units(output, 12345).is_empty());
        let output = "Id=rast-xpra-u7\nActiveState=active\nMainPID=999999999\n";
        assert!(owned_units(output, 0).is_empty());
    }

    #[test]
    fn repair_detection_covers_mode_and_owner() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("home");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = std::fs::symlink_metadata(&dir).unwrap();
        // SAFETY: geteuid/getegid have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert!(!needs_repair(&metadata, uid, gid));
        assert!(needs_repair(&metadata, uid + 1, gid));
        assert!(needs_repair(&metadata, uid, gid + 1));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = std::fs::symlink_metadata(&dir).unwrap();
        assert!(needs_repair(&metadata, uid, gid));
    }

    #[tokio::test]
    async fn ensure_home_creates_and_repairs_the_home_directory() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let run = temp.path().join("run");
        std::fs::create_dir_all(&data).unwrap();
        let options = NativeOptions::for_test(data.join("native.sock"), data.clone(), run.clone());
        let identity = Identity::derive(&options, 7).unwrap();
        // Pre-create a wrong-mode home (an older helper left it world-readable).
        std::fs::create_dir_all(&identity.home).unwrap();
        std::fs::set_permissions(&identity.home, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_home(&options, &identity).await.unwrap();
        assert_eq!(
            std::fs::metadata(&identity.home)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // Idempotent.
        ensure_home(&options, &identity).await.unwrap();
        // Runtime directory is created 0700 and repaired the same way.
        let runtime = ensure_runtime_dir(&options, &identity).unwrap();
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn identity_derivation_uses_configuration() {
        let options = NativeOptions::for_test(
            PathBuf::from("/tmp/rast-native-test.sock"),
            PathBuf::from("/tmp/rast-native-test-data"),
            PathBuf::from("/tmp/rast-native-test-run"),
        );
        let identity = Identity::derive(&options, 7).unwrap();
        assert_eq!(identity.username, "rast-u7");
        assert_eq!(identity.uid, options.uid_base + 7);
        assert_eq!(identity.gid, options.uid_base + 7);
        assert_eq!(
            identity.home,
            PathBuf::from("/tmp/rast-native-test-data/linux-users/7")
        );
        assert!(Identity::derive(&options, 0).is_err());
        assert!(Identity::derive(&options, MAX_USER_ID + 1).is_err());
    }
}
