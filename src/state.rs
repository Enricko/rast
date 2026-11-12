//! Shared application state handed to every handler.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sqlx::SqlitePool;

use crate::{
    auth::rate_limit::RateLimiter,
    config::Config,
    db,
    error::{AppError, AppResult},
    events::EventBus,
    jobs::JobManager,
    secrets::Secrets,
};

/// Everything a request handler may need. Cheap to clone (all fields are `Arc`
/// or pool handles).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub secrets: Arc<Secrets>,
    pub events: EventBus,
    pub jobs: Arc<JobManager>,
    pub monitor: Arc<crate::monitor::Monitor>,
    pub storage: Arc<crate::storage::Storage>,
    pub docker: Arc<crate::docker::DockerManager>,
    pub packages: Arc<crate::packages::PackageManager>,
    pub linux_apps: Arc<crate::linux_apps::LinuxApps>,
    pub started_at: i64,
    pub login_ip_limiter: Arc<RateLimiter>,
    pub login_user_limiter: Arc<RateLimiter>,
}

impl AppState {
    /// Build the full state: directories, database, secrets, subsystems.
    pub async fn new(config: Config) -> AppResult<Self> {
        config
            .ensure_dirs()
            .map_err(|e| AppError::internal(format!("cannot create directories: {e}")))?;
        let db = db::init(&config).await?;
        let secrets = Secrets::load_or_create(&config.secret_path())
            .map_err(|e| AppError::internal(format!("cannot load server secrets: {e}")))?;
        let events = EventBus::new();
        let jobs = JobManager::new(db.clone(), events.clone(), config.job_concurrency);
        jobs.recover_stale().await?;

        crate::backup::schedule::ensure_default(&db).await?;

        let monitor = crate::monitor::Monitor::new(&config)?;
        let storage = Arc::new(
            crate::storage::Storage::new(&config)
                .map_err(|e| AppError::internal(format!("cannot initialise storage: {e}")))?,
        );
        let docker = crate::docker::DockerManager::new(&config);

        let config = Arc::new(config);
        let packages = crate::packages::PackageManager::new(
            config.clone(),
            db.clone(),
            events.clone(),
            jobs.clone(),
        );
        // Restores the catalog check state and starts the periodic checker.
        packages.start().await;
        let linux_apps = crate::linux_apps::LinuxApps::new(
            config.clone(),
            db.clone(),
            docker.clone(),
            events.clone(),
            jobs.clone(),
        );
        // Stops copies left from before a restart, then sweeps and checks.
        linux_apps.start().await;
        // Background maintenance: retention + session cleanup + monitor sampling.
        spawn_maintenance(
            db.clone(),
            monitor.clone(),
            events.clone(),
            config.monitor_interval_secs,
            storage.clone(),
            config.clone(),
        );
        // Scheduled backups (disabled until the operator enables them).
        crate::backup::schedule::spawn(db.clone(), config.clone(), jobs.clone(), events.clone());
        // Realtime filesystem watcher (external File Station changes).
        crate::realtime::watch::spawn(&config, db.clone(), events.clone());
        // Realtime container/application reconciler (external Docker changes).
        crate::docker::reconcile::spawn(&config, db.clone(), docker.clone(), events.clone());

        Ok(Self {
            config,
            db,
            secrets: Arc::new(secrets),
            events,
            jobs,
            monitor,
            storage,
            docker,
            packages,
            linux_apps,
            started_at: now_ms(),
            login_ip_limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(300))),
            login_user_limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(900))),
        })
    }

    pub fn version(&self) -> &'static str {
        crate::VERSION
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.config.data_dir
    }

    pub fn bind_addr(&self) -> String {
        self.config.bind.to_string()
    }

    /// Public, unauthenticated summary used by `/healthz` and `rast status`.
    pub fn health_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": crate::VERSION,
            "uptimeSecs": now_ms().saturating_sub(self.started_at) / 1000,
            "monitorFresh": self.monitor.is_fresh(),
        })
    }
}

/// Spawn the maintenance timer: monitor sampling, retention pruning, session
/// cleanup. The monitor sampler is process-wide, not per browser tab.
fn spawn_maintenance(
    db: SqlitePool,
    monitor: Arc<crate::monitor::Monitor>,
    events: EventBus,
    interval_secs: u64,
    storage: Arc<crate::storage::Storage>,
    config: Arc<Config>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tick: u64 = 0;
        loop {
            ticker.tick().await;
            let snapshot = monitor.sample().await;
            events.publish("monitor", snapshot);
            tick = tick.wrapping_add(1);
            // Every ~5 minutes: prune sessions, old rows and expired trash.
            if tick.is_multiple_of((300 / interval_secs.max(1)).max(1)) {
                if let Err(e) = crate::auth::session::cleanup(&db).await {
                    tracing::warn!(error = %e, "session cleanup failed");
                }
                let audit_days = db::get_setting(&db, "auditRetentionDays")
                    .await
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(90);
                if let Err(e) = db::prune_old_rows(&db, audit_days, 30).await {
                    tracing::warn!(error = %e, "retention pruning failed");
                }
                let trash_days = db::get_setting(&db, "trashRetentionDays")
                    .await
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(30);
                match crate::storage::trash::cleanup_all_expired(&db, &storage, trash_days).await {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "expired recycle-bin items purged"),
                    Err(e) => tracing::warn!(error = %e, "trash cleanup failed"),
                }
            }
            // Once a week: rebuild per-user storage usage from the filesystem so
            // optimistic quota accounting can never drift permanently.
            let weekly_ticks = (7 * 24 * 3600 / interval_secs.max(1)).max(1);
            if tick > 0 && tick.is_multiple_of(weekly_ticks) {
                match crate::storage::quota::recalculate_all(&db, &storage).await {
                    Ok(users) => tracing::info!(users, "weekly quota recalculation finished"),
                    Err(e) => tracing::warn!(error = %e, "quota recalculation failed"),
                }
            }
            let _ = &config;
        }
    });
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
