//! Terminal sandboxes: shells for accounts that are not administrators.
//!
//! Administrators' Terminal is a root shell on the server ([`crate::terminal`]).
//! Everyone else an administrator grants the Terminal app (a group or
//! per-account app rule) gets shells in **their own container** instead,
//! started by the Docker worker (`crate::worker::sandbox`):
//!
//! * the account's home folder is the only RAST storage inside it, at
//!   `/home/<name>`; App Center's runtimes are there read-only;
//! * the root filesystem is read-only, `/tmp` is a small memory-backed
//!   folder, and the shell runs as the account that owns the home folders
//!   (so File Station sees the files as usual) with no capabilities,
//!   `no-new-privileges`, and limits on memory, CPU and processes;
//! * there is **no network**: a container on Docker's bridge could reach
//!   services on the server and on its private networks (LAN, VPN).
//!
//! One container per account holds all of its shells; it is removed after
//! [`IDLE_REMOVE`] without a shell, when the account loses access, or when it
//! is deactivated or deleted. Files in the home folder stay. The image
//! ([`DOCKERFILE`]) is built on the server the first time it is needed and
//! named after a hash of that file.
//!
//! The worker serves the same socket protocol as the root helper
//! ([`crate::terminal::Request`]) on [`socket_path`], next to its own socket,
//! for the web server's account only.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{
    auth::{AuthUser, Role},
    config::Config,
    error::{AppError, AppResult},
    terminal::{helper::Limits, TerminalClient, TerminalError},
};

/// The sandbox image recipe.
pub const DOCKERFILE: &str = include_str!("Dockerfile");
/// Image repository; the tag is a hash of [`DOCKERFILE`].
pub const IMAGE_REPO: &str = "rast-shell";
/// Containers are named `rast-shell-<account id>`.
pub const CONTAINER_PREFIX: &str = "rast-shell-";
/// Label on every sandbox container (`1`).
pub const LABEL: &str = "rast.shell";
/// Label with the owning account id.
pub const LABEL_USER: &str = "rast.shell.user";
/// Label with a hash of how the container was made (recreated on change).
pub const LABEL_SPEC: &str = "rast.shell.spec";
/// The app id that grants sandboxes to non-administrators.
pub const APP_ID: &str = "terminal";

/// Memory (and memory + swap) of one account's container, in MiB.
pub const MEMORY_MB: u64 = 1024;
/// CPUs one account's container may use.
pub const CPUS: &str = "2";
/// Processes (and threads) in one account's container.
pub const PIDS: u32 = 512;
/// Size of `/tmp` (memory-backed, counts towards [`MEMORY_MB`]), in MiB.
pub const TMP_MB: u64 = 512;
/// Shells open at once: on the server, and per account.
pub const LIMITS: Limits = Limits {
    total: 16,
    per_owner: 4,
};
/// A container without shells for this long is removed.
pub const IDLE_REMOVE: Duration = Duration::from_secs(15 * 60);
/// How long opening a shell may take (starting the container).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(90);
/// How long building the image may take (slow downloads included).
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// `rast-shell:<first 12 hex digits of the Dockerfile's SHA-256>`.
pub fn image_ref() -> String {
    let digest = Sha256::digest(DOCKERFILE.as_bytes());
    format!("{IMAGE_REPO}:{}", &hex::encode(digest)[..12])
}

/// The worker's sandbox socket: `shells.sock` next to `worker.sock`.
pub fn socket_path(worker_socket: &Path) -> PathBuf {
    worker_socket.with_file_name("shells.sock")
}

/// The web server's client for sandbox shells.
pub fn client(cfg: &Config) -> TerminalClient {
    TerminalClient::new(socket_path(&cfg.worker_socket)).with_open_timeout(OPEN_TIMEOUT)
}

/// Account names that can name a home folder and a passwd entry.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// End an account's sandbox shells and remove its container (lost access,
/// deactivated, deleted). Best effort: nothing to do without Docker.
pub async fn end_shells_of(cfg: &Config, user: i64) {
    match client(cfg).kill_user(user).await {
        Ok(_) | Err(TerminalError::Off) => {}
        Err(error) => {
            tracing::warn!(user, error = %error, "could not end the account's sandbox shells")
        }
    }
}

/// Whether an account may use a sandbox right now (active, granted the app).
pub async fn allowed(db: &sqlx::SqlitePool, user_id: i64) -> bool {
    let Ok(Some(record)) = crate::users::find_by_id(db, user_id).await else {
        return false;
    };
    if !record.active {
        return false;
    }
    let user = AuthUser {
        id: record.id,
        username: record.username,
        display_name: record.display_name,
        role: record.role,
        session_id: String::new(),
        quota_bytes: record.quota_bytes,
        must_change_password: record.must_change_password,
    };
    crate::access::app_access(db, &user, APP_ID)
        .await
        .is_ok_and(|access| access.allowed)
}

/// Get the account's home folder ready for a sandbox: create it when it does
/// not exist yet, close the storage roots to other server accounts (files a
/// shell creates could carry set-user-id bits), count what earlier shells
/// wrote, and refuse when the home folder is over its quota.
pub async fn ready_home(
    db: &sqlx::SqlitePool,
    storage: &crate::storage::Storage,
    user: &AuthUser,
) -> AppResult<()> {
    let name = user.username.to_ascii_lowercase();
    if !valid_name(&name) {
        return Err(AppError::conflict(
            "Terminal cannot be used with this account name",
        ));
    }
    let home = storage.home_dir(&name);
    let roots = [storage.homes_root.clone(), storage.shared_root.clone()];
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for root in roots {
            let mode = std::fs::metadata(&root)?.permissions().mode();
            if mode & 0o007 != 0 {
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode & 0o770))?;
            }
        }
        if std::fs::symlink_metadata(&home).is_err() {
            std::fs::create_dir(&home)?;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    })
    .await
    .map_err(AppError::from)?
    .map_err(|e| AppError::internal(format!("cannot prepare the home folder: {e}")))?;

    let used = crate::storage::quota::recalculate(db, storage, user.id, &name).await?;
    let quota = crate::storage::quota::info(db, user.id).await?;
    if !quota.is_unlimited() && used >= quota.quota_bytes {
        return Err(AppError::conflict(format!(
            "Your home folder is over its quota ({} of {}). Free up space in File Station, then open Terminal again.",
            human_bytes(used),
            human_bytes(quota.quota_bytes)
        )));
    }
    Ok(())
}

fn human_bytes(bytes: i64) -> String {
    let mut value = bytes.max(0) as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if value < 1024.0 || unit == "TB" {
            return if unit == "B" {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

/// Every minute, end shells whose owner may no longer have them: sandbox
/// shells of accounts that lost the Terminal app or were deactivated, and
/// root shells of accounts that are no longer active administrators. (Role
/// changes and deactivation also end them at once; this catches group rule
/// changes and anything missed.)
pub fn spawn_access_sweeper(db: sqlx::SqlitePool, cfg: std::sync::Arc<Config>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let sandboxes = client(&cfg);
            if let Ok(owners) = sandboxes.owners().await {
                for owner in owners {
                    if !allowed(&db, owner).await {
                        tracing::info!(user = owner, "ending sandbox shells: no access anymore");
                        let _ = sandboxes.kill_user(owner).await;
                    }
                }
            }
            let root = TerminalClient::default();
            if let Ok(owners) = root.owners().await {
                for owner in owners {
                    let admin = matches!(
                        crate::users::find_by_id(&db, owner).await,
                        Ok(Some(record)) if record.active && record.role == Role::Admin
                    );
                    if !admin {
                        tracing::info!(user = owner, "ending root shells: not an administrator");
                        let _ = root.kill_user(owner).await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        for good in ["alice", "t-bob", "a.b_c", "x1"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Alice",
            "-x",
            ".x",
            "a/b",
            "a b",
            "a:b",
            &"a".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_image_is_named_after_its_recipe() {
        let image = image_ref();
        assert!(image.starts_with("rast-shell:"));
        assert_eq!(image.len(), "rast-shell:".len() + 12);
        assert!(DOCKERFILE.contains("FROM ubuntu:24.04"));
    }

    #[test]
    fn the_socket_sits_next_to_the_workers() {
        assert_eq!(
            socket_path(Path::new("/run/rast/worker.sock")),
            Path::new("/run/rast/shells.sock")
        );
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }
}
