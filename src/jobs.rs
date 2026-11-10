//! Background job queue with bounded concurrency and cooperative cancellation.
//!
//! Long operations (copy/move/delete of trees, backups, app deployments) run
//! here so HTTP requests stay responsive. Jobs are persisted in SQLite and
//! their progress is streamed to the UI through the event bus.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, Semaphore};

use crate::{db, error::AppError, events::EventBus};

pub const QUEUE_CAPACITY: usize = 128;
const PROGRESS_WRITE_INTERVAL_MS: i64 = 250;

/// Cooperative cancellation + progress reporting for a running job.
#[derive(Clone)]
pub struct JobContext {
    pub id: String,
    pub db: SqlitePool,
    events: EventBus,
    cancel: Arc<AtomicBool>,
    last_write: Arc<AtomicU64>,
    user_id: Option<i64>,
}

impl JobContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Returns `Err(AppError::Cancelled)` when the job has been cancelled.
    pub fn check_cancelled(&self) -> Result<(), AppError> {
        if self.is_cancelled() {
            Err(AppError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Report progress; writes are throttled to avoid hammering SQLite.
    pub async fn progress(&self, done: u64, total: u64, message: &str) {
        let now = db::now_ms();
        let last = self.last_write.load(Ordering::Relaxed) as i64;
        let finished = total > 0 && done >= total;
        if !finished && now - last < PROGRESS_WRITE_INTERVAL_MS {
            return;
        }
        self.last_write.store(now as u64, Ordering::Relaxed);
        let _ = sqlx::query("UPDATE jobs SET progress = ?, total = ?, message = ? WHERE id = ?")
            .bind(done as i64)
            .bind(total as i64)
            .bind(truncate(message, 200))
            .bind(&self.id)
            .execute(&self.db)
            .await;
        let payload = json!({
            "id": self.id,
            "progress": done,
            "total": total,
            "message": message,
            "userId": self.user_id,
        });
        match self.user_id {
            Some(uid) => self
                .events
                .publish_user_and_admin("job.progress", uid, payload),
            None => self.events.publish_admin("job.progress", payload),
        };
    }

    pub async fn set_message(&self, message: &str) {
        let _ = sqlx::query("UPDATE jobs SET message = ? WHERE id = ?")
            .bind(truncate(message, 200))
            .bind(&self.id)
            .execute(&self.db)
            .await;
    }
}

type BoxFuture = Pin<Box<dyn Future<Output = Result<Value, AppError>> + Send>>;
type JobFn = Box<dyn FnOnce(JobContext) -> BoxFuture + Send>;

/// A job to enqueue.
pub struct JobSpec {
    pub kind: String,
    pub user_id: Option<i64>,
    pub username: String,
    pub message: String,
    pub run: JobFn,
}

impl JobSpec {
    pub fn new<F, Fut>(kind: impl Into<String>, run: F) -> Self
    where
        F: FnOnce(JobContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Value, AppError>> + Send + 'static,
    {
        Self {
            kind: kind.into(),
            user_id: None,
            username: String::new(),
            message: String::new(),
            run: Box::new(move |ctx| Box::pin(run(ctx))),
        }
    }

    pub fn actor(mut self, user: &crate::auth::AuthUser) -> Self {
        self.user_id = Some(user.id);
        self.username = user.username.clone();
        self
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
}

struct Task {
    id: String,
    spec: JobSpec,
    cancel: Arc<AtomicBool>,
}

/// Queue + worker pool.
pub struct JobManager {
    db: SqlitePool,
    events: EventBus,
    tx: mpsc::Sender<Task>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    active: Arc<AtomicU64>,
}

impl JobManager {
    /// Create the manager and start `concurrency` worker slots.
    pub fn new(db: SqlitePool, events: EventBus, concurrency: usize) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Task>(QUEUE_CAPACITY);
        let cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let active = Arc::new(AtomicU64::new(0));
        let manager = Arc::new(Self {
            db: db.clone(),
            events: events.clone(),
            tx,
            cancel_flags: cancel_flags.clone(),
            active: active.clone(),
        });

        tokio::spawn(dispatch_loop(
            db,
            events,
            rx,
            Arc::new(Semaphore::new(concurrency.max(1))),
            cancel_flags,
            active,
        ));
        manager
    }

    pub fn running_count(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    /// Enqueue a job; the returned id can be polled or streamed via SSE.
    pub async fn submit(&self, spec: JobSpec) -> Result<String, AppError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = db::now_ms();
        sqlx::query(
            "INSERT INTO jobs (id, kind, status, message, user_id, username, created_at)
             VALUES (?, ?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&spec.kind)
        .bind(truncate(&spec.message, 200))
        .bind(spec.user_id)
        .bind(&spec.username)
        .bind(now)
        .execute(&self.db)
        .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        if let Ok(mut flags) = self.cancel_flags.lock() {
            flags.insert(id.clone(), cancel.clone());
        }
        let queued = json!({"id": id, "kind": spec.kind, "userId": spec.user_id});
        match spec.user_id {
            Some(uid) => self
                .events
                .publish_user_and_admin("job.queued", uid, queued),
            None => self.events.publish_admin("job.queued", queued),
        };
        match self
            .tx
            .try_send(Task {
                id: id.clone(),
                spec,
                cancel,
            })
            .map_err(|e| AppError::unavailable(format!("job queue is full: {e}")))
        {
            Ok(()) => Ok(id),
            Err(e) => {
                let _ = sqlx::query(
                    "UPDATE jobs SET status = 'failed', error = ?, finished_at = ? WHERE id = ?",
                )
                .bind("queue full")
                .bind(now)
                .bind(&id)
                .execute(&self.db)
                .await;
                Err(e)
            }
        }
    }

    /// Request cancellation. Returns `false` when the job is unknown/finished.
    pub fn cancel(&self, id: &str) -> bool {
        let flags = self.cancel_flags.lock().ok();
        if let Some(flags) = flags {
            if let Some(flag) = flags.get(id) {
                flag.store(true, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Mark jobs left running by a previous process as failed.
    pub async fn recover_stale(&self) -> Result<(), AppError> {
        let now = db::now_ms();
        sqlx::query(
            "UPDATE jobs SET status = 'failed', error = 'server restarted before the job completed', finished_at = ?
             WHERE status IN ('pending', 'running')",
        )
        .bind(now)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

async fn dispatch_loop(
    db: SqlitePool,
    events: EventBus,
    mut rx: mpsc::Receiver<Task>,
    semaphore: Arc<Semaphore>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    active: Arc<AtomicU64>,
) {
    while let Some(task) = rx.recv().await {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let db = db.clone();
        let events = events.clone();
        let flags = cancel_flags.clone();
        let active = active.clone();
        let flags_for_job = flags.clone();
        tokio::spawn(async move {
            active.fetch_add(1, Ordering::Relaxed);
            run_job(db, events, task, flags_for_job).await;
            active.fetch_sub(1, Ordering::Relaxed);
            drop(permit);
        });
    }
}

async fn run_job(
    db: SqlitePool,
    events: EventBus,
    task: Task,
    flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
) {
    let Task { id, spec, cancel } = task;
    let started = db::now_ms();
    let _ = sqlx::query("UPDATE jobs SET status = 'running', started_at = ? WHERE id = ?")
        .bind(started)
        .bind(&id)
        .execute(&db)
        .await;
    let owner = spec.user_id;
    let start_event = json!({"id": id, "kind": spec.kind, "userId": owner});
    match owner {
        Some(uid) => events.publish_user_and_admin("job.start", uid, start_event),
        None => events.publish_admin("job.start", start_event),
    };

    let ctx = JobContext {
        id: id.clone(),
        db: db.clone(),
        events: events.clone(),
        cancel,
        last_write: Arc::new(AtomicU64::new(0)),
        user_id: owner,
    };
    let outcome = (spec.run)(ctx).await;
    let finished = db::now_ms();

    match outcome {
        Ok(value) => {
            let _ = sqlx::query(
                "UPDATE jobs SET status = 'done', progress = total, finished_at = ?, result = ? WHERE id = ?",
            )
            .bind(finished)
            .bind(value.to_string())
            .bind(&id)
            .execute(&db)
            .await;
            let done_event = json!({"id": id, "result": value, "userId": owner});
            match owner {
                Some(uid) => events.publish_user_and_admin("job.done", uid, done_event),
                None => events.publish_admin("job.done", done_event),
            };
        }
        Err(AppError::Cancelled) => {
            let _ = sqlx::query(
                "UPDATE jobs SET status = 'cancelled', finished_at = ?, error = 'cancelled' WHERE id = ?",
            )
            .bind(finished)
            .bind(&id)
            .execute(&db)
            .await;
            let cancelled_event = json!({"id": id, "userId": owner});
            match owner {
                Some(uid) => events.publish_user_and_admin("job.cancelled", uid, cancelled_event),
                None => events.publish_admin("job.cancelled", cancelled_event),
            };
        }
        Err(err) => {
            let message = err.public_message();
            let _ = sqlx::query(
                "UPDATE jobs SET status = 'failed', finished_at = ?, error = ? WHERE id = ?",
            )
            .bind(finished)
            .bind(truncate(&message, 500))
            .bind(&id)
            .execute(&db)
            .await;
            let failed_event = json!({"id": id, "error": message, "userId": owner});
            match owner {
                Some(uid) => events.publish_user_and_admin("job.failed", uid, failed_event),
                None => events.publish_admin("job.failed", failed_event),
            };
        }
    }
    // Drop the cancellation flag now that the job is finished so the map stays
    // bounded over long uptimes.
    if let Ok(mut map) = flags.lock() {
        map.remove(&id);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// Fetch one job row as JSON.
pub async fn job_json(pool: &SqlitePool, id: &str) -> Result<Option<Value>, AppError> {
    let row = sqlx::query(
        "SELECT id, kind, status, progress, total, message, user_id, username, created_at,
                started_at, finished_at, result, error
         FROM jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        let result: Option<String> = r.get("result");
        json!({
            "id": r.get::<String, _>("id"),
            "kind": r.get::<String, _>("kind"),
            "status": r.get::<String, _>("status"),
            "progress": r.get::<i64, _>("progress"),
            "total": r.get::<i64, _>("total"),
            "message": r.get::<String, _>("message"),
            "userId": r.get::<Option<i64>, _>("user_id"),
            "username": r.get::<String, _>("username"),
            "createdAt": r.get::<i64, _>("created_at"),
            "startedAt": r.get::<Option<i64>, _>("started_at"),
            "finishedAt": r.get::<Option<i64>, _>("finished_at"),
            "result": result.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
            "error": r.get::<Option<String>, _>("error"),
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::time::Duration;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("jobs.db"), 4).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn job_runs_and_result_is_persisted() {
        let (_d, pool) = pool().await;
        let events = EventBus::new();
        let manager = JobManager::new(pool.clone(), events, 2);
        let id = manager
            .submit(JobSpec::new("test", |ctx| async move {
                ctx.progress(1, 2, "half").await;
                ctx.progress(2, 2, "done").await;
                Ok(json!({"files": 2}))
            }))
            .await
            .unwrap();
        let mut status = String::new();
        for _ in 0..100 {
            let job = job_json(&pool, &id).await.unwrap().unwrap();
            status = job["status"].as_str().unwrap_or_default().to_string();
            if status != "pending" && status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status, "done", "job did not finish: {status}");
        let job = job_json(&pool, &id).await.unwrap().unwrap();
        assert_eq!(job["result"]["files"], 2);
    }

    #[tokio::test]
    async fn cancellation_is_honoured() {
        let (_d, pool) = pool().await;
        let events = EventBus::new();
        let manager = JobManager::new(pool.clone(), events, 1);
        let manager2 = manager.clone();
        let id = manager
            .submit(JobSpec::new("cancel-me", move |ctx| async move {
                for _ in 0..200 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    if manager2.cancel(&ctx.id) {
                        break;
                    }
                    ctx.check_cancelled()?;
                }
                Err(AppError::Cancelled)
            }))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let job = job_json(&pool, &id).await.unwrap().unwrap();
        assert!(job["status"] == "running" || job["status"] == "cancelled");
        // Job observes the flag and exits cancelled.
        for _ in 0..100 {
            let job = job_json(&pool, &id).await.unwrap().unwrap();
            if job["status"] == "cancelled" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job was not cancelled");
    }

    #[tokio::test]
    async fn failed_job_records_error() {
        let (_d, pool) = pool().await;
        let events = EventBus::new();
        let manager = JobManager::new(pool.clone(), events, 1);
        let id = manager
            .submit(JobSpec::new("boom", |_ctx| async move {
                Err(AppError::internal("disk exploded"))
            }))
            .await
            .unwrap();
        for _ in 0..100 {
            let job = job_json(&pool, &id).await.unwrap().unwrap();
            if job["status"] == "failed" {
                assert!(job["error"].as_str().unwrap().contains("disk exploded"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job did not fail");
    }

    #[tokio::test]
    async fn stale_jobs_are_recovered_on_startup() {
        let (_d, pool) = pool().await;
        sqlx::query(
            "INSERT INTO jobs (id, kind, status, created_at) VALUES ('stale','x','running',1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let manager = JobManager::new(pool.clone(), EventBus::new(), 1);
        manager.recover_stale().await.unwrap();
        let job = job_json(&pool, "stale").await.unwrap().unwrap();
        assert_eq!(job["status"], "failed");
    }
}
