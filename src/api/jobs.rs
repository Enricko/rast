//! Background job endpoints (status, progress, cancellation).

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    audit::{self, AuditLog},
    auth::AuthUser,
    error::{AppError, AppResult},
    jobs::job_json,
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
}

#[derive(Debug, Deserialize)]
pub struct JobQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
    pub status: Option<String>,
}

/// GET /api/jobs
pub async fn list_jobs(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<JobQuery>,
) -> AppResult<Json<Value>> {
    let page = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(25).clamp(1, 100);
    let offset = (page - 1) * per_page;
    let status_filter = q.status.as_deref().unwrap_or("");
    let (total, rows) = if user.is_admin() {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE (? = '' OR status = ?)",
        )
        .bind(status_filter)
        .bind(status_filter)
        .fetch_one(&state.db)
        .await?;
        let rows = sqlx::query(
            "SELECT id, kind, status, progress, total, message, user_id, username, created_at,
                    started_at, finished_at, error
             FROM jobs WHERE (? = '' OR status = ?)
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(status_filter)
        .bind(status_filter)
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (total, rows)
    } else {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE user_id = ? AND (? = '' OR status = ?)",
        )
        .bind(user.id)
        .bind(status_filter)
        .bind(status_filter)
        .fetch_one(&state.db)
        .await?;
        let rows = sqlx::query(
            "SELECT id, kind, status, progress, total, message, user_id, username, created_at,
                    started_at, finished_at, error
             FROM jobs WHERE user_id = ? AND (? = '' OR status = ?)
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(user.id)
        .bind(status_filter)
        .bind(status_filter)
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (total, rows)
    };
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
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
                "error": r.get::<Option<String>, _>("error"),
            })
        })
        .collect();
    Ok(Json(json!({"items": items, "page": page, "perPage": per_page, "total": total})))
}

/// GET /api/jobs/{id}
pub async fn get_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let job = job_json(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("job not found"))?;
    let owner = job.get("userId").and_then(|v| v.as_i64());
    if !user.is_admin() && owner != Some(user.id) {
        return Err(AppError::NotFound("job not found"));
    }
    Ok(Json(job))
}

/// POST /api/jobs/{id}/cancel
pub async fn cancel_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let job = job_json(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("job not found"))?;
    let owner = job.get("userId").and_then(|v| v.as_i64());
    if !user.is_admin() && owner != Some(user.id) {
        return Err(AppError::NotFound("job not found"));
    }
    let cancelled = state.jobs.cancel(&id);
    audit::record(
        &state.db,
        AuditLog::new("job.cancel")
            .actor(&user)
            .object("job", id.clone())
            .success()
            .detail(if cancelled { "cancel requested" } else { "not running" }),
    )
    .await;
    Ok(Json(json!({"ok": true, "cancelRequested": cancelled})))
}
