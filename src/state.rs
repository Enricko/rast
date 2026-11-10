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
        let secrets = Secrets::load_or_create(&config.secret_path()).map_err(|e| {
            AppError::internal(format!("cannot load server secrets: {e}"))
        })?;
        let events = EventBus::new();
        let jobs = JobManager::new(db.clone(), events.clone(), config.job_concurrency);
        jobs.recover_stale().await?;

        let monitor = crate::monitor::Monitor::new(&config)?;
        let storage = crate::storage::Storage::new(&config).map_err(|e| {
            AppError::internal(format!("cannot initialise storage: {e}"))
        })?;
        let docker = crate::docker::DockerManager::new(&config);

        // Background maintenance: retention + session cleanup + monitor sampling.
        spawn_maintenance(
            db.clone(),
            monitor.clone(),
            events.clone(),
            config.monitor_interval_secs,
        );

        Ok(Self {
            config: Arc::new(config),
            db,
            secrets: Arc::new(secrets),
            events,
            jobs,
            monitor,
            storage: Arc::new(storage),
            docker,
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
            // Every ~5 minutes: prune sessions and old rows.
            if tick % (300 / interval_secs.max(1)).max(1) == 0 {
                if let Err(e) = crate::auth::session::cleanup(&db).await {
                    tracing::warn!(error = %e, "session cleanup failed");
                }
                if let Err(e) = db::prune_old_rows(&db, 90, 30).await {
                    tracing::warn!(error = %e, "retention pruning failed");
                }
            }
        }
    });
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
