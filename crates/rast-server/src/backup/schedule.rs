//! Backup scheduling and retention.
//!
//! Schedules are stored in SQLite and evaluated by a background task. Times are
//! entered by the operator in the server's local timezone.

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::{
    backup::{self, ProgressFn},
    config::Config,
    db,
    error::AppResult,
    events::EventBus,
    jobs::{JobManager, JobSpec},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: i64,
    pub enabled: bool,
    pub frequency: String,
    pub time_of_day: String,
    pub weekday: i64,
    pub retention_count: i64,
    pub scopes: Vec<String>,
    pub last_run_at: Option<i64>,
    pub next_run_at: i64,
}

impl Schedule {
    pub fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        let scopes: Vec<String> = row
            .get::<String, _>("scopes")
            .parse::<serde_json::Value>()
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|items| {
                items
                    .into_iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_else(|| vec!["db".into(), "config".into()]);
        Self {
            id: row.get("id"),
            enabled: row.get::<i64, _>("enabled") != 0,
            frequency: row.get("frequency"),
            time_of_day: row.get("time_of_day"),
            weekday: row.get("weekday"),
            retention_count: row.get("retention_count"),
            scopes,
            last_run_at: row.get("last_run_at"),
            next_run_at: row.get("next_run_at"),
        }
    }
}

/// Parse "HH:MM" with a safe fallback.
pub fn parse_time_of_day(value: &str) -> (u32, u32) {
    let mut parts = value.split(':');
    let hour = parts.next().and_then(|v| v.trim().parse::<u32>().ok());
    let minute = parts.next().and_then(|v| v.trim().parse::<u32>().ok());
    match (hour, minute) {
        (Some(h), Some(m)) if h < 24 && m < 60 => (h, m),
        _ => (3, 0),
    }
}

/// Compute the next run timestamp (epoch ms) strictly after `from_ms`.
pub fn compute_next_run(frequency: &str, time_of_day: &str, weekday: i64, from_ms: i64) -> i64 {
    let (hour, minute) = parse_time_of_day(time_of_day);
    let from = Local
        .timestamp_millis_opt(from_ms)
        .single()
        .unwrap_or_else(Local::now);
    let time = NaiveTime::from_hms_opt(hour, minute, 0)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(3, 0, 0).expect("3:00 is a valid time"));

    let candidate = match frequency {
        "hourly" => {
            // Next occurrence of the configured minute within the hour.
            let mut candidate = from
                .with_minute(minute)
                .and_then(|c| c.with_second(0))
                .and_then(|c| c.with_nanosecond(0))
                .unwrap_or(from);
            if candidate <= from {
                candidate += ChronoDuration::hours(1);
            }
            candidate
        }
        "weekly" => {
            let target = weekday.clamp(0, 6) as u32;
            let current = from.weekday().num_days_from_sunday();
            let mut days_ahead = (target + 7 - current) % 7;
            let mut candidate = from
                .date_naive()
                .and_time(time)
                .and_local_timezone(Local)
                .earliest()
                .unwrap_or(from);
            if days_ahead == 0 && candidate <= from {
                days_ahead = 7;
            }
            if days_ahead > 0 {
                candidate += ChronoDuration::days(days_ahead as i64);
            }
            candidate
        }
        _ => {
            let mut candidate = from
                .date_naive()
                .and_time(time)
                .and_local_timezone(Local)
                .earliest()
                .unwrap_or(from);
            if candidate <= from {
                candidate += ChronoDuration::days(1);
            }
            candidate
        }
    };
    candidate.timestamp_millis()
}

/// Load all schedules.
pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Schedule>> {
    let rows = sqlx::query(
        "SELECT id, enabled, frequency, time_of_day, weekday, retention_count, scopes, last_run_at, next_run_at
         FROM backup_schedules ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(Schedule::from_row).collect())
}

/// Validate a schedule payload and return normalised values.
pub fn validate(
    frequency: &str,
    time_of_day: &str,
    weekday: i64,
    retention: i64,
    scopes: &[String],
) -> Result<(String, String, i64, i64, Vec<String>), String> {
    if !["hourly", "daily", "weekly"].contains(&frequency) {
        return Err("frequency must be hourly, daily or weekly".into());
    }
    let (hour, minute) = parse_time_of_day(time_of_day);
    let normalized_time = format!("{hour:02}:{minute:02}");
    if !(0..=6).contains(&weekday) {
        return Err("weekday must be between 0 (Sunday) and 6 (Saturday)".into());
    }
    if !(1..=365).contains(&retention) {
        return Err("retentionCount must be between 1 and 365".into());
    }
    let scopes = backup::normalize_scopes(scopes).map_err(|e| e.public_message())?;
    Ok((
        frequency.to_string(),
        normalized_time,
        weekday,
        retention,
        scopes,
    ))
}

/// Background task: fires due schedules and prunes old backups.
pub fn spawn(pool: SqlitePool, cfg: Arc<Config>, jobs: Arc<JobManager>, events: EventBus) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(error) = tick(&pool, &cfg, &jobs, &events).await {
                tracing::warn!(error = %error, "backup scheduler tick failed");
            }
        }
    });
}

async fn tick(
    pool: &SqlitePool,
    cfg: &Arc<Config>,
    jobs: &Arc<JobManager>,
    events: &EventBus,
) -> AppResult<()> {
    let schedules = list(pool).await?;
    let now = db::now_ms();
    for schedule in schedules {
        if !schedule.enabled {
            continue;
        }
        let due = if schedule.next_run_at <= 0 {
            compute_next_run(
                &schedule.frequency,
                &schedule.time_of_day,
                schedule.weekday,
                now,
            )
        } else {
            schedule.next_run_at
        };
        if due > now {
            if due != schedule.next_run_at {
                sqlx::query("UPDATE backup_schedules SET next_run_at = ? WHERE id = ?")
                    .bind(due)
                    .bind(schedule.id)
                    .execute(pool)
                    .await?;
            }
            continue;
        }
        // Time to run.
        let next = compute_next_run(
            &schedule.frequency,
            &schedule.time_of_day,
            schedule.weekday,
            now + 1_000,
        );
        sqlx::query("UPDATE backup_schedules SET last_run_at = ?, next_run_at = ? WHERE id = ?")
            .bind(now)
            .bind(next)
            .bind(schedule.id)
            .execute(pool)
            .await?;

        let cfg = cfg.clone();
        let db = pool.clone();
        let scopes = schedule.scopes.clone();
        let retention = schedule.retention_count;
        let events = events.clone();
        let note = format!("scheduled {} backup", schedule.frequency);
        let spec = JobSpec::new("backup.scheduled", move |ctx| async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, u64, String)>(64);
            let progress_ctx = ctx.clone();
            let progress_task = tokio::spawn(async move {
                while let Some((done, total, message)) = rx.recv().await {
                    progress_ctx.progress(done, total, &message).await;
                }
            });
            let tx = Arc::new(tx);
            let progress: ProgressFn = {
                let tx = tx.clone();
                Arc::new(move |done, total, message| {
                    let _ = tx.try_send((done, total, message));
                })
            };
            let result =
                backup::create(&cfg, Some(&db), &scopes, &note, None, Some(progress)).await?;
            drop(tx);
            let _ = progress_task.await;
            let removed = backup::prune_retention(&cfg, &db, retention).await?;
            events.publish(
                "backup.done",
                json!({"file": result.file_name, "removed": removed.len()}),
            );
            serde_json::to_value(result).map_err(|e| {
                crate::error::AppError::internal(format!("cannot serialise result: {e}"))
            })
        });
        jobs.submit(spec).await?;
        tracing::info!(schedule = schedule.id, "scheduled backup enqueued");
    }
    Ok(())
}

/// Create the default schedule row on first run (disabled by default).
pub async fn ensure_default(pool: &SqlitePool) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_schedules")
        .fetch_one(pool)
        .await?;
    if count == 0 {
        let next = compute_next_run("daily", "03:00", 0, db::now_ms());
        sqlx::query(
            "INSERT INTO backup_schedules (enabled, frequency, time_of_day, weekday, retention_count, scopes, next_run_at)
             VALUES (0, 'daily', '03:00', 0, 7, '[\"db\",\"config\"]', ?)",
        )
        .bind(next)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Retention helper used by the API when deleting a backup manually.
pub fn backup_path(cfg: &Config, file_name: &str) -> PathBuf {
    cfg.backup_dir().join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        assert_eq!(parse_time_of_day("03:30"), (3, 30));
        assert_eq!(parse_time_of_day("24:00"), (3, 0));
        assert_eq!(parse_time_of_day("bad"), (3, 0));
    }

    #[test]
    fn daily_next_run_is_in_the_future() {
        let now = db::now_ms();
        let next = compute_next_run("daily", "03:00", 0, now);
        assert!(next > now);
        assert!(next - now <= 24 * 3600 * 1000 + 1000);
    }

    #[test]
    fn hourly_next_run_within_an_hour() {
        let now = db::now_ms();
        let next = compute_next_run("hourly", "15", 0, now);
        assert!(next > now);
        assert!(next - now <= 3600 * 1000 + 1000);
    }

    #[test]
    fn weekly_next_run_within_a_week() {
        let now = db::now_ms();
        let next = compute_next_run("weekly", "23:59", 0, now);
        assert!(next > now);
        assert!(next - now <= 7 * 24 * 3600 * 1000 + 1000);
    }

    #[test]
    fn validation_rejects_bad_input() {
        assert!(validate("never", "03:00", 0, 7, &[]).is_err());
        assert!(validate("daily", "03:00", 9, 7, &[]).is_err());
        assert!(validate("daily", "03:00", 0, 0, &[]).is_err());
        assert!(validate("daily", "03:00", 0, 7, &["bogus".into()]).is_err());
        assert!(validate("daily", "03:00", 0, 7, &["db".into()]).is_ok());
    }

    #[tokio::test]
    async fn default_schedule_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("s.db"), 2).await.unwrap();
        ensure_default(&pool).await.unwrap();
        ensure_default(&pool).await.unwrap();
        let schedules = list(&pool).await.unwrap();
        assert_eq!(schedules.len(), 1);
        assert!(!schedules[0].enabled);
        assert_eq!(schedules[0].frequency, "daily");
    }
}
