//! Backup Center API.
//!
//! Archives contain the full account database (password and session hashes),
//! the server master key, configuration and optionally every user's files, so
//! downloading an archive, reading the restore guide (host paths) and every
//! mutation are administrator-only. A read-only `backup` group grant exposes
//! status metadata only: backup ids, timestamps, sizes, scopes and outcome,
//! plus the schedule summary — never archive bytes, file names or host paths.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use tokio_util::io::ReaderStream;

use crate::{
    access,
    audit::{self, AuditLog},
    auth::{AdminUser, AuthUser},
    backup::{self, schedule},
    db,
    error::{AppError, AppResult},
    jobs::JobSpec,
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/backups", get(list_backups).post(create_backup))
        .route(
            "/api/backups/schedule",
            get(get_schedule).put(update_schedule),
        )
        .route("/api/backups/restore-guide", get(restore_guide))
        .route("/api/backups/{id}", axum::routing::delete(delete_backup))
        .route("/api/backups/{id}/download", get(download_backup))
        .route("/api/backups/{id}/verify", post(verify_backup))
}

#[derive(Debug, Deserialize)]
pub struct BackupQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupDto {
    pub id: String,
    /// Archive file name (admins only; empty for read-only viewers).
    pub file_name: String,
    pub size_bytes: i64,
    /// Archive checksum (admins only; empty for read-only viewers).
    pub sha256: String,
    pub scope: Vec<String>,
    pub status: String,
    /// Free-form operator note (admins only; empty for read-only viewers).
    pub note: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Failure text (admins only; read-only viewers get a generic marker).
    pub error: String,
    /// Whether the caller may download the archive.
    pub downloadable: bool,
}

/// GET /api/backups
pub async fn list_backups(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<BackupQuery>,
) -> AppResult<Json<Value>> {
    // Read handler: admins or non-admins with the read-only `backup` grant.
    // Read-only viewers receive status metadata only (see module docs).
    access::app_surface(&state.db, &state, &user, "backup").await?;
    let is_admin = user.is_admin();
    let page = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(25).clamp(1, 100);
    let offset = (page - 1) * per_page;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups")
        .fetch_one(&state.db)
        .await?;
    let rows = sqlx::query(
        "SELECT id, file_name, size_bytes, sha256, scope, status, note, created_at, finished_at, error
         FROM backups ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(per_page)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<BackupDto> = rows
        .into_iter()
        .map(|row| {
            let error: String = row.get("error");
            BackupDto {
                id: row.get("id"),
                file_name: if is_admin {
                    row.get("file_name")
                } else {
                    String::new()
                },
                size_bytes: row.get("size_bytes"),
                sha256: if is_admin {
                    row.get("sha256")
                } else {
                    String::new()
                },
                scope: serde_json::from_str::<Vec<String>>(&row.get::<String, _>("scope"))
                    .unwrap_or_default(),
                status: row.get("status"),
                note: if is_admin {
                    row.get("note")
                } else {
                    String::new()
                },
                created_at: row.get("created_at"),
                finished_at: row.get("finished_at"),
                error: if is_admin || error.is_empty() {
                    error
                } else {
                    "backup failed".to_string()
                },
                downloadable: is_admin,
            }
        })
        .collect();
    let schedule = schedule::list(&state.db).await?;
    Ok(Json(json!({
        "items": items,
        "page": page,
        "perPage": per_page,
        "total": total,
        "scopes": backup::SCOPES.map(|scope| json!({
            "id": scope,
            "description": backup::scope_description(scope),
        })),
        "schedule": schedule.first(),
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBackupRequest {
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default)]
    pub note: Option<String>,
}

/// POST /api/backups — enqueue a backup job.
pub async fn create_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<CreateBackupRequest>,
) -> AppResult<Json<Value>> {
    let scopes = backup::normalize_scopes(body.scope.as_deref().unwrap_or(&[]))?;
    let note = body.note.unwrap_or_default().trim().to_string();
    if note.chars().count() > 200 {
        return Err(AppError::bad_request("note must be at most 200 characters"));
    }

    let cfg = state.config.clone();
    let db = state.db.clone();
    let retention = db::get_setting(&db, "backupRetentionCount")
        .await
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(7);
    let scopes_for_job = scopes.clone();
    let note_for_job = note.clone();
    let spec = JobSpec::new("backup", move |ctx| async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, u64, String)>(64);
        let progress_ctx = ctx.clone();
        let progress_task = tokio::spawn(async move {
            while let Some((done, total, message)) = rx.recv().await {
                progress_ctx.progress(done, total, &message).await;
            }
        });
        let tx = Arc::new(tx);
        let progress: backup::ProgressFn = {
            let tx = tx.clone();
            Arc::new(move |done, total, message| {
                let _ = tx.try_send((done, total, message));
            })
        };
        let result = backup::create(
            &cfg,
            Some(&db),
            &scopes_for_job,
            &note_for_job,
            None,
            Some(progress),
        )
        .await?;
        drop(tx);
        let _ = progress_task.await;
        backup::prune_retention(&cfg, &db, retention).await?;
        serde_json::to_value(result)
            .map_err(|e| AppError::internal(format!("cannot serialise result: {e}")))
    })
    .actor(&admin.0);

    let job_id = state.jobs.submit(spec).await?;
    state.events.publish_admin(
        "backup.changed",
        json!({"reason": "queued", "jobId": job_id}),
    );
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("backup.create")
            .actor(&admin.0)
            .object("job", job_id.clone())
            .success()
            .detail(format!("scopes={}", scopes.join(","))),
    )
    .await;
    Ok(Json(json!({"jobId": job_id, "scopes": scopes})))
}

/// GET /api/backups/schedule
pub async fn get_schedule(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    access::app_surface(&state.db, &state, &user, "backup").await?;
    let schedules = schedule::list(&state.db).await?;
    Ok(Json(json!({"schedule": schedules.first()})))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateScheduleRequest {
    pub enabled: bool,
    pub frequency: String,
    pub time_of_day: String,
    #[serde(default)]
    pub weekday: i64,
    pub retention_count: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// PUT /api/backups/schedule
pub async fn update_schedule(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<UpdateScheduleRequest>,
) -> AppResult<Json<Value>> {
    let (frequency, time_of_day, weekday, retention, scopes) = schedule::validate(
        &body.frequency,
        &body.time_of_day,
        body.weekday,
        body.retention_count,
        &body.scopes,
    )
    .map_err(AppError::bad_request)?;
    let next = schedule::compute_next_run(&frequency, &time_of_day, weekday, db::now_ms());
    let scopes_json = serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into());

    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM backup_schedules ORDER BY id LIMIT 1")
            .fetch_optional(&state.db)
            .await?;
    match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE backup_schedules SET enabled = ?, frequency = ?, time_of_day = ?, weekday = ?,
                        retention_count = ?, scopes = ?, next_run_at = ? WHERE id = ?",
            )
            .bind(if body.enabled { 1 } else { 0 })
            .bind(&frequency)
            .bind(&time_of_day)
            .bind(weekday)
            .bind(retention)
            .bind(&scopes_json)
            .bind(next)
            .bind(id)
            .execute(&state.db)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO backup_schedules (enabled, frequency, time_of_day, weekday, retention_count, scopes, next_run_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(if body.enabled { 1 } else { 0 })
            .bind(&frequency)
            .bind(&time_of_day)
            .bind(weekday)
            .bind(retention)
            .bind(&scopes_json)
            .bind(next)
            .execute(&state.db)
            .await?;
        }
    }
    db::set_setting(&state.db, "backupRetentionCount", &retention.to_string()).await?;
    state
        .events
        .publish_admin("backup.changed", json!({"reason": "schedule"}));
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("backup.schedule_update")
            .actor(&admin.0)
            .object("schedule", frequency.clone())
            .success()
            .detail(format!("enabled={}, retention={retention}", body.enabled)),
    )
    .await;
    get_schedule(State(state), admin.0).await
}

/// DELETE /api/backups/{id}
pub async fn delete_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT path, file_name FROM backups WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("backup not found"))?;
    let path: String = row.get("path");
    let path_ref = PathBuf::from(&path);
    // Only ever delete files inside the configured backup directory.
    if !path_ref.starts_with(state.config.backup_dir()) {
        return Err(AppError::Forbidden(
            "refusing to delete a file outside the backup directory",
        ));
    }
    if path_ref.exists() {
        std::fs::remove_file(&path_ref)?;
    }
    sqlx::query("DELETE FROM backups WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    state
        .events
        .publish_admin("backup.changed", json!({"reason": "deleted", "id": id}));
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("backup.delete")
            .actor(&admin.0)
            .object("backup", id)
            .path(path)
            .success(),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

/// GET /api/backups/{id}/download — streamed archive download.
pub async fn download_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    // Archives contain the account database, the master key and possibly
    // every user's files: operator-only, regardless of group grants.
    let row = sqlx::query("SELECT path, file_name FROM backups WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("backup not found"))?;
    let path: String = row.get("path");
    let file_name: String = row.get("file_name");
    let path_ref = PathBuf::from(&path);
    if !path_ref.starts_with(state.config.backup_dir()) || !path_ref.is_file() {
        return Err(AppError::NotFound("backup file not found"));
    }
    let file = tokio::fs::File::open(&path_ref).await?;
    let stream = ReaderStream::with_capacity(file, 256 * 1024);
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            safe_file_name(&file_name)
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("backup.download")
            .actor(&admin.0)
            .object("backup", id)
            .path(safe_file_name(&file_name))
            .success(),
    )
    .await;
    Ok(response)
}

fn safe_file_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

/// POST /api/backups/{id}/verify
pub async fn verify_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT path FROM backups WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("backup not found"))?;
    let path: String = row.get("path");
    let path_ref = PathBuf::from(&path);
    if !path_ref.starts_with(state.config.backup_dir()) {
        return Err(AppError::Forbidden("invalid backup path"));
    }
    let result = backup::verify(&path_ref).await?;
    let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("backup.verify")
            .actor(&admin.0)
            .object("backup", id)
            .path(path)
            .detail(if ok {
                "verified"
            } else {
                "verification failed"
            })
            .success(),
    )
    .await;
    Ok(Json(result))
}

/// GET /api/backups/restore-guide?backupId=...
pub async fn restore_guide(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(q): Query<RestoreGuideQuery>,
) -> AppResult<Json<Value>> {
    let file_name = if let Some(id) = &q.backup_id {
        sqlx::query_scalar::<_, String>("SELECT file_name FROM backups WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
    } else {
        None
    };
    let archive = file_name
        .map(|name| state.config.backup_dir().join(name))
        .unwrap_or_else(|| state.config.backup_dir().join("<backup-file>.tar.gz"));
    Ok(Json(json!({
        "archive": archive.display().to_string(),
        "databasePath": state.config.db_path().display().to_string(),
        "steps": [
            {"title": "Download the archive (optional)", "command": null,
             "detail": "Keep an off-server copy. Backups include the server secret key: store them securely."},
            {"title": "Stop RAST", "command": "sudo systemctl stop rast.service",
             "detail": "The database is replaced offline so no writes can race the restore."},
            {"title": "Check the archive", "command": format!("sudo -u rast rast restore --from {} --dry-run", archive.display()),
             "detail": "Verifies checksums, the manifest and SQLite integrity before touching anything."},
            {"title": "Restore the database", "command": format!("sudo -u rast rast restore --from {} --yes", archive.display()),
             "detail": "The previous database is preserved under /var/lib/rast/backups/pre-restore-*."},
            {"title": "Restart and verify", "command": "sudo systemctl start rast.service && rast status",
             "detail": "Sign in and check File Station, users and Activity Logs."},
            {"title": "Restore user files (optional)", "command": format!(
                "sudo tar -xzf {archive} -C {homes} --strip-components=2 users/homes && sudo tar -xzf {archive} -C {shared} --strip-components=2 users/shared && sudo chown -R rast:rast {homes} {shared}",
                archive = archive.display(),
                homes = state.config.homes_dir().display(),
                shared = state.config.shared_dir().display(),
             ),
             "detail": "Only for backups that include the users scope. Stop RAST first and try the command on an isolated copy before restoring over live data; existing files with the same names are overwritten."}
        ],
        "rollback": "Database migrations are forward-only: a RAST binary older than the database schema refuses to start. To roll back a release, stop the service, restore the database backup taken before the upgrade, then install the older binary. Binaries older than 0.2.0 do not enforce group policy (only legacy shared-folder memberships), so review access after such a rollback; writes made after the backup are lost.",
        "warning": "Deployed application data (container volumes) is NOT part of a RAST backup; back it up with the application's own procedure."
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreGuideQuery {
    pub backup_id: Option<String>,
}

/// Group-policy enforcement tests for the Backup Center read surface.
#[cfg(test)]
mod access_policy_tests {
    use crate::api::apps::access_test_support::TestServer;
    use axum::http::{header, StatusCode};
    use serde_json::json;

    const ADMIN_PASSWORD: &str = "Admin-Password-123!";
    const USER_PASSWORD: &str = "Alice-Password-123!";

    /// Seed one completed backup row + archive file inside the backup dir.
    async fn seed_backup(server: &TestServer) -> String {
        let backup_dir = server.state.config.backup_dir();
        std::fs::create_dir_all(&backup_dir).expect("backup dir");
        let path = backup_dir.join("b1.tar.gz");
        std::fs::write(&path, b"fake-archive-bytes").expect("write archive");
        sqlx::query(
            "INSERT INTO backups (id, file_name, path, size_bytes, sha256, scope, status, note, created_at, finished_at, error)
             VALUES ('b1', 'b1.tar.gz', ?, 18, '', '[\"db\"]', 'done', '', 1, NULL, '')",
        )
        .bind(path.display().to_string())
        .execute(&server.state.db)
        .await
        .expect("insert backup row");
        path.display().to_string()
    }

    #[tokio::test]
    async fn unauthenticated_backup_reads_are_401() {
        let server = TestServer::start().await;
        for path in [
            "/api/backups",
            "/api/backups/schedule",
            "/api/backups/restore-guide",
        ] {
            let (status, _, _) = server.get_anon(path).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn denied_non_admin_backup_reads_are_403() {
        let server = TestServer::start().await;
        server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        let alice = server.login("alice", USER_PASSWORD).await;
        for path in [
            "/api/backups",
            "/api/backups/schedule",
            "/api/backups/restore-guide",
            "/api/backups/missing/download",
        ] {
            let (status, _, body) = server.get(path, &alice).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} -> {body}");
            assert_eq!(body["error"]["code"], "forbidden", "{path} -> {body}");
        }
    }

    #[tokio::test]
    async fn read_only_grant_sees_status_metadata_but_no_archive_or_paths() {
        // Regression for audit finding F1: archives contain the account
        // database, the master key and possibly every user's files, so a
        // read-only Backup Center grant must never expose archive bytes,
        // file names, checksums, operator notes or host paths.
        let server = TestServer::start().await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "backup", "allow").await;
        seed_backup(&server).await;
        let session = server.login("alice", USER_PASSWORD).await;

        let (status, _, body) = server.get("/api/backups", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
        let item = &body["items"][0];
        assert_eq!(item["id"], "b1");
        assert_eq!(item["fileName"], "", "file name hidden: {item}");
        assert_eq!(item["sha256"], "", "checksum hidden: {item}");
        assert_eq!(item["note"], "", "operator note hidden: {item}");
        assert_eq!(item["downloadable"], false, "{item}");
        assert!(item["status"].is_string() && item["sizeBytes"].is_number());

        let (status, _, body) = server.get("/api/backups/schedule", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("schedule").is_some(), "{body}");

        let (status, _, body) = server.get("/api/backups/restore-guide", &session).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "restore guide exposes host paths: {body}"
        );

        let (status, _, body) = server.get("/api/backups/b1/download", &session).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "read-only grant must not download archives: {body}"
        );
    }

    #[tokio::test]
    async fn admin_downloads_archive_and_gets_safe_restore_commands() {
        let server = TestServer::start().await;
        server
            .create_user("root", ADMIN_PASSWORD, crate::auth::Role::Admin)
            .await;
        seed_backup(&server).await;
        let session = server.login("root", ADMIN_PASSWORD).await;

        let (status, _, body) = server.get("/api/backups", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["items"][0]["fileName"], "b1.tar.gz");
        assert_eq!(body["items"][0]["downloadable"], true);

        let (status, headers, _) = server.get("/api/backups/b1/download", &session).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
            "application/gzip"
        );

        // Regression for audit finding F4: user files must be restored into the
        // storage roots, never extracted into `/`.
        let (status, _, guide) = server.get("/api/backups/restore-guide", &session).await;
        assert_eq!(status, StatusCode::OK, "{guide}");
        let commands: Vec<String> = guide["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|step| step["command"].as_str().map(str::to_string))
            .collect();
        let files = commands
            .iter()
            .find(|command| command.contains("users/homes"))
            .expect("user file restore command");
        assert!(!files.contains("-C / "), "must not extract into /: {files}");
        assert!(files.contains("users/shared"), "{files}");
        let homes = server.state.config.homes_dir().display().to_string();
        assert!(files.contains(&homes), "targets storage homes: {files}");
        assert!(guide["rollback"]
            .as_str()
            .is_some_and(|text| text.contains("refuses to start")));

        let audited: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = 'backup.download'")
                .fetch_one(&server.state.db)
                .await
                .unwrap();
        assert_eq!(audited, 1, "archive downloads are audited");
    }

    #[tokio::test]
    async fn read_only_grant_still_forbids_backup_mutations() {
        let server = TestServer::start().await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "backup", "allow").await;
        seed_backup(&server).await;
        let session = server.login("alice", USER_PASSWORD).await;

        let cases: [(&str, &str, Option<serde_json::Value>); 4] = [
            (
                "POST",
                "/api/backups",
                Some(json!({"scope": ["db"], "note": "x"})),
            ),
            ("DELETE", "/api/backups/b1", None),
            ("POST", "/api/backups/b1/verify", None),
            (
                "PUT",
                "/api/backups/schedule",
                Some(json!({
                    "enabled": true,
                    "frequency": "daily",
                    "timeOfDay": "03:00",
                    "retentionCount": 7,
                    "scopes": ["db"],
                })),
            ),
        ];
        for (method, path, body) in cases {
            let (status, _, value) = server
                .request(method, path, body, session.headers_with_csrf())
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} -> {value}");
        }
    }

    #[tokio::test]
    async fn explicit_none_blocks_backup_grant() {
        let server = TestServer::start().await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "backup", "allow").await;
        server.set_user_app_rule(alice, "backup", "none").await;
        let session = server.login("alice", USER_PASSWORD).await;
        let (status, _, body) = server.get("/api/backups", &session).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn admin_behaviour_is_unchanged() {
        let server = TestServer::start().await;
        server
            .create_user("admin", ADMIN_PASSWORD, crate::auth::Role::Admin)
            .await;
        seed_backup(&server).await;
        let admin = server.login("admin", ADMIN_PASSWORD).await;

        let (status, _, body) = server.get("/api/backups", &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
        let (status, _, body) = server.get("/api/backups/b1/download", &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Administrators keep the schedule write path.
        let (status, _, body) = server
            .request(
                "PUT",
                "/api/backups/schedule",
                Some(json!({
                    "enabled": true,
                    "frequency": "daily",
                    "timeOfDay": "03:30",
                    "retentionCount": 5,
                    "scopes": ["db", "config"],
                })),
                admin.headers_with_csrf(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["schedule"]["frequency"], "daily", "{body}");
    }
}
