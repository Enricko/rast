//! `rast restore-files` (review F1, decision A6): restore the `users` part
//! of a Backup Center archive as root, without trusting the archive.
//!
//! The checks and the writer are in the root helper's crate
//! ([`crate::native::restore_files`]); this module gathers what they need:
//! the host's account files, the RAST accounts and shared folders from the
//! database (read without changing it), the service account, and the
//! target folder, and runs the restore on a blocking thread.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::native::restore_files::{
    open_archive, restore, Facts, HostAccounts, Options, Owners, Report, Target,
};

/// The account the RAST service runs as (created by the installer).
pub const SERVICE_ACCOUNT: &str = "rast";

/// Where the restore writes.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Check every entry, write nothing.
    DryRun,
    /// A new folder (default: `<storage_dir>/restore-<time>`).
    Scratch(Option<PathBuf>),
    /// The live `homes` and `shared` folders (the service stopped): only
    /// folders that do not exist there.
    Storage,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub archive: PathBuf,
    /// `users/homes/<name>` or `users/shared/<container>`; empty: all.
    pub trees: Vec<String>,
    pub mode: Mode,
}

/// What the restore checks against.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub facts: Facts,
    pub host: HostAccounts,
    pub service_uid: u32,
    pub service_gid: u32,
    pub owners: Owners,
}

/// Restore with the host's account files (`/etc/passwd`, `/etc/group`)
/// and the live database. Root only: it sets owners.
pub async fn run(cfg: &Config, options: RunOptions) -> AppResult<Report> {
    let host = HostAccounts::read(Path::new("/etc"))
        .map_err(|error| AppError::internal(format!("cannot read the host accounts: {error}")))?;
    let (service_uid, service_gid) = host.user(SERVICE_ACCOUNT).ok_or_else(|| {
        AppError::internal(format!(
            "the host has no {SERVICE_ACCOUNT} account; is RAST installed?"
        ))
    })?;
    let facts = load_facts(&cfg.db_path()).await?;
    run_with(
        cfg,
        options,
        Inputs {
            facts,
            host,
            service_uid,
            service_gid,
            owners: Owners::Apply,
        },
    )
    .await
}

/// [`run`] with the account facts given (tests).
pub async fn run_with(cfg: &Config, options: RunOptions, inputs: Inputs) -> AppResult<Report> {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let target = match options.mode {
        Mode::DryRun => Target::DryRun,
        Mode::Scratch(Some(path)) => Target::Scratch(path),
        Mode::Scratch(None) => Target::Scratch(cfg.storage_dir.join(format!("restore-{stamp}"))),
        Mode::Storage => Target::Storage {
            storage: cfg.storage_dir.clone(),
            staging: format!(".restore-{stamp}"),
        },
    };
    let file = open_archive(&options.archive).map_err(|error| {
        AppError::bad_request(format!(
            "cannot open {}: {error}",
            options.archive.display()
        ))
    })?;
    let restore_options = Options {
        uid_base: cfg.linux_uid_base,
        service_uid: inputs.service_uid,
        service_gid: inputs.service_gid,
        trees: options.trees,
        target,
        owners: inputs.owners,
    };
    tokio::task::spawn_blocking(move || {
        let mut input = GzDecoder::new(BufReader::with_capacity(256 * 1024, file));
        restore(&mut input, &restore_options, &inputs.facts, &inputs.host)
    })
    .await
    .map_err(|error| AppError::internal(format!("the restore stopped: {error}")))?
    .map_err(AppError::bad_request)
}

/// The accounts and shared folders in the database, read without writing
/// anything: root must not leave root-owned `-wal` or `-shm` files next to
/// the service's database. With both present (the service running, or
/// stopped uncleanly) the database is opened read-only; otherwise as
/// immutable, which creates no file at all.
pub async fn load_facts(db: &Path) -> AppResult<Facts> {
    let sibling = |suffix: &str| {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let options = SqliteConnectOptions::new()
        .filename(db)
        .create_if_missing(false);
    let options = if sibling("-wal").exists() && sibling("-shm").exists() {
        options.read_only(true)
    } else {
        options.immutable(true)
    };
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|error| {
            AppError::internal(format!(
                "cannot open the database {}: {error}",
                db.display()
            ))
        })?;
    let accounts: Vec<(i64, String)> = sqlx::query_as("SELECT id, username FROM users")
        .fetch_all(&pool)
        .await?;
    let shares: Vec<(String, String)> = sqlx::query_as("SELECT id, container_path FROM shares")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(Facts {
        accounts: accounts.into_iter().map(|(id, name)| (name, id)).collect(),
        shares: shares
            .into_iter()
            .map(|(id, container)| (container, id))
            .collect(),
    })
}
