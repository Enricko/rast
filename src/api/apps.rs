//! Applications / Deployment API and Docker administration (admin only).
//!
//! Managed applications are Compose projects stored in the `apps` table with
//! their encrypted environment in `env_json`. The web server renders the
//! Compose file and `.env` into `<data_dir>/apps/<slug>/` and asks the
//! privileged worker to validate and run them; the Docker socket is never
//! opened from this process.
//!
//! Security rules enforced here:
//! * admin role required for every endpoint in this module;
//! * environment values marked `secret` are encrypted with [`Secrets`] at rest
//!   and masked (`••••`) in every response; they are never logged;
//! * Compose text is structurally checked here and authoritatively validated
//!   by the worker (`docker compose config` + sandbox policy);
//! * slugs are generated server-side, files are written atomically with mode
//!   `0600`, and the app directory is created `0700`;
//! * a `DELETE` keeps volumes by default; volume removal requires an explicit
//!   `removeVolumes=true` and never uses `docker compose down -v`;
//! * unmanaged containers are only ever listed, never adopted implicitly. The
//!   import endpoint is the sole, explicit adoption path.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    convert::Infallible,
    hash::{DefaultHasher, Hash, Hasher},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    audit::{self, AuditLog},
    auth::{AdminUser, AuthUser},
    db,
    docker::{
        compose,
        containers::derive_status,
        protocol::{validate_slug, WorkerOp, WorkerRequest},
        DockerError,
    },
    error::{AppError, AppResult},
    jobs::{JobContext, JobSpec},
    secrets::Secrets,
    state::AppState,
};

/// Length cap for an application display name.
const MAX_APP_NAME_LEN: usize = 64;
/// Maximum environment entries per application.
const MAX_ENV_ENTRIES: usize = 128;
/// Maximum environment key length.
const MAX_ENV_KEY_LEN: usize = 128;
/// Maximum environment value length (characters).
const MAX_ENV_VALUE_LEN: usize = 8192;
/// Placeholder returned instead of secret values.
pub const MASKED_ENV_VALUE: &str = "••••";
/// Maximum Compose text stored per application (same bound as the worker).
const MAX_COMPOSE_BYTES: usize = compose::MAX_COMPOSE_BYTES;
/// Maximum number of containers accepted by one import request.
const MAX_IMPORT_CONTAINERS: usize = 16;

// --- Live application log stream (admin SSE, per-connection) -----------------
/// How often the stream polls the worker for new lines.
const LOG_STREAM_INTERVAL: Duration = Duration::from_secs(2);
/// Per-poll worker deadline: a slow daemon must not stall the SSE stream.
const LOG_STREAM_POLL_TIMEOUT: Duration = Duration::from_secs(8);
/// Bounded server-side ring buffer per connection (contract: last ~500 lines).
const LOG_STREAM_RING_CAPACITY: usize = 500;
/// Dedupe window size for incremental `--since` reads (inclusive cursor).
const LOG_STREAM_SEEN_CAPACITY: usize = 1000;
/// SSE keep-alive interval (contract: 15 s).
const LOG_STREAM_KEEPALIVE_SECS: u64 = 15;
/// Default and maximum `?tail=` values for the initial seed.
const LOG_STREAM_DEFAULT_TAIL: usize = 200;
const LOG_STREAM_MAX_TAIL: usize = 500;
/// Consecutive failed polls before the stream closes (client reconnects and
/// gets a fresh 503/404 instead of a silently dead stream).
const LOG_STREAM_MAX_FAILURES: u32 = 15;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/docker/status", get(docker_status))
        .route("/api/docker/containers", get(docker_containers))
        .route("/api/docker/import", post(docker_import))
        .route("/api/apps", get(list_apps).post(create_app))
        .route("/api/apps/{id}", get(get_app).delete(delete_app))
        .route("/api/apps/{id}/start", post(start_app))
        .route("/api/apps/{id}/stop", post(stop_app))
        .route("/api/apps/{id}/restart", post(restart_app))
        .route("/api/apps/{id}/redeploy", post(redeploy_app))
        .route("/api/apps/{id}/logs", get(app_logs))
        .route("/api/apps/{id}/logs/stream", get(app_logs_stream))
        .route("/api/apps/{id}/history", get(app_history))
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvInput {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    pub name: String,
    pub compose: String,
    #[serde(default)]
    pub env: Vec<EnvInput>,
}

#[derive(Debug, Deserialize)]
pub struct RedeployRequest {
    pub compose: Option<String>,
    pub env: Option<Vec<EnvInput>>,
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub tail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    #[serde(rename = "removeVolumes")]
    pub remove_volumes: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    #[serde(rename = "containerIds")]
    pub container_ids: Vec<String>,
}

/// Environment variable as persisted: `value` is plaintext for non-secrets and
/// AES-GCM ciphertext (base64) for secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEnv {
    key: String,
    secret: bool,
    value: String,
}

/// One application row plus its decoded environment.
struct AppRecord {
    id: String,
    name: String,
    slug: String,
    compose: String,
    env: Vec<StoredEnv>,
    status: String,
    last_error: String,
    created_at: i64,
    updated_at: i64,
}

impl AppRecord {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        let env_json: String = row.get("env_json");
        Self {
            id: row.get("id"),
            name: row.get("name"),
            slug: row.get("slug"),
            compose: row.get("compose"),
            env: parse_stored_env(&env_json),
            status: row.get("status"),
            last_error: row.get("last_error"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}

// ---------------------------------------------------------------------------
// Docker status / containers
// ---------------------------------------------------------------------------

/// GET /api/docker/status — never fails: reports why Docker is unavailable.
///
/// Read handler: admins and non-admins with a read-only `applications` grant.
pub async fn docker_status(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Value>> {
    let access = crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    let status = state.docker.status().await;
    Ok(Json(if access.read_only {
        docker_status_for_read_only(status)
    } else {
        status
    }))
}

/// GET /api/docker/containers — discovered containers flagged `managed`.
///
/// Returns 503 when the Docker subsystem is unavailable (the status endpoint
/// explains why, so the UI can show a helpful message).
///
/// Intentionally **admin-only**: this lists every container on the host (not
/// just managed applications) and exists to drive the explicit import flow,
/// which is an administrator action. Read-only `applications` grants get
/// `/api/docker/status` instead, which is enough to render the availability
/// banner. The group-policy middleware also blocks this only at the app level,
/// so the `AdminUser` extractor is the defence in depth for this handler.
pub async fn docker_containers(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let data = state
        .docker
        .all_containers()
        .await
        .map_err(AppError::from)?;
    let containers = data
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let managed_slugs: BTreeSet<String> = sqlx::query_scalar::<_, String>("SELECT slug FROM apps")
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect();
    let managed_projects: BTreeSet<String> = managed_slugs
        .iter()
        .map(|slug| format!("rast-{slug}"))
        .collect();

    let mut items: Vec<Value> = Vec::with_capacity(containers.len());
    for container in containers {
        let project = container
            .get("project")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let slug = project
            .strip_prefix("rast-")
            .filter(|_| managed_projects.contains(&project))
            .map(str::to_string)
            .or_else(|| {
                if managed_slugs.contains(&project) {
                    Some(project.clone())
                } else {
                    None
                }
            });
        let managed = slug
            .as_deref()
            .map(|slug| managed_slugs.contains(slug))
            .unwrap_or(false);
        let mut item = container;
        if let Some(object) = item.as_object_mut() {
            object.insert("managed".to_string(), json!(managed));
            object.insert(
                "slug".to_string(),
                slug.map(Value::String).unwrap_or(Value::Null),
            );
        }
        items.push(item);
    }
    let total = items.len() as i64;
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 200);
    let offset = ((page - 1) * per_page).max(0) as usize;
    items = items
        .into_iter()
        .skip(offset)
        .take(per_page as usize)
        .collect();
    Ok(Json(json!({
        "items": items,
        "page": page,
        "perPage": per_page,
        "total": total,
        "docker": state.docker.status().await,
    })))
}

// ---------------------------------------------------------------------------
// Applications
// ---------------------------------------------------------------------------

/// GET /api/apps — managed apps with a freshly probed status.
pub async fn list_apps(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    // Defence in depth: the policy middleware also gates /api/apps, but the
    // handler must never rely on the route layer alone.
    let access = crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 200);
    let offset = (page - 1) * per_page;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps")
        .fetch_one(&state.db)
        .await?;
    let rows = sqlx::query(
        "SELECT id, name, slug, compose, env_json, status, last_error, created_at, updated_at
         FROM apps ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(per_page)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    // One worker call refreshes every status (grouped by Compose project).
    let (containers_by_project, docker_ok, docker_reason) = probe_all_containers(&state).await;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let app = AppRecord::from_row(row);
        let project = format!("rast-{}", app.slug);
        let grouped = containers_by_project.get(&project);
        let refreshed = grouped
            .map(|containers| derive_status(containers, &app.status))
            .unwrap_or_else(|| app.status.clone());
        let containers = grouped.cloned().unwrap_or_default();
        if access.read_only {
            items.push(app_json_restricted(&app, &containers, &refreshed));
        } else {
            items.push(app_json(
                &app,
                &state.secrets,
                false,
                &containers,
                &refreshed,
            ));
        }
    }
    let docker = if access.read_only {
        json!({
            "available": docker_ok,
            "reason": if docker_ok {
                Value::Null
            } else {
                json!("Docker integration is unavailable")
            },
        })
    } else {
        json!({"available": docker_ok, "reason": docker_reason})
    };
    Ok(Json(json!({
        "items": items,
        "page": page,
        "perPage": per_page,
        "total": total,
        "docker": docker,
    })))
}

/// GET /api/apps/{id}
pub async fn get_app(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let access = crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    let app = load_app(&state, &id).await?;
    let (containers, docker_reason) = probe_app_containers(&state, &app.slug).await;
    let status = derive_status(&containers, &app.status);
    // The Compose document may contain inline values an administrator typed
    // (e.g. a literal API key); F2 also hides environment values and deployment
    // error detail from read-only viewers.
    let mut detail = if access.read_only {
        app_json_restricted(&app, &containers, &status)
    } else {
        app_json(&app, &state.secrets, true, &containers, &status)
    };
    if let Some(object) = detail.as_object_mut() {
        object.insert("containers".to_string(), Value::Array(containers.clone()));
        object.insert(
            "ports".to_string(),
            Value::Array(collect_ports(&containers)),
        );
        object.insert(
            "volumes".to_string(),
            Value::Array(collect_volumes(&containers)),
        );
        let (resources, resources_reason) = probe_app_resources(&state, &containers).await;
        object.insert("resources".to_string(), Value::Array(resources));
        let available = docker_reason.is_none();
        let reason = resources_reason.or(docker_reason);
        object.insert(
            "docker".to_string(),
            if access.read_only {
                json!({
                    "available": available,
                    "reason": if available { Value::Null } else { json!("Docker integration is unavailable") },
                })
            } else {
                json!({"available": available, "reason": reason})
            },
        );
    }
    Ok(Json(detail))
}

/// POST /api/apps — validate, persist and deploy a new managed application.
pub async fn create_app(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(request): Json<CreateAppRequest>,
) -> AppResult<Json<Value>> {
    let name = validate_app_name(&request.name)?;
    let stored_env = build_stored_env(&state.secrets, &request.env)?;
    let compose_text = request.compose.trim().to_string();
    if compose_text.len() > MAX_COMPOSE_BYTES {
        return Err(AppError::bad_request(format!(
            "compose file is too large (max {MAX_COMPOSE_BYTES} bytes)"
        )));
    }

    // Cheap structural validation first (400 even when Docker is down).
    compose::check_compose_text(&compose_text, &slugify(&name)).map_err(AppError::bad_request)?;

    // Make sure the Docker worker is usable before creating any state: without
    // it the application could never be validated or deployed.
    state.docker.ensure_available().await?;

    let slug = unique_slug(&state, &name).await?;
    let env_file = render_env_file(&state.secrets, &stored_env)?;

    // Stage the files so the worker can validate the exact configuration that
    // will later be deployed.
    write_app_files(&state, &slug, &compose_text, &env_file)?;
    if let Err(error) = validate_compose_via_worker(&state, &slug).await {
        remove_app_dir(&state, &slug, false);
        return Err(error);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = db::now_ms();
    let env_json = serde_json::to_string(&stored_env)
        .map_err(|e| AppError::internal(format!("cannot encode environment: {e}")))?;
    let insert = sqlx::query(
        "INSERT INTO apps (id, name, slug, compose, env_json, status, last_error, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'deploying', '', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&slug)
    .bind(&compose_text)
    .bind(&env_json)
    .bind(admin.0.id)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(error) = insert {
        remove_app_dir(&state, &slug, true);
        return Err(error.into());
    }

    let job_id = match enqueue_app_job(&state, &id, &slug, AppAction::Deploy, &admin.0).await {
        Ok(job_id) => job_id,
        Err(error) => {
            mark_app_error(&state, &id, &error.public_message()).await;
            return Err(error);
        }
    };
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.create")
            .actor(&admin.0)
            .object("app", &id)
            .path(&slug)
            .success()
            .detail(format!("job {job_id}")),
    )
    .await;
    let app = load_app(&state, &id).await?;
    Ok(Json(json!({
        "app": app_json(&app, &state.secrets, true, &[], &app.status),
        "jobId": job_id,
    })))
}

/// POST /api/apps/{id}/start
pub async fn start_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let app = load_app(&state, &id).await?;
    let job_id = enqueue_for_existing(&state, &app, AppAction::Start, &admin.0).await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.start")
            .actor(&admin.0)
            .object("app", &app.id)
            .path(&app.slug)
            .success()
            .detail(format!("job {job_id}")),
    )
    .await;
    Ok(Json(json!({"jobId": job_id, "status": "deploying"})))
}

/// POST /api/apps/{id}/stop
pub async fn stop_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let app = load_app(&state, &id).await?;
    let job_id = enqueue_for_existing(&state, &app, AppAction::Stop, &admin.0).await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.stop")
            .actor(&admin.0)
            .object("app", &app.id)
            .path(&app.slug)
            .success()
            .detail(format!("job {job_id}")),
    )
    .await;
    Ok(Json(json!({"jobId": job_id, "status": "stopping"})))
}

/// POST /api/apps/{id}/restart
pub async fn restart_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let app = load_app(&state, &id).await?;
    let job_id = enqueue_for_existing(&state, &app, AppAction::Restart, &admin.0).await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.restart")
            .actor(&admin.0)
            .object("app", &app.id)
            .path(&app.slug)
            .success()
            .detail(format!("job {job_id}")),
    )
    .await;
    Ok(Json(json!({"jobId": job_id, "status": "restarting"})))
}

/// POST /api/apps/{id}/redeploy — optionally replace the Compose text and/or
/// environment, then redeploy.
pub async fn redeploy_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RedeployRequest>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let app = load_app(&state, &id).await?;

    let compose_text = match request.compose {
        Some(text) => {
            let text = text.trim().to_string();
            if text.len() > MAX_COMPOSE_BYTES {
                return Err(AppError::bad_request(format!(
                    "compose file is too large (max {MAX_COMPOSE_BYTES} bytes)"
                )));
            }
            compose::check_compose_text(&text, &app.slug).map_err(AppError::bad_request)?;
            text
        }
        None => app.compose.clone(),
    };
    let stored_env = match request.env {
        Some(entries) => merge_redeploy_env(&state.secrets, &app.env, &entries)?,
        None => app.env.clone(),
    };
    let env_file = render_env_file(&state.secrets, &stored_env)?;

    // Stage and validate; restore the previous files on validation failure so a
    // failed redeploy leaves the running application untouched.
    let previous_compose = app.compose.clone();
    let previous_env = render_env_file(&state.secrets, &app.env).unwrap_or_default();
    write_app_files(&state, &app.slug, &compose_text, &env_file)?;
    if let Err(error) = validate_compose_via_worker(&state, &app.slug).await {
        let _ = write_app_files(&state, &app.slug, &previous_compose, &previous_env);
        return Err(error);
    }

    let env_json = serde_json::to_string(&stored_env)
        .map_err(|e| AppError::internal(format!("cannot encode environment: {e}")))?;
    let now = db::now_ms();
    sqlx::query(
        "UPDATE apps SET compose = ?, env_json = ?, status = 'deploying', last_error = '', updated_at = ? WHERE id = ?",
    )
    .bind(&compose_text)
    .bind(&env_json)
    .bind(now)
    .bind(&app.id)
    .execute(&state.db)
    .await?;

    let job_id = enqueue_app_job(&state, &app.id, &app.slug, AppAction::Redeploy, &admin.0).await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.redeploy")
            .actor(&admin.0)
            .object("app", &app.id)
            .path(&app.slug)
            .success()
            .detail(format!("job {job_id}")),
    )
    .await;
    Ok(Json(json!({"jobId": job_id, "status": "deploying"})))
}

/// GET /api/apps/{id}/logs?tail=200
pub async fn app_logs(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> AppResult<Json<Value>> {
    crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    // F2: container logs commonly contain credentials/tokens; only
    // administrators may read them (read-only grants get a clear 403).
    if !user.is_admin() {
        return Err(AppError::Forbidden(
            "container logs require an administrator",
        ));
    }
    state.docker.ensure_available().await?;
    let app = load_app(&state, &id).await?;
    let tail = crate::api::parse_bounded(query.tail.as_deref(), 200, 2000) as u32;
    let data = state
        .docker
        .call(
            WorkerRequest::new(WorkerOp::Logs)
                .slug(&app.slug)
                .tail(tail),
        )
        .await
        .map_err(AppError::from)?;
    Ok(Json(json!({
        "appId": app.id,
        "slug": app.slug,
        "tail": data.get("tail").cloned().unwrap_or(json!(tail)),
        "lines": data.get("lines").cloned().unwrap_or(json!([])),
    })))
}

/// Bounded log-line ring buffer with deduplication for inclusive `--since`
/// polls. Kept per SSE connection; memory is constant regardless of log volume.
#[derive(Debug)]
pub struct LogRing {
    capacity: usize,
    lines: VecDeque<Value>,
    seen: VecDeque<u64>,
    last_ts: Option<i64>,
}

impl LogRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            lines: VecDeque::with_capacity(capacity.clamp(1, 512)),
            seen: VecDeque::with_capacity(LOG_STREAM_SEEN_CAPACITY),
            last_ts: None,
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Newest line timestamp in the buffer (epoch ms).
    pub fn last_ts(&self) -> Option<i64> {
        self.last_ts
    }

    /// Inclusive `--since` cursor in Unix seconds (worker converts it to an
    /// RFC3339 argument; duplicate seconds are removed by deduplication).
    pub fn since_secs(&self) -> Option<i64> {
        self.last_ts.map(|ts| ts / 1000)
    }

    /// All buffered lines, oldest first.
    pub fn snapshot(&self) -> Vec<Value> {
        self.lines.iter().cloned().collect()
    }

    /// Replace the buffer with a fresh tail (initial connect or rotation).
    pub fn seed(&mut self, lines: Vec<Value>) -> Vec<Value> {
        self.lines.clear();
        self.seen.clear();
        self.last_ts = None;
        for line in lines {
            self.push(&line);
        }
        self.snapshot()
    }

    /// Append new lines, skipping duplicates from the inclusive `--since`
    /// cursor. Returns the lines that were actually added.
    pub fn append(&mut self, lines: Vec<Value>) -> Vec<Value> {
        let mut added = Vec::new();
        for line in lines {
            if self.push(&line) {
                added.push(line);
            }
        }
        added
    }

    fn push(&mut self, line: &Value) -> bool {
        let key = log_line_key(line);
        if self.seen.contains(&key) {
            return false;
        }
        while self.seen.len() >= LOG_STREAM_SEEN_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back(key);
        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
        let ts = line.get("ts").and_then(Value::as_i64).unwrap_or(0);
        if ts > 0 {
            self.last_ts = Some(self.last_ts.map_or(ts, |current: i64| current.max(ts)));
        }
        true
    }
}

/// Stable, bounded-size identity for a log line: timestamp + service + message.
fn log_line_key(line: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    line.get("ts")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .hash(&mut hasher);
    line.get("service")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .hash(&mut hasher);
    line.get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// One SSE payload produced by [`LogStreamState::poll`]. Kept as a small typed
/// value (instead of an opaque `Event`) so the streaming logic is unit-testable
/// without parsing SSE frames.
#[derive(Debug, Clone, PartialEq)]
struct LogStreamItem {
    app_id: String,
    lines: Vec<Value>,
    reset: bool,
}

impl LogStreamItem {
    fn to_event(&self) -> Event {
        let mut data = json!({"appId": self.app_id, "lines": self.lines});
        if self.reset {
            if let Some(object) = data.as_object_mut() {
                object.insert("reset".to_string(), json!(true));
            }
        }
        Event::default().event("app.logs").data(data.to_string())
    }
}

/// Identity captured when the log stream connects. F7: the session and the
/// administrator role are revalidated on every poll so a revoked session or a
/// demoted/disabled account stops receiving container logs immediately.
struct LogStreamIdentity {
    user_id: i64,
    session_id: String,
    token: String,
    ttl_hours: i64,
}

enum IdentityCheck {
    Valid,
    Revoked,
    Unknown,
}

/// Re-check session validity and the current account role/active flag.
async fn check_stream_identity(
    db: &sqlx::SqlitePool,
    identity: &LogStreamIdentity,
) -> IdentityCheck {
    match crate::auth::session::validate(db, &identity.token, identity.ttl_hours).await {
        Ok(Some(record)) => {
            if record.session_id != identity.session_id {
                return IdentityCheck::Revoked;
            }
        }
        Ok(None) => return IdentityCheck::Revoked,
        Err(error) => {
            tracing::warn!(error = %error, "log stream session revalidation failed");
            return IdentityCheck::Unknown;
        }
    }
    match sqlx::query_as::<_, (crate::auth::Role, bool)>(
        "SELECT role, active FROM users WHERE id = ?",
    )
    .bind(identity.user_id)
    .fetch_optional(db)
    .await
    {
        Ok(Some((role, active))) => {
            if active && role == crate::auth::Role::Admin {
                IdentityCheck::Valid
            } else {
                IdentityCheck::Revoked
            }
        }
        Ok(None) => IdentityCheck::Revoked,
        Err(error) => {
            tracing::warn!(error = %error, "log stream user revalidation failed");
            IdentityCheck::Unknown
        }
    }
}

/// Mutable state of one live-log SSE connection.
struct LogStreamState {
    db: sqlx::SqlitePool,
    docker: Arc<crate::docker::DockerManager>,
    identity: LogStreamIdentity,
    app_id: String,
    slug: String,
    tail: usize,
    ring: LogRing,
    seeded: bool,
    container_ids: Vec<String>,
    failures: u32,
    interval: tokio::time::Interval,
}

impl LogStreamState {
    fn new(
        state: &AppState,
        identity: LogStreamIdentity,
        app_id: String,
        slug: String,
        tail: usize,
    ) -> Self {
        // First tick after one full interval: the initial seed is fetched
        // without waiting, later polls are spaced out.
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + LOG_STREAM_INTERVAL,
            LOG_STREAM_INTERVAL,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            db: state.db.clone(),
            docker: state.docker.clone(),
            identity,
            app_id,
            slug,
            tail,
            ring: LogRing::new(LOG_STREAM_RING_CAPACITY),
            seeded: false,
            container_ids: Vec::new(),
            failures: 0,
            interval,
        }
    }

    fn poll_request(&self) -> WorkerRequest {
        let request = WorkerRequest::new(WorkerOp::ComposeLogsSince).slug(&self.slug);
        if self.seeded {
            let request = request.tail(LOG_STREAM_RING_CAPACITY as u32);
            match self.ring.since_secs() {
                Some(since) => request.since_secs(since),
                None => request,
            }
        } else {
            request.tail(self.tail as u32)
        }
    }

    /// Fetch one batch from the worker and return the SSE payload to send, if
    /// any.
    async fn poll(&mut self) -> Option<LogStreamItem> {
        let response = self
            .docker
            .call_with_timeout(self.poll_request(), LOG_STREAM_POLL_TIMEOUT)
            .await;
        let data = match response {
            Ok(data) => {
                self.failures = 0;
                data
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                tracing::debug!(
                    error = %error,
                    app = %self.app_id,
                    "application log stream poll failed"
                );
                return None;
            }
        };
        let lines = data
            .get("lines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let container_ids: Vec<String> = data
            .get("containerIds")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if !self.seeded {
            let seeded = self.ring.seed(lines);
            self.container_ids = container_ids;
            self.seeded = true;
            return Some(LogStreamItem {
                app_id: self.app_id.clone(),
                lines: seeded,
                reset: true,
            });
        }

        // Container recreation / rotation: the incremental cursor belongs to
        // the old containers, so reset the client from a fresh tail.
        if self.container_ids != container_ids {
            let reseed_request = WorkerRequest::new(WorkerOp::ComposeLogsSince)
                .slug(&self.slug)
                .tail(self.tail as u32);
            let reseed_lines = match self
                .docker
                .call_with_timeout(reseed_request, LOG_STREAM_POLL_TIMEOUT)
                .await
            {
                Ok(data) => data
                    .get("lines")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or(lines),
                Err(_) => lines,
            };
            let seeded = self.ring.seed(reseed_lines);
            self.container_ids = container_ids;
            return Some(LogStreamItem {
                app_id: self.app_id.clone(),
                lines: seeded,
                reset: true,
            });
        }

        self.container_ids = container_ids;
        let fresh = self.ring.append(lines);
        if fresh.is_empty() {
            return None;
        }
        Some(LogStreamItem {
            app_id: self.app_id.clone(),
            lines: fresh,
            reset: false,
        })
    }
}

/// Build the per-connection SSE stream for one managed application.
fn log_stream(
    state: AppState,
    identity: LogStreamIdentity,
    app_id: String,
    slug: String,
    tail: usize,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    let initial = LogStreamState::new(&state, identity, app_id, slug, tail);
    futures::stream::unfold(initial, |mut stream_state| async move {
        loop {
            // End the stream when the application is deleted; the admin gets a
            // final `app.changed` event from the reconciler.
            let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM apps WHERE id = ?")
                .bind(&stream_state.app_id)
                .fetch_optional(&stream_state.db)
                .await;
            match exists {
                Ok(None) => return None,
                Ok(Some(_)) => {}
                Err(error) => {
                    stream_state.failures = stream_state.failures.saturating_add(1);
                    tracing::warn!(error = %error, "log stream app lookup failed");
                    if stream_state.failures >= LOG_STREAM_MAX_FAILURES {
                        return None;
                    }
                }
            }
            if stream_state.seeded {
                stream_state.interval.tick().await;
                // F7: revalidate the session and the active-administrator
                // status immediately before every incremental poll (after the
                // wait, so a revocation during the interval is never followed
                // by another batch). The initial tail was authorized at connect.
                match check_stream_identity(&stream_state.db, &stream_state.identity).await {
                    IdentityCheck::Valid => {}
                    IdentityCheck::Revoked => return None,
                    IdentityCheck::Unknown => {
                        stream_state.failures = stream_state.failures.saturating_add(1);
                        if stream_state.failures >= LOG_STREAM_MAX_FAILURES {
                            return None;
                        }
                        continue;
                    }
                }
            }
            if let Some(item) = stream_state.poll().await {
                return Some((Ok(item.to_event()), stream_state));
            }
            if stream_state.failures >= LOG_STREAM_MAX_FAILURES {
                return None;
            }
        }
    })
}

/// GET /api/apps/{id}/logs/stream?tail=200 — admin-only SSE live log tail.
///
/// * 404 for unknown applications (checked before touching Docker);
/// * 503 when the Docker worker is unavailable;
/// * bounded ring buffer, incremental `--since` polling, rotation resets,
///   15 s keep-alive, closes when the app is deleted or the worker stays down.
pub async fn app_logs_stream(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(query): Query<LogsQuery>,
) -> AppResult<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>> {
    crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    // F2: container logs are administrator-only; F7 revalidates this identity on
    // every poll and closes the stream if the account loses the role.
    if !user.is_admin() {
        return Err(AppError::Forbidden(
            "container logs require an administrator",
        ));
    }
    let token = crate::auth::cookie_value(&headers, crate::auth::session::SESSION_COOKIE)
        .unwrap_or_default();
    if token.is_empty() {
        return Err(AppError::Unauthorized("authentication required"));
    }
    let app = load_app(&state, &id).await?;
    state.docker.ensure_available().await?;
    let tail = crate::api::parse_bounded(
        query.tail.as_deref(),
        LOG_STREAM_DEFAULT_TAIL,
        LOG_STREAM_MAX_TAIL,
    );
    let identity = LogStreamIdentity {
        user_id: user.id,
        session_id: user.session_id.clone(),
        token,
        ttl_hours: state.config.session_ttl_hours,
    };
    // End the tail when the server shuts down so restarts are not held open.
    let shutdown = state.events.shutdown_signal();
    Ok(Sse::new(futures::StreamExt::take_until(
        log_stream(
            state.clone(),
            identity,
            app.id.clone(),
            app.slug.clone(),
            tail,
        ),
        shutdown,
    ))
    .keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(LOG_STREAM_KEEPALIVE_SECS))
            .text("keep-alive"),
    ))
}

/// GET /api/apps/{id}/history
pub async fn app_history(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let access = crate::access::app_surface(&state.db, &state, &user, "applications").await?;
    let _app = load_app(&state, &id).await?;
    let rows = sqlx::query(
        "SELECT id, action, status, detail, created_at, finished_at
         FROM app_deployments WHERE app_id = ? ORDER BY created_at DESC, id DESC LIMIT 100",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut item = json!({
                "id": row.get::<i64, _>("id"),
                "appId": id,
                "action": row.get::<String, _>("action"),
                "status": row.get::<String, _>("status"),
                "createdAt": row.get::<i64, _>("created_at"),
                "finishedAt": row.get::<Option<i64>, _>("finished_at"),
            });
            // F2: deployment error detail can include host paths / container
            // stderr; only administrators receive it.
            if !access.read_only {
                if let Some(object) = item.as_object_mut() {
                    object.insert("detail".to_string(), json!(row.get::<String, _>("detail")));
                }
            }
            item
        })
        .collect();
    Ok(Json(json!({"items": items, "total": items.len()})))
}

/// DELETE /api/apps/{id}?removeVolumes=false
///
/// Stops and removes the Compose project containers, then deletes the managed
/// row. Named volumes and the app `data/` directory are kept unless
/// `removeVolumes=true` is passed explicitly.
pub async fn delete_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DeleteQuery>,
) -> AppResult<Json<Value>> {
    let app = load_app(&state, &id).await?;
    state.docker.ensure_available().await?;

    // `down` never removes volumes; a missing compose file simply means the app
    // was never deployed, which is not an error for deletion.
    let down = state
        .docker
        .call(WorkerRequest::new(WorkerOp::ComposeDown).slug(&app.slug))
        .await;
    let mut warnings: Vec<String> = Vec::new();
    if let Err(error) = down {
        match error {
            DockerError::BadRequest(message) if message.contains("no compose.yaml") => {}
            other => {
                warnings.push(format!("could not stop containers: {other}"));
            }
        }
    }

    let remove_volumes = query.remove_volumes.unwrap_or(false);
    let mut removed_volumes: Vec<String> = Vec::new();
    if remove_volumes {
        match state
            .docker
            .call(WorkerRequest::new(WorkerOp::RemoveVolumes).slug(&app.slug))
            .await
        {
            Ok(data) => {
                removed_volumes = data
                    .get("removed")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Err(error) => warnings.push(format!("could not remove volumes: {error}")),
        }
    }

    sqlx::query("DELETE FROM apps WHERE id = ?")
        .bind(&app.id)
        .execute(&state.db)
        .await?;
    remove_app_dir(&state, &app.slug, remove_volumes);
    state.docker.request_reconcile();
    // Keep the audit trail readable even after the app row is gone.
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("app.delete")
            .actor(&admin.0)
            .object("app", &app.id)
            .path(&app.slug)
            .success()
            .detail(format!(
                "removeVolumes={remove_volumes}, removedVolumes={}",
                removed_volumes.len()
            )),
    )
    .await;
    Ok(Json(json!({
        "deleted": true,
        "appId": app.id,
        "slug": app.slug,
        "removeVolumes": remove_volumes,
        "removedVolumes": removed_volumes,
        "warnings": warnings,
    })))
}

// ---------------------------------------------------------------------------
// Docker import
// ---------------------------------------------------------------------------

/// POST /api/docker/import  {containerIds: []}
///
/// Explicitly imports (adopts) unmanaged containers: writes a Compose file and
/// an `apps` row. The original containers are left running; deploying the
/// imported app as a managed project is a separate, explicit step, and host
/// bind mounts / named volumes are refused because they cannot be represented
/// safely inside the app sandbox.
pub async fn docker_import(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(request): Json<ImportRequest>,
) -> AppResult<Json<Value>> {
    if request.container_ids.len() > MAX_IMPORT_CONTAINERS {
        return Err(AppError::bad_request(format!(
            "at most {MAX_IMPORT_CONTAINERS} containers can be imported at once"
        )));
    }
    state.docker.ensure_available().await?;
    let mut ids: Vec<String> = Vec::new();
    for id in &request.container_ids {
        let id = id.trim().to_string();
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::bad_request(format!(
                "invalid container id {id:?}"
            )));
        }
        if !ids.iter().any(|existing| existing == &id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(AppError::bad_request("containerIds must not be empty"));
    }
    let data = state
        .docker
        .call(WorkerRequest::new(WorkerOp::Inspect).ids(ids.clone()))
        .await
        .map_err(AppError::from)?;
    let inspected = data
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut imported: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for container in inspected {
        let container_id = container
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match import_one_container(&state, &admin.0, &container).await {
            Ok(app) => {
                audit::record_with_events(
                    &state.db,
                    &state.events,
                    AuditLog::new("docker.import")
                        .actor(&admin.0)
                        .object("app", &app.id)
                        .path(&app.slug)
                        .success()
                        .detail(format!("container {container_id}")),
                )
                .await;
                imported.push(app_json(&app, &state.secrets, true, &[], "imported"));
            }
            Err(error) => {
                skipped.push(json!({
                    "containerId": container_id,
                    "name": container.get("name").cloned().unwrap_or(Value::Null),
                    "reason": error.public_message(),
                }));
            }
        }
    }
    if imported.is_empty() {
        let reasons = skipped
            .iter()
            .filter_map(|item| item.get("reason").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AppError::bad_request(format!(
            "no containers could be imported: {reasons}"
        )));
    }
    state.docker.request_reconcile();
    Ok(Json(json!({
        "items": imported,
        "skipped": skipped,
        "note": "Imported containers are left running. Stop and remove the originals before deploying the imported app to avoid port/name conflicts.",
    })))
}

async fn import_one_container(
    state: &AppState,
    user: &AuthUser,
    container: &Value,
) -> AppResult<AppRecord> {
    let name = container
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("container has no name"));
    }
    let project = container
        .get("composeProject")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !project.is_empty() {
        return Err(AppError::bad_request(format!(
            "container already belongs to Compose project {project:?}; not adopting it"
        )));
    }
    let image = container
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !valid_image_reference(&image) {
        return Err(AppError::bad_request(format!(
            "container image {image:?} cannot be imported"
        )));
    }
    // Refuse stateful layouts we cannot represent safely in the sandbox.
    let empty = Vec::new();
    let mounts = container
        .get("mounts")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if let Some(first) = mounts.first() {
        return Err(AppError::bad_request(format!(
            "container has persistent mounts (first: {:?}); only stateless containers can be imported",
            first
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        )));
    }

    let mut env_entries: Vec<EnvInput> = Vec::new();
    if let Some(env) = container.get("env").and_then(Value::as_array) {
        for item in env {
            let Some(text) = item.as_str() else { continue };
            let Some((key, value)) = text.split_once('=') else {
                continue;
            };
            if is_default_container_env(key) || !valid_env_key(key) {
                continue;
            }
            env_entries.push(EnvInput {
                key: key.to_string(),
                value: value.to_string(),
                secret: true,
            });
        }
    }
    env_entries.truncate(MAX_ENV_ENTRIES);
    let stored_env = build_stored_env(&state.secrets, &env_entries)?;

    let app_name = validate_app_name(&name)?;
    let service = sanitize_service_name(&name);
    let compose_text = generate_import_compose(container, &service, &env_entries, &name)?;
    compose::check_compose_text(&compose_text, "import").map_err(AppError::bad_request)?;
    let env_file = render_env_file(&state.secrets, &stored_env)?;
    let slug = unique_slug(state, &name).await?;
    write_app_files(state, &slug, &compose_text, &env_file)?;
    if let Err(error) = validate_compose_via_worker(state, &slug).await {
        remove_app_dir(state, &slug, true);
        return Err(error);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = db::now_ms();
    let env_json = serde_json::to_string(&stored_env)
        .map_err(|e| AppError::internal(format!("cannot encode environment: {e}")))?;
    let insert = sqlx::query(
        "INSERT INTO apps (id, name, slug, compose, env_json, status, last_error, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'imported', '', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&app_name)
    .bind(&slug)
    .bind(&compose_text)
    .bind(&env_json)
    .bind(user.id)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(error) = insert {
        remove_app_dir(state, &slug, true);
        return Err(error.into());
    }
    load_app(state, &id).await
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppAction {
    Deploy,
    Start,
    Stop,
    Restart,
    Redeploy,
}

impl AppAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Redeploy => "redeploy",
        }
    }

    fn worker_op(self) -> WorkerOp {
        match self {
            Self::Deploy | Self::Start | Self::Redeploy => WorkerOp::ComposeUp,
            Self::Stop => WorkerOp::ComposeStop,
            Self::Restart => WorkerOp::ComposeRestart,
        }
    }

    fn result_status(self) -> &'static str {
        match self {
            Self::Stop => "stopped",
            _ => "running",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Deploy => "deploying application",
            Self::Start => "starting application",
            Self::Stop => "stopping application",
            Self::Restart => "restarting application",
            Self::Redeploy => "redeploying application",
        }
    }
}

async fn enqueue_for_existing(
    state: &AppState,
    app: &AppRecord,
    action: AppAction,
    user: &AuthUser,
) -> AppResult<String> {
    let now = db::now_ms();
    sqlx::query(
        "UPDATE apps SET status = 'deploying', last_error = '', updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(&app.id)
    .execute(&state.db)
    .await?;
    match enqueue_app_job(state, &app.id, &app.slug, action, user).await {
        Ok(job_id) => Ok(job_id),
        Err(error) => {
            mark_app_error(state, &app.id, &error.public_message()).await;
            Err(error)
        }
    }
}

async fn enqueue_app_job(
    state: &AppState,
    app_id: &str,
    slug: &str,
    action: AppAction,
    user: &AuthUser,
) -> AppResult<String> {
    let job_state = state.clone();
    let jobs = state.jobs.clone();
    let app_id = app_id.to_string();
    let slug = slug.to_string();
    let spec = JobSpec::new("app.deploy", move |ctx| async move {
        run_app_job(job_state, app_id, slug, action, ctx).await
    })
    .actor(user)
    .message(action.message());
    jobs.submit(spec).await
}

async fn run_app_job(
    state: AppState,
    app_id: String,
    slug: String,
    action: AppAction,
    ctx: JobContext,
) -> Result<Value, AppError> {
    let deployment_id = begin_deployment(&state, &app_id, action.as_str()).await?;
    ctx.progress(1, 3, "validating").await;
    if let Err(error) = ctx.check_cancelled() {
        finish_deployment(&state, deployment_id, "cancelled", "cancelled by user").await;
        return Err(error);
    }

    let request = WorkerRequest::new(action.worker_op()).slug(&slug);
    let outcome = state.docker.call(request).await;
    ctx.progress(2, 3, "applying").await;

    match outcome {
        Ok(data) => {
            let status = action.result_status();
            let now = db::now_ms();
            sqlx::query("UPDATE apps SET status = ?, last_error = '', updated_at = ? WHERE id = ?")
                .bind(status)
                .bind(now)
                .bind(&app_id)
                .execute(&state.db)
                .await?;
            finish_deployment(
                &state,
                deployment_id,
                "success",
                &format!("{} finished", action.as_str()),
            )
            .await;
            ctx.progress(3, 3, "done").await;
            // Wake the reconciler so admins see the new state without waiting
            // for the next 5 s tick (it coalesces duplicate notifications).
            state.docker.request_reconcile();
            let _ = data;
            Ok(json!({
                "appId": app_id,
                "slug": slug,
                "action": action.as_str(),
                "status": status,
            }))
        }
        Err(error) => {
            let message = error.to_string();
            let now = db::now_ms();
            sqlx::query(
                "UPDATE apps SET status = 'error', last_error = ?, updated_at = ? WHERE id = ?",
            )
            .bind(audit::sanitize(&message, 500))
            .bind(now)
            .bind(&app_id)
            .execute(&state.db)
            .await?;
            finish_deployment(&state, deployment_id, "failed", &message).await;
            state.docker.request_reconcile();
            Err(error.into())
        }
    }
}

async fn begin_deployment(state: &AppState, app_id: &str, action: &str) -> AppResult<i64> {
    let result = sqlx::query(
        "INSERT INTO app_deployments (app_id, action, status, detail, created_at) VALUES (?, ?, 'running', '', ?)",
    )
    .bind(app_id)
    .bind(action)
    .bind(db::now_ms())
    .execute(&state.db)
    .await?;
    Ok(result.last_insert_rowid())
}

async fn finish_deployment(state: &AppState, id: i64, status: &str, detail: &str) {
    let _ = sqlx::query(
        "UPDATE app_deployments SET status = ?, detail = ?, finished_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(audit::sanitize(detail, 500))
    .bind(db::now_ms())
    .bind(id)
    .execute(&state.db)
    .await;
}

async fn mark_app_error(state: &AppState, app_id: &str, message: &str) {
    let _ = sqlx::query(
        "UPDATE apps SET status = 'error', last_error = ?, updated_at = ? WHERE id = ?",
    )
    .bind(audit::sanitize(message, 500))
    .bind(db::now_ms())
    .bind(app_id)
    .execute(&state.db)
    .await;
}

// ---------------------------------------------------------------------------
// Validation & serialization helpers
// ---------------------------------------------------------------------------

fn validate_app_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("application name must not be empty"));
    }
    if name.chars().count() > MAX_APP_NAME_LEN {
        return Err(AppError::bad_request(format!(
            "application name must be at most {MAX_APP_NAME_LEN} characters"
        )));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(AppError::bad_request(
            "application name must not contain control characters",
        ));
    }
    Ok(name.to_string())
}

/// Turn a display name into a slug: lowercase ascii alphanumerics, single
/// dashes between words, max 40 chars.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && out.len() < 39 {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// Generate a unique slug, checking both the database and the filesystem.
async fn unique_slug(state: &AppState, name: &str) -> AppResult<String> {
    let base = slugify(name);
    for attempt in 0..100 {
        let candidate = if attempt == 0 {
            base.clone()
        } else {
            let suffix = format!("-{attempt}");
            let keep = 40usize.saturating_sub(suffix.len());
            format!("{}{}", &base[..base.len().min(keep)], suffix)
        };
        let taken: Option<String> = sqlx::query_scalar("SELECT slug FROM apps WHERE slug = ?")
            .bind(&candidate)
            .fetch_optional(&state.db)
            .await?;
        if taken.is_some() {
            continue;
        }
        if app_dir_path(state, &candidate).exists() {
            continue;
        }
        return Ok(candidate);
    }
    // Extremely unlikely; fall back to a random suffix.
    let candidate = format!(
        "app-{}",
        uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>()
    );
    validate_slug(&candidate).map_err(AppError::bad_request)?;
    Ok(candidate)
}

/// Environment keys must be shell/POSIX-style identifiers.
pub fn valid_env_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_ENV_KEY_LEN {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap_or(' ');
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_env_value(key: &str, value: &str) -> AppResult<()> {
    if value.chars().count() > MAX_ENV_VALUE_LEN {
        return Err(AppError::bad_request(format!(
            "environment value for {key} is too long (max {MAX_ENV_VALUE_LEN} characters)"
        )));
    }
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(AppError::bad_request(format!(
            "environment value for {key} must be a single line"
        )));
    }
    Ok(())
}

/// Encrypt secrets and persist the complete environment list.
fn build_stored_env(secrets: &Secrets, entries: &[EnvInput]) -> AppResult<Vec<StoredEnv>> {
    if entries.len() > MAX_ENV_ENTRIES {
        return Err(AppError::bad_request(format!(
            "at most {MAX_ENV_ENTRIES} environment entries are allowed"
        )));
    }
    let mut stored: Vec<StoredEnv> = Vec::with_capacity(entries.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        let key = entry.key.trim().to_string();
        if !valid_env_key(&key) {
            return Err(AppError::bad_request(format!(
                "invalid environment variable name {key:?}"
            )));
        }
        if !seen.insert(key.clone()) {
            return Err(AppError::bad_request(format!(
                "duplicate environment variable {key}"
            )));
        }
        validate_env_value(&key, &entry.value)?;
        let value = if entry.secret {
            if entry.value.is_empty() {
                // An explicitly empty secret (e.g. unset token) is allowed.
                secrets
                    .encrypt_str("")
                    .map_err(|e| AppError::internal(format!("cannot encrypt secret: {e}")))?
            } else {
                secrets
                    .encrypt_str(&entry.value)
                    .map_err(|e| AppError::internal(format!("cannot encrypt secret: {e}")))?
            }
        } else {
            entry.value.clone()
        };
        stored.push(StoredEnv {
            key,
            secret: entry.secret,
            value,
        });
    }
    Ok(stored)
}

/// Merge submitted environment values with the stored ones for a redeploy.
/// A secret submitted as the mask placeholder (`••••`) or as an empty value
/// keeps the previously stored ciphertext, so the UI can round-trip masked
/// values without exposing them.
fn merge_redeploy_env(
    secrets: &Secrets,
    existing: &[StoredEnv],
    incoming: &[EnvInput],
) -> AppResult<Vec<StoredEnv>> {
    if incoming.len() > MAX_ENV_ENTRIES {
        return Err(AppError::bad_request(format!(
            "at most {MAX_ENV_ENTRIES} environment entries are allowed"
        )));
    }
    let by_key: BTreeMap<&str, &StoredEnv> = existing
        .iter()
        .map(|entry| (entry.key.as_str(), entry))
        .collect();
    let mut stored: Vec<StoredEnv> = Vec::with_capacity(incoming.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in incoming {
        let key = entry.key.trim().to_string();
        if !valid_env_key(&key) {
            return Err(AppError::bad_request(format!(
                "invalid environment variable name {key:?}"
            )));
        }
        if !seen.insert(key.clone()) {
            return Err(AppError::bad_request(format!(
                "duplicate environment variable {key}"
            )));
        }
        if entry.secret && (entry.value.is_empty() || entry.value == MASKED_ENV_VALUE) {
            match by_key.get(key.as_str()) {
                Some(previous) if previous.secret => {
                    stored.push((*previous).clone());
                    continue;
                }
                _ => {
                    return Err(AppError::bad_request(format!(
                        "a value is required for secret environment variable {key}"
                    )))
                }
            }
        }
        validate_env_value(&key, &entry.value)?;
        let value = if entry.secret {
            secrets
                .encrypt_str(&entry.value)
                .map_err(|e| AppError::internal(format!("cannot encrypt secret: {e}")))?
        } else {
            entry.value.clone()
        };
        stored.push(StoredEnv {
            key,
            secret: entry.secret,
            value,
        });
    }
    Ok(stored)
}

fn parse_stored_env(env_json: &str) -> Vec<StoredEnv> {
    serde_json::from_str::<Vec<StoredEnv>>(env_json).unwrap_or_default()
}

/// Environment values as returned to clients: secrets are always masked.
fn env_response(_secrets: &Secrets, stored: &[StoredEnv]) -> Vec<Value> {
    stored
        .iter()
        .map(|entry| {
            let value = if entry.secret {
                MASKED_ENV_VALUE.to_string()
            } else {
                entry.value.clone()
            };
            json!({
                "key": entry.key,
                "secret": entry.secret,
                "value": value,
            })
        })
        .collect()
}

/// Render the `.env` file consumed by `docker compose --env-file`. Secret
/// values are decrypted in memory only; the function never logs them.
fn render_env_file(secrets: &Secrets, stored: &[StoredEnv]) -> AppResult<String> {
    let mut out = String::new();
    for entry in stored {
        let value = if entry.secret {
            secrets.decrypt_string(&entry.value).map_err(|_| {
                AppError::internal(format!(
                    "cannot decrypt stored environment variable {}",
                    entry.key
                ))
            })?
        } else {
            entry.value.clone()
        };
        out.push_str(&entry.key);
        out.push('=');
        out.push_str(&quote_env_value(&value));
        out.push('\n');
    }
    Ok(out)
}

/// Quote a value for a Compose `.env` file. Double quotes are used so `#`
/// stays literal; `$` is escaped as `$$` so Compose does not interpolate it,
/// and backslashes/quotes/newlines are escaped.
pub fn quote_env_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Maximum length of the sanitized `lastError` shown to read-only viewers.
const RESTRICTED_LAST_ERROR_MAX: usize = 200;

/// Sanitize deployment error text for read-only viewers: first line only,
/// control characters stripped, bounded length.
pub fn sanitize_last_error(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    audit::sanitize(first_line, RESTRICTED_LAST_ERROR_MAX)
}

/// Read-only application payload (F2): status/metadata only. Environment
/// variables are never included (only their counts), and the Compose document
/// and history details are administrator-only.
fn app_json_restricted(app: &AppRecord, containers: &[Value], status: &str) -> Value {
    let secret_count = app.env.iter().filter(|entry| entry.secret).count();
    json!({
        "id": app.id,
        "name": app.name,
        "slug": app.slug,
        "status": status,
        "lastError": sanitize_last_error(&app.last_error),
        "envCount": app.env.len(),
        "secretCount": secret_count,
        "createdAt": app.created_at,
        "updatedAt": app.updated_at,
        "containers": containers,
    })
}

/// Read-only-safe Docker status: never expose host paths or worker socket
/// locations in `reason` (F2).
fn docker_status_for_read_only(status: Value) -> Value {
    let available = status
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = status;
    if let Some(object) = out.as_object_mut() {
        object.insert(
            "reason".to_string(),
            if available {
                Value::Null
            } else {
                Value::String("Docker integration is unavailable".to_string())
            },
        );
    }
    out
}

fn app_json(
    app: &AppRecord,
    secrets: &Secrets,
    include_compose: bool,
    containers: &[Value],
    status: &str,
) -> Value {
    let mut object = json!({
        "id": app.id,
        "name": app.name,
        "slug": app.slug,
        "status": status,
        "lastError": app.last_error,
        "env": env_response(secrets, &app.env),
        "createdAt": app.created_at,
        "updatedAt": app.updated_at,
        "containers": containers,
    });
    if include_compose {
        if let Some(map) = object.as_object_mut() {
            map.insert("compose".to_string(), json!(app.compose));
        }
    }
    object
}

async fn load_app(state: &AppState, id: &str) -> AppResult<AppRecord> {
    let row = sqlx::query(
        "SELECT id, name, slug, compose, env_json, status, last_error, created_at, updated_at
         FROM apps WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("application not found"))?;
    Ok(AppRecord::from_row(&row))
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn app_dir_path(state: &AppState, slug: &str) -> PathBuf {
    state.config.apps_dir().join(slug)
}

fn write_app_files(
    state: &AppState,
    slug: &str,
    compose_text: &str,
    env_file: &str,
) -> AppResult<()> {
    validate_slug(slug).map_err(AppError::bad_request)?;
    let dir = app_dir_path(state, slug);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    // The application directory and files must be readable by the privileged
    // worker (User=rast-worker, Group=rast) which validates and deploys the
    // compose file. Owner rast + group rast with 0640/0750 is enough — the
    // worker never needs write access to the definitions.
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750));
    write_atomic(&dir.join("compose.yaml"), compose_text.as_bytes(), 0o640)?;
    write_atomic(&dir.join(".env"), env_file.as_bytes(), 0o640)?;
    Ok(())
}

/// Write a file atomically: temp file with restrictive mode in the same
/// directory, fsync, rename.
fn write_atomic(path: &Path, content: &[u8], mode: u32) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::internal("invalid destination path"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("write"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)?;
    use std::io::Write;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

/// Remove the rendered Compose files. With `remove_data` the whole app
/// directory (including bind-mount data) is deleted; otherwise `data/` and
/// other user content stays on disk so volumes are preserved by default.
fn remove_app_dir(state: &AppState, slug: &str, remove_data: bool) {
    let dir = app_dir_path(state, slug);
    if !dir.exists() {
        return;
    }
    if remove_data {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let _ = std::fs::remove_file(dir.join("compose.yaml"));
    let _ = std::fs::remove_file(dir.join(".env"));
    // Keep data, but do not leave an empty managed directory behind.
    if let Ok(mut entries) = std::fs::read_dir(&dir) {
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// Docker probing helpers
// ---------------------------------------------------------------------------

type ProjectContainers = BTreeMap<String, Vec<Value>>;

async fn probe_all_containers(state: &AppState) -> (ProjectContainers, bool, Option<String>) {
    if !state.docker.enabled {
        return (
            ProjectContainers::new(),
            false,
            Some("Docker integration is disabled (RAST_DOCKER_ENABLED=false)".to_string()),
        );
    }
    match state.docker.all_containers().await {
        Ok(data) => {
            let mut grouped: ProjectContainers = BTreeMap::new();
            if let Some(items) = data.get("items").and_then(Value::as_array) {
                for container in items {
                    let project = container
                        .get("project")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    grouped.entry(project).or_default().push(container.clone());
                }
            }
            (grouped, true, None)
        }
        Err(error) => (ProjectContainers::new(), false, Some(error.to_string())),
    }
}

async fn probe_app_containers(state: &AppState, slug: &str) -> (Vec<Value>, Option<String>) {
    if !state.docker.enabled {
        return (
            Vec::new(),
            Some("Docker integration is disabled (RAST_DOCKER_ENABLED=false)".to_string()),
        );
    }
    match state.docker.app_containers(slug).await {
        Ok(data) => (
            data.get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error.to_string())),
    }
}

async fn probe_app_resources(
    state: &AppState,
    containers: &[Value],
) -> (Vec<Value>, Option<String>) {
    if containers.is_empty() {
        return (Vec::new(), None);
    }
    if !state.docker.enabled {
        return (
            Vec::new(),
            Some("Docker integration is disabled".to_string()),
        );
    }
    let names: BTreeSet<&str> = containers
        .iter()
        .filter_map(|container| container.get("name").and_then(Value::as_str))
        .collect();
    match state.docker.container_stats().await {
        Ok(data) => {
            let items = data
                .get("items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| {
                            item.get("name")
                                .and_then(Value::as_str)
                                .map(|name| names.contains(name))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            (items, None)
        }
        Err(error) => (Vec::new(), Some(error.to_string())),
    }
}

fn collect_ports(containers: &[Value]) -> Vec<Value> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for container in containers {
        let Some(ports) = container.get("ports").and_then(Value::as_array) else {
            continue;
        };
        for port in ports {
            let container_port = port
                .get("containerPort")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let host_port = port.get("hostPort").and_then(Value::as_u64);
            let protocol = port
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("tcp");
            let key = format!("{container_port}/{protocol}/{host_port:?}");
            if seen.insert(key) {
                out.push(port.clone());
            }
        }
    }
    out
}

fn collect_volumes(containers: &[Value]) -> Vec<Value> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for container in containers {
        if let Some(mounts) = container.get("mounts").and_then(Value::as_array) {
            for mount in mounts {
                let text = mount.as_str().unwrap_or_default().trim();
                if text.is_empty() {
                    continue;
                }
                for part in text.split(',') {
                    let part = part.trim();
                    if !part.is_empty() && seen.insert(part.to_string()) {
                        out.push(json!(part));
                    }
                }
            }
        }
    }
    out
}

async fn validate_compose_via_worker(state: &AppState, slug: &str) -> AppResult<()> {
    match state
        .docker
        .call(WorkerRequest::new(WorkerOp::ComposeValidate).slug(slug))
        .await
    {
        Ok(_) => Ok(()),
        Err(DockerError::BadRequest(message)) => Err(AppError::bad_request(format!(
            "compose configuration is not valid: {message}"
        ))),
        Err(other) => Err(other.into()),
    }
}

// ---------------------------------------------------------------------------
// Import helpers
// ---------------------------------------------------------------------------

fn is_default_container_env(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "HOSTNAME"
            | "HOME"
            | "TERM"
            | "PWD"
            | "OLDPWD"
            | "SHLVL"
            | "_"
            | "LANG"
            | "container"
            | "container_uuid"
            | "DOCKER_HOST"
            | "DOCKER_TLS_VERIFY"
            | "DOCKER_CERT_PATH"
    ) || key.starts_with("DOCKER_")
}

fn valid_image_reference(image: &str) -> bool {
    if image.is_empty() || image.len() > 255 {
        return false;
    }
    image
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+'))
}

fn sanitize_service_name(name: &str) -> String {
    let slug = slugify(name);
    if slug.is_empty() {
        "app".to_string()
    } else {
        slug
    }
}

/// Build a Compose file for a stateless container discovered on the host.
fn generate_import_compose(
    container: &Value,
    service: &str,
    env_entries: &[EnvInput],
    container_name: &str,
) -> AppResult<String> {
    let image = container
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !valid_image_reference(&image) {
        return Err(AppError::bad_request("container image is not importable"));
    }
    let mut yaml = String::from("services:\n");
    yaml.push_str(&format!("  {}:\n", service));
    yaml.push_str(&format!("    image: \"{}\"\n", escape_yaml_double(&image)));
    yaml.push_str("    restart: unless-stopped\n");

    let empty = Vec::new();
    let ports = container
        .get("ports")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut port_lines: Vec<String> = Vec::new();
    for port in ports {
        let container_port = port
            .get("containerPort")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !(1..=65535).contains(&container_port) {
            continue;
        }
        let protocol = match port
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("tcp")
        {
            "udp" => "udp",
            "sctp" => "sctp",
            _ => "tcp",
        };
        let line = match port.get("hostPort").and_then(Value::as_u64) {
            Some(host_port) if (1..=65535).contains(&host_port) => {
                format!("\"{host_port}:{container_port}/{protocol}\"")
            }
            _ => format!("\"{container_port}/{protocol}\""),
        };
        if !port_lines.contains(&line) {
            port_lines.push(line);
        }
    }
    if !port_lines.is_empty() {
        yaml.push_str("    ports:\n");
        for line in port_lines {
            yaml.push_str(&format!("      - {line}\n"));
        }
    }
    if !env_entries.is_empty() {
        yaml.push_str("    environment:\n");
        for entry in env_entries {
            yaml.push_str(&format!("      - {}=${{{}}}\n", entry.key, entry.key));
        }
    }
    // Keep the original container name as documentation only; Compose names
    // containers after the project so no name conflict is created.
    let _ = container_name;
    Ok(yaml)
}

fn escape_yaml_double(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_secrets(dir: &Path) -> Secrets {
        Secrets::load_or_create(&dir.join("secret.key")).unwrap()
    }

    fn test_config(dir: &Path) -> crate::config::Config {
        let mut cfg =
            crate::config::Config::from_parts(None, crate::config::FileConfig::default(), |_| None)
                .unwrap();
        cfg.data_dir = dir.join("data");
        cfg.storage_dir = dir.join("storage");
        cfg.worker_socket = dir.join("worker.sock");
        cfg.docker_enabled = false;
        cfg.monitor_interval_secs = 1;
        cfg.db_max_connections = 2;
        cfg.job_concurrency = 1;
        cfg
    }

    fn admin() -> AuthUser {
        AuthUser {
            id: 1,
            username: "admin".into(),
            display_name: "Admin".into(),
            role: crate::auth::Role::Admin,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    fn admin_user() -> AdminUser {
        AdminUser(admin())
    }

    #[test]
    fn slug_generation_is_stable_and_safe() {
        assert_eq!(slugify("My Shop App"), "my-shop-app");
        assert_eq!(slugify("  ---Hello---World---  "), "hello-world");
        assert_eq!(slugify("Ünïcode App"), "n-code-app");
        assert_eq!(slugify(""), "app");
        assert_eq!(slugify("!!!"), "app");
        let long = slugify(&"a".repeat(200));
        assert!(long.len() <= 40);
        validate_slug(&long).unwrap();
        assert_eq!(long, "a".repeat(40));
    }

    #[test]
    fn env_keys_are_validated() {
        for ok in ["A", "_X", "MY_VAR_1", "lower_case"] {
            assert!(valid_env_key(ok), "{ok}");
        }
        for bad in [
            "",
            "1A",
            "A B",
            "A-B",
            "A=B",
            "A\0",
            &"A".repeat(MAX_ENV_KEY_LEN + 1),
        ] {
            assert!(!valid_env_key(bad), "{bad:?}");
        }
        let secrets = temp_secrets(tempfile::tempdir().unwrap().path());
        let err = build_stored_env(
            &secrets,
            &[
                EnvInput {
                    key: "DUP".into(),
                    value: "a".into(),
                    secret: false,
                },
                EnvInput {
                    key: "DUP".into(),
                    value: "b".into(),
                    secret: false,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        let err = build_stored_env(
            &secrets,
            &[EnvInput {
                key: "MULTILINE".into(),
                value: "a\nb".into(),
                secret: false,
            }],
        )
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn secrets_are_encrypted_and_masked_in_responses() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = temp_secrets(dir.path());
        let stored = build_stored_env(
            &secrets,
            &[
                EnvInput {
                    key: "PUBLIC_URL".into(),
                    value: "https://example.test".into(),
                    secret: false,
                },
                EnvInput {
                    key: "DB_PASSWORD".into(),
                    value: "super-secret".into(),
                    secret: true,
                },
            ],
        )
        .unwrap();
        // Persisted value is ciphertext, not the plaintext secret.
        assert_ne!(stored[1].value, "super-secret");
        assert!(!stored[1].value.contains("super-secret"));
        // Round-trips through the cipher.
        assert_eq!(
            secrets.decrypt_string(&stored[1].value).unwrap(),
            "super-secret"
        );
        // Responses mask secret values and keep public ones.
        let response = env_response(&secrets, &stored);
        assert_eq!(response[0]["value"], "https://example.test");
        assert_eq!(response[1]["value"], MASKED_ENV_VALUE);
        assert_eq!(response[1]["secret"], true);
        // The rendered .env contains the plaintext (needed by Compose) but is
        // only written to a 0600 file.
        let env_file = render_env_file(&secrets, &stored).unwrap();
        assert!(env_file.contains("DB_PASSWORD=\"super-secret\""));
        assert!(env_file.contains("PUBLIC_URL=\"https://example.test\""));
    }

    #[test]
    fn secret_merge_keeps_masked_values() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = temp_secrets(dir.path());
        let existing = build_stored_env(
            &secrets,
            &[EnvInput {
                key: "TOKEN".into(),
                value: "old-token".into(),
                secret: true,
            }],
        )
        .unwrap();
        // Masked/empty secret values keep the old ciphertext.
        for value in [MASKED_ENV_VALUE.to_string(), String::new()] {
            let merged = merge_redeploy_env(
                &secrets,
                &existing,
                &[EnvInput {
                    key: "TOKEN".into(),
                    value,
                    secret: true,
                }],
            )
            .unwrap();
            assert_eq!(
                secrets.decrypt_string(&merged[0].value).unwrap(),
                "old-token"
            );
        }
        // A new value replaces the ciphertext.
        let merged = merge_redeploy_env(
            &secrets,
            &existing,
            &[EnvInput {
                key: "TOKEN".into(),
                value: "new-token".into(),
                secret: true,
            }],
        )
        .unwrap();
        assert_eq!(
            secrets.decrypt_string(&merged[0].value).unwrap(),
            "new-token"
        );
        // A brand-new secret without a value is rejected.
        let err = merge_redeploy_env(
            &secrets,
            &existing,
            &[EnvInput {
                key: "OTHER".into(),
                value: MASKED_ENV_VALUE.into(),
                secret: true,
            }],
        )
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn env_file_values_are_escaped() {
        assert_eq!(quote_env_value("plain"), "\"plain\"");
        assert_eq!(quote_env_value("a$b"), "\"a$$b\"");
        assert_eq!(quote_env_value("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote_env_value("a#b"), "\"a#b\"");
    }

    #[test]
    fn atomic_file_writes_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compose.yaml");
        write_atomic(&path, b"services: {}\n", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "services: {}\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        write_atomic(&path, b"services: {}\n", 0o600).unwrap();
    }

    #[test]
    fn import_compose_generation_is_valid_yaml_shape() {
        let container = json!({
            "name": "legacy-web",
            "image": "nginx:alpine",
            "ports": [{"containerPort": 80, "hostPort": 8080, "protocol": "tcp"}],
            "env": ["FOO=bar"],
        });
        let env = vec![EnvInput {
            key: "FOO".into(),
            value: "bar".into(),
            secret: true,
        }];
        let yaml = generate_import_compose(&container, "legacy-web", &env, "legacy-web").unwrap();
        assert!(yaml.contains("services:\n  legacy-web:"));
        assert!(yaml.contains("image: \"nginx:alpine\""));
        assert!(yaml.contains("\"8080:80/tcp\""));
        assert!(yaml.contains("- FOO=${FOO}"));
        compose::check_compose_text(&yaml, "import").unwrap();
        // The normalized-config validator accepts the generated shape.
        let normalized = json!({
            "services": {"legacy-web": {"image": "nginx:alpine", "ports": [{"target": 80, "published": "8080", "protocol": "tcp"}]}}
        });
        compose::validate_normalized_config(&normalized, &PathBuf::from("/tmp/apps/import"))
            .unwrap();
    }

    #[test]
    fn import_rejects_unsafe_images_and_env_filters() {
        assert!(!valid_image_reference(""));
        assert!(!valid_image_reference("nginx\nimage"));
        assert!(!valid_image_reference("nginx alpine"));
        assert!(valid_image_reference("registry.example/a:b@sha256:abc"));
        assert!(is_default_container_env("PATH"));
        assert!(is_default_container_env("DOCKER_HOST"));
        assert!(!is_default_container_env("DATABASE_URL"));
    }

    #[tokio::test]
    async fn docker_unavailable_endpoints_return_503() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();

        // docker/status always answers 200 and explains why.
        let response = docker_status(State(state.clone()), admin()).await.unwrap();
        assert_eq!(response.0["enabled"], false);
        assert_eq!(response.0["available"], false);

        // docker/containers needs Docker: 503.
        let err = docker_containers(
            State(state.clone()),
            admin_user(),
            Query(PageQuery {
                page: None,
                per_page: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        // app mutations need Docker: 503 (after cheap request validation).
        let err = create_app(
            State(state.clone()),
            admin_user(),
            Json(CreateAppRequest {
                name: "web".into(),
                compose: "services:\n  web:\n    image: nginx:alpine\n".into(),
                env: Vec::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let err = start_app(
            State(state.clone()),
            admin_user(),
            AxumPath("missing".into()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        // Listing managed apps works without Docker and reports the reason.
        let list = list_apps(
            State(state.clone()),
            admin(),
            Query(PageQuery {
                page: None,
                per_page: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(list.0["items"].as_array().unwrap().len(), 0);
        assert_eq!(list.0["docker"]["available"], false);
        assert!(list.0["docker"]["reason"].as_str().is_some());
    }

    #[tokio::test]
    async fn invalid_requests_are_rejected_before_touching_docker() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();
        // Empty name -> 400 even though Docker is disabled.
        let err = create_app(
            State(state.clone()),
            admin_user(),
            Json(CreateAppRequest {
                name: "  ".into(),
                compose: "services: {}\n".into(),
                env: Vec::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);

        // Invalid env key -> 400.
        let err = create_app(
            State(state.clone()),
            admin_user(),
            Json(CreateAppRequest {
                name: "web".into(),
                compose: "services:\n  web:\n    image: nginx\n".into(),
                env: vec![EnvInput {
                    key: "BAD KEY".into(),
                    value: "x".into(),
                    secret: false,
                }],
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn app_json_masks_secrets_and_can_include_compose() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = temp_secrets(dir.path());
        let stored = build_stored_env(
            &secrets,
            &[EnvInput {
                key: "TOKEN".into(),
                value: "sensitive".into(),
                secret: true,
            }],
        )
        .unwrap();
        let app = AppRecord {
            id: "id1".into(),
            name: "Shop".into(),
            slug: "shop".into(),
            compose: "services: {}\n".into(),
            env: stored,
            status: "running".into(),
            last_error: String::new(),
            created_at: 1,
            updated_at: 2,
        };
        let value = app_json(&app, &secrets, true, &[], "running");
        assert_eq!(value["env"][0]["value"], MASKED_ENV_VALUE);
        assert_eq!(value["compose"], "services: {}\n");
        assert!(!value.to_string().contains("sensitive"));
        let without_compose = app_json(&app, &secrets, false, &[], "running");
        assert!(without_compose.get("compose").is_none());
    }

    #[test]
    fn endpoint_router_builds() {
        // Compile-time smoke test: all routes are registered without conflicts.
        let _router = routes();
    }
}

/// Realtime tests for the live application log stream (bounded ring buffer,
/// rotation resets, authorization and SSE transport). Kept in a separate module
/// so the transport helpers stay close to the endpoint under test.
#[cfg(test)]
mod realtime_tests {
    use super::*;
    use axum::extract::FromRequestParts;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use std::path::Path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn admin() -> AuthUser {
        AuthUser {
            id: 1,
            username: "admin".into(),
            display_name: "Admin".into(),
            role: crate::auth::Role::Admin,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    fn admin_cookie_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("rast_session=test-session"),
        );
        headers
    }

    fn test_identity() -> LogStreamIdentity {
        LogStreamIdentity {
            user_id: 1,
            session_id: "test-session".into(),
            token: "test-session".into(),
            ttl_hours: 168,
        }
    }

    fn test_config(dir: &Path, docker_socket: Option<&Path>) -> crate::config::Config {
        let mut cfg =
            crate::config::Config::from_parts(None, crate::config::FileConfig::default(), |_| None)
                .unwrap();
        cfg.data_dir = dir.join("data");
        cfg.storage_dir = dir.join("storage");
        cfg.worker_socket = docker_socket
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.join("unavailable.sock"));
        cfg.docker_enabled = docker_socket.is_some();
        cfg.monitor_interval_secs = 5;
        cfg.db_max_connections = 2;
        cfg.job_concurrency = 1;
        cfg
    }

    async fn insert_app_row(state: &AppState, id: &str, slug: &str) {
        sqlx::query(
            "INSERT INTO apps (id, name, slug, compose, env_json, status, last_error, created_by, created_at, updated_at)
             VALUES (?, ?, ?, 'services: {}', '[]', 'running', '', NULL, 1, 1)",
        )
        .bind(id)
        .bind(slug)
        .bind(slug)
        .execute(&state.db)
        .await
        .unwrap();
    }

    fn log_line(ts: i64, message: &str) -> Value {
        json!({"ts": ts, "stream": "stdout", "service": "web", "message": message})
    }

    #[test]
    fn log_ring_is_bounded_and_dedupes_inclusive_cursor() {
        let mut ring = LogRing::new(3);
        let seeded = ring.seed(
            (1..=5)
                .map(|i| log_line(i * 1000, &format!("m{i}")))
                .collect(),
        );
        assert_eq!(ring.len(), 3, "ring is bounded to its capacity");
        assert_eq!(seeded.len(), 3);
        assert_eq!(seeded[2]["message"], "m5");
        assert_eq!(ring.last_ts(), Some(5000));
        assert_eq!(ring.since_secs(), Some(5));

        // `--since` is inclusive: the newest line is re-delivered by Docker and
        // must be deduped; only genuinely new lines are returned.
        let fresh = ring.append(vec![log_line(5000, "m5"), log_line(6000, "m6")]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0]["message"], "m6");
        // Same timestamp, different message is a distinct line.
        assert_eq!(ring.append(vec![log_line(6000, "other")]).len(), 1);
        // Re-seeding (initial connect / rotation) clears buffer and dedupe state.
        let seeded = ring.seed(vec![log_line(9000, "fresh")]);
        assert_eq!(seeded.len(), 1);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.since_secs(), Some(9));
    }

    #[test]
    fn log_ring_empty_state_has_no_cursor() {
        let mut ring = LogRing::new(LOG_STREAM_RING_CAPACITY);
        assert!(ring.is_empty());
        assert_eq!(ring.since_secs(), None);
        assert_eq!(ring.last_ts(), None);
        assert!(ring.append(Vec::new()).is_empty());
        assert!(ring.snapshot().is_empty());
    }

    #[test]
    fn stream_limits_match_contract() {
        // Compile-time bounds: the ring buffer is bounded and the exposed tail
        // can never exceed it.
        const { assert!(LOG_STREAM_RING_CAPACITY <= 500, "bounded ring buffer") };
        const { assert!(LOG_STREAM_MAX_TAIL <= LOG_STREAM_RING_CAPACITY) };
        assert_eq!(LOG_STREAM_KEEPALIVE_SECS, 15, "contract keep-alive");
        assert!((1..=5).contains(&LOG_STREAM_INTERVAL.as_secs()));
        assert_eq!(LOG_STREAM_DEFAULT_TAIL, 200);
    }

    #[tokio::test]
    async fn logs_stream_unknown_app_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path(), None)).await.unwrap();
        let result = app_logs_stream(
            State(state),
            admin(),
            AxumPath("missing".into()),
            admin_cookie_headers(),
            Query(LogsQuery { tail: None }),
        )
        .await;
        match result {
            Ok(_) => panic!("unknown app must be 404"),
            Err(err) => assert_eq!(err.status(), StatusCode::NOT_FOUND),
        }
    }

    #[tokio::test]
    async fn logs_stream_returns_503_when_docker_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path(), None)).await.unwrap();
        insert_app_row(&state, "app1", "shop").await;
        let result = app_logs_stream(
            State(state),
            admin(),
            AxumPath("app1".into()),
            admin_cookie_headers(),
            Query(LogsQuery {
                tail: Some("200".into()),
            }),
        )
        .await;
        match result {
            Ok(_) => panic!("docker disabled must be 503"),
            Err(err) => {
                assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
                let message = err.public_message().to_ascii_lowercase();
                assert!(message.contains("docker"), "unexpected message: {message}");
            }
        }
    }

    #[tokio::test]
    async fn logs_stream_requires_authentication_via_router() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path(), None)).await.unwrap();
        let router = crate::api::router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/apps/whatever/logs/stream")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logs_stream_rejects_non_admin_principal() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path(), None)).await.unwrap();
        let non_admin = AuthUser {
            role: crate::auth::Role::User,
            ..admin()
        };
        let mut request = axum::http::Request::builder()
            .uri("/api/apps/app1/logs/stream")
            .body(())
            .unwrap();
        request.extensions_mut().insert(non_admin);
        let (mut parts, _) = request.into_parts();
        let result =
            <AdminUser as FromRequestParts<AppState>>::from_request_parts(&mut parts, &state).await;
        match result {
            Ok(_) => panic!("non-admin must be forbidden"),
            Err(err) => assert_eq!(err.status(), StatusCode::FORBIDDEN),
        }
    }

    #[tokio::test]
    async fn logs_stream_sends_bounded_initial_event_over_sse() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "hello from stream".into(),
            },
        );
        let state = AppState::new(test_config(dir.path(), Some(&socket)))
            .await
            .unwrap();
        insert_app_row(&state, "app1", "shop").await;

        let result = app_logs_stream(
            State(state),
            admin(),
            AxumPath("app1".into()),
            admin_cookie_headers(),
            Query(LogsQuery {
                tail: Some("200".into()),
            }),
        )
        .await;
        let sse = match result {
            Ok(sse) => sse,
            Err(err) => panic!("stream should open: {err}"),
        };
        let response = sse.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("text/event-stream"),
            "unexpected content-type: {content_type}"
        );

        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(3), body.frame())
            .await
            .expect("initial SSE frame must arrive promptly")
            .expect("stream must yield a frame")
            .expect("SSE frame must not be an error");
        let data = frame.into_data().expect("data frame");
        let text = String::from_utf8_lossy(&data).into_owned();
        assert!(text.contains("app.logs"), "SSE event name: {text}");
        assert!(text.contains("\"appId\":\"app1\""), "{text}");
        assert!(text.contains("hello from stream"), "{text}");
        assert!(
            text.contains("\"reset\":true"),
            "initial seed resets: {text}"
        );
        worker.abort();
    }

    #[tokio::test]
    async fn logs_stream_resets_when_containers_are_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(socket.clone(), FakeWorkerMode::Rotating);
        let state = AppState::new(test_config(dir.path(), Some(&socket)))
            .await
            .unwrap();
        let mut stream =
            LogStreamState::new(&state, test_identity(), "app1".into(), "shop".into(), 200);
        let first = stream.poll().await.expect("initial seed event");
        assert!(first.reset, "initial seed resets the client");
        assert_eq!(first.app_id, "app1");
        assert_eq!(first.lines[0]["message"], "old-container-line");

        let second = stream.poll().await.expect("rotation event");
        assert!(
            second.reset,
            "container recreation must reset the client: {second:?}"
        );
        assert_eq!(second.lines[0]["message"], "new-container-line");
        assert_eq!(second.lines.len(), 1, "old buffer must be dropped on reset");
        worker.abort();
    }

    // ------------------------------------------------------------------
    // Fake Docker worker used by the transport tests (no Docker required)
    // ------------------------------------------------------------------

    pub(super) enum FakeWorkerMode {
        Initial { message: String },
        Rotating,
    }

    pub(super) fn start_fake_worker(
        socket: PathBuf,
        mode: FakeWorkerMode,
    ) -> tokio::task::JoinHandle<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mode = Arc::new(mode);
        let calls = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mode = mode.clone();
                let calls = calls.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) => return,
                            Ok(_) => {
                                if byte[0] == b'\n' {
                                    break;
                                }
                                raw.push(byte[0]);
                            }
                            Err(_) => return,
                        }
                    }
                    let Ok(request) = serde_json::from_slice::<Value>(&raw) else {
                        return;
                    };
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let op = request
                        .get("op")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let data = match op {
                        "version" => json!({"version": "26.1.4"}),
                        "info" => json!({"version": "26.1.4"}),
                        "stats" => json!({"items": []}),
                        "ps-all" => json!({"items": [
                            {"id": "0123456789ab", "name": "shop-web-1", "image": "nginx:alpine",
                             "state": "running", "status": "Up 1 min", "project": "rast-shop",
                             "ports": [], "mounts": [], "labels": {}}
                        ]}),
                        "logs" => json!({
                            "lines": [{"ts": 1_700_000_000_123i64, "stream": "stdout", "service": "web", "message": "access-test log line"}],
                            "tail": 200,
                        }),
                        "compose-validate" => {
                            json!({"valid": true, "services": ["web"], "bindMounts": 0})
                        }
                        "compose-up" => json!({"action": "up", "running": true}),
                        "compose-stop" => json!({"action": "stop", "running": false}),
                        "compose-restart" => json!({"action": "restart", "running": true}),
                        "compose-down" => json!({"action": "down", "running": false}),
                        "app-containers" | "ps" => json!({
                            "project": "rast-shop",
                            "items": [{"id": "0123456789ab", "name": "shop-web-1",
                                       "image": "nginx:alpine", "state": "running",
                                       "status": "Up 1 min", "project": "rast-shop"}],
                        }),
                        "inspect" => json!({"items": []}),
                        "remove-volumes" => json!({"removed": []}),
                        "compose-logs-since" => {
                            let call = calls.fetch_add(1, Ordering::SeqCst);
                            match mode.as_ref() {
                                FakeWorkerMode::Initial { message } => json!({
                                    "lines": [
                                        {"ts": 1_700_000_000_123i64, "stream": "stdout", "service": "web", "message": message}
                                    ],
                                    "containerIds": ["0123456789ab"],
                                    "tail": 200,
                                }),
                                FakeWorkerMode::Rotating => {
                                    // call 0: seed (old container), call 1: first
                                    // incremental read (detects new container id),
                                    // call 2: reseed from the new container.
                                    let (line, container) = if call == 0 {
                                        ("old-container-line", "0123456789ab")
                                    } else if call == 1 {
                                        ("ignored-transition-line", "abcdef012345")
                                    } else {
                                        ("new-container-line", "abcdef012345")
                                    };
                                    json!({
                                        "lines": [
                                            {"ts": 1_700_000_000_123i64 + call as i64 * 1000, "stream": "stdout", "service": "web", "message": line}
                                        ],
                                        "containerIds": [container],
                                        "tail": 200,
                                    })
                                }
                            }
                        }
                        _ => {
                            let response = json!({
                                "id": id,
                                "ok": false,
                                "error": {"code": "invalid_request", "message": format!("unsupported op {op}")}
                            });
                            let mut line = serde_json::to_vec(&response).unwrap();
                            line.push(b'\n');
                            let _ = stream.write_all(&line).await;
                            let _ = stream.flush().await;
                            return;
                        }
                    };
                    let response = json!({"id": id, "ok": true, "data": data});
                    let mut line = serde_json::to_vec(&response).unwrap();
                    line.push(b'\n');
                    let _ = stream.write_all(&line).await;
                    let _ = stream.flush().await;
                });
            }
        })
    }
}

/// Shared router-level test support for the group-access enforcement tests in
/// `api::apps` and `api::backup`. Compiled only for tests.
#[cfg(test)]
pub(crate) mod access_test_support {
    use axum::{
        body::Body,
        http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::{
        auth::Role,
        config::{Config, FileConfig},
        state::AppState,
        users,
    };

    /// A real `AppState` + router backed by a temporary database/storage tree.
    pub struct TestServer {
        pub _dir: tempfile::TempDir,
        pub state: AppState,
        router: axum::Router,
    }

    /// Authenticated browser session (cookie + CSRF token).
    pub struct Session {
        pub cookie: String,
        pub csrf: String,
    }

    impl Session {
        pub fn headers(&self) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&self.cookie).expect("valid cookie header"),
            );
            headers
        }

        pub fn headers_with_csrf(&self) -> HeaderMap {
            let mut headers = self.headers();
            headers.insert(
                "x-csrf-token",
                HeaderValue::from_str(&self.csrf).expect("valid csrf header"),
            );
            headers
        }
    }

    impl TestServer {
        pub async fn start() -> Self {
            Self::build(None).await
        }

        /// Docker-backed server; the caller must have started a fake worker on
        /// `socket` first (see `api::apps::realtime_tests::start_fake_worker`).
        pub async fn start_with_docker(socket: &std::path::Path) -> Self {
            Self::build(Some(socket)).await
        }

        async fn build(docker_socket: Option<&std::path::Path>) -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let mut cfg = Config::from_parts(
                None,
                FileConfig {
                    data_dir: Some(dir.path().join("data")),
                    storage_dir: Some(dir.path().join("storage")),
                    public_url: Some("https://rast.test".into()),
                    secure_cookies: Some(false),
                    setup_token_required: Some(true),
                    ..Default::default()
                },
                |_| None,
            )
            .expect("config");
            if let Some(socket) = docker_socket {
                cfg.worker_socket = socket.to_path_buf();
                cfg.docker_enabled = true;
            }
            let state = AppState::new(cfg).await.expect("state");
            // Disjoint user-id range: the access evaluator cache is keyed by
            // (user_id, resource) in-process, so parallel tests must not reuse ids.
            {
                static NEXT_USER_BASE: std::sync::atomic::AtomicI64 =
                    std::sync::atomic::AtomicI64::new(1_000_000);
                let base = NEXT_USER_BASE.fetch_add(1000, std::sync::atomic::Ordering::SeqCst);
                let updated =
                    sqlx::query("UPDATE sqlite_sequence SET seq = ? WHERE name = 'users'")
                        .bind(base)
                        .execute(&state.db)
                        .await
                        .map(|result| result.rows_affected())
                        .unwrap_or(0);
                if updated == 0 {
                    let _ =
                        sqlx::query("INSERT INTO sqlite_sequence (name, seq) VALUES ('users', ?)")
                            .bind(base)
                            .execute(&state.db)
                            .await;
                }
                crate::access::bump_generation();
            }
            let router = crate::api::router(state.clone());
            Self {
                _dir: dir,
                state,
                router,
            }
        }

        pub async fn create_user(&self, username: &str, password: &str, role: Role) -> i64 {
            users::create(
                &self.state.db,
                &self.state.config,
                username,
                password,
                role,
                None,
                None,
            )
            .await
            .expect("create user")
            .id
        }

        pub async fn login(&self, username: &str, password: &str) -> Session {
            let (status, headers, body) = self
                .request(
                    "POST",
                    "/api/auth/login",
                    Some(json!({"username": username, "password": password})),
                    HeaderMap::new(),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "login failed: {body}");
            let csrf = body["csrfToken"]
                .as_str()
                .expect("csrf token in login response")
                .to_string();
            let cookie = headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .find(|value| value.starts_with("rast_session="))
                .and_then(|value| value.split(';').next())
                .expect("session cookie")
                .to_string();
            Session { cookie, csrf }
        }

        pub async fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<Value>,
            headers: HeaderMap,
        ) -> (StatusCode, HeaderMap, Value) {
            let mut builder = Request::builder().method(method).uri(path);
            for (key, value) in headers.iter() {
                builder = builder.header(key, value);
            }
            let body = match body {
                Some(value) => {
                    builder = builder.header(header::CONTENT_TYPE, "application/json");
                    Body::from(value.to_string())
                }
                None => Body::empty(),
            };
            let response = self
                .router
                .clone()
                .oneshot(builder.body(body).expect("request"))
                .await
                .expect("router response");
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("response body")
                .to_bytes();
            let value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            };
            (status, headers, value)
        }

        pub async fn get(&self, path: &str, session: &Session) -> (StatusCode, HeaderMap, Value) {
            self.request("GET", path, None, session.headers()).await
        }

        pub async fn get_anon(&self, path: &str) -> (StatusCode, HeaderMap, Value) {
            self.request("GET", path, None, HeaderMap::new()).await
        }

        /// Grant an app rule through a group the user belongs to.
        pub async fn grant_group_app(&self, user_id: i64, app_id: &str, rule: &str) {
            let now = crate::db::now_ms();
            let group_name = format!(
                "grant-{user_id}-{app_id}-{rule}-{}",
                uuid::Uuid::new_v4().simple()
            );
            let inserted = sqlx::query(
                "INSERT INTO groups (name, description, created_at, updated_at) VALUES (?, '', ?, ?)",
            )
            .bind(&group_name)
            .bind(now)
            .bind(now)
            .execute(&self.state.db)
            .await
            .expect("insert group");
            let group_id = inserted.last_insert_rowid();
            sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, ?)")
                .bind(group_id)
                .bind(user_id)
                .bind(now)
                .execute(&self.state.db)
                .await
                .expect("insert membership");
            sqlx::query("INSERT INTO group_app_rules (group_id, app_id, rule) VALUES (?, ?, ?)")
                .bind(group_id)
                .bind(app_id)
                .bind(rule)
                .execute(&self.state.db)
                .await
                .expect("insert app rule");
            crate::access::bump_generation();
        }

        /// Set a direct per-user app rule (explicit override).
        pub async fn set_user_app_rule(&self, user_id: i64, app_id: &str, rule: &str) {
            sqlx::query("INSERT INTO user_app_rules (user_id, app_id, rule) VALUES (?, ?, ?)")
                .bind(user_id)
                .bind(app_id)
                .bind(rule)
                .execute(&self.state.db)
                .await
                .expect("insert direct app rule");
            crate::access::bump_generation();
        }

        pub async fn insert_app(&self, id: &str, slug: &str) {
            self.insert_app_full(id, slug, "[]", "", "running").await;
        }

        /// Insert an app row with a JSON environment and `last_error` so
        /// confidentiality tests can assert exactly what is (not) returned.
        pub async fn insert_app_full(
            &self,
            id: &str,
            slug: &str,
            env_json: &str,
            last_error: &str,
            status: &str,
        ) {
            sqlx::query(
                "INSERT INTO apps (id, name, slug, compose, env_json, status, last_error, created_by, created_at, updated_at)
                 VALUES (?, ?, ?, 'services:\n  web:\n    image: nginx:alpine\n', ?, ?, ?, NULL, 1, 1)",
            )
            .bind(id)
            .bind(slug)
            .bind(slug)
            .bind(env_json)
            .bind(status)
            .bind(last_error)
            .execute(&self.state.db)
            .await
            .expect("insert app");
        }

        pub async fn insert_deployment(
            &self,
            app_id: &str,
            action: &str,
            status: &str,
            detail: &str,
        ) {
            sqlx::query(
                "INSERT INTO app_deployments (app_id, action, status, detail, created_at, finished_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(app_id)
            .bind(action)
            .bind(status)
            .bind(detail)
            .bind(crate::db::now_ms())
            .bind(crate::db::now_ms())
            .execute(&self.state.db)
            .await
            .expect("insert deployment");
        }

        /// Perform a GET request and hand back the raw (possibly streaming)
        /// response body so tests can read SSE frames.
        pub async fn raw_get(&self, path: &str, session: &Session) -> axum::response::Response {
            let mut builder = Request::builder().method("GET").uri(path);
            for (key, value) in session.headers().iter() {
                builder = builder.header(key, value);
            }
            self.router
                .clone()
                .oneshot(builder.body(Body::empty()).expect("request"))
                .await
                .expect("router response")
        }
    }
}

/// Group-policy enforcement tests for the Applications/Docker read surfaces.
#[cfg(test)]
mod access_policy_tests {
    use super::access_test_support::TestServer;
    use super::realtime_tests::{start_fake_worker, FakeWorkerMode};
    use super::MASKED_ENV_VALUE;
    use axum::http::StatusCode;
    use serde_json::json;

    const ADMIN_PASSWORD: &str = "Admin-Password-123!";
    const USER_PASSWORD: &str = "Alice-Password-123!";

    #[tokio::test]
    async fn denied_non_admin_reads_return_403() {
        let server = TestServer::start().await;
        server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        let alice = server.login("alice", USER_PASSWORD).await;
        for path in [
            "/api/apps",
            "/api/apps/app1",
            "/api/apps/app1/logs",
            "/api/apps/app1/logs/stream?tail=10",
            "/api/apps/app1/history",
            "/api/docker/status",
        ] {
            let (status, _, body) = server.get(path, &alice).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} -> {body}");
            assert_eq!(body["error"]["code"], "forbidden", "{path} -> {body}");
        }
    }

    #[tokio::test]
    async fn unauthenticated_reads_are_401() {
        let server = TestServer::start().await;
        let (status, _, _) = server.get_anon("/api/apps").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = server.get_anon("/api/docker/status").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    const ENV_JSON: &str = r#"[{"key":"PUBLIC_URL","secret":false,"value":"https://example.test"},{"key":"DB_PASSWORD","secret":true,"value":"v1:not-a-real-ciphertext"}]"#;
    const LAST_ERROR: &str =
        "deploy failed: exit code 1\nfull stderr with /var/lib/rast/apps/shop/secret.key";
    const DEPLOY_DETAIL: &str = "command failed: SECRET_TOKEN=abcd at /var/lib/rast/secret.key";

    #[tokio::test]
    async fn read_only_grant_receives_metadata_only() {
        let dir = tempfile::tempdir().expect("socket dir");
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "access log".into(),
            },
        );
        let server = TestServer::start_with_docker(&socket).await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "applications", "allow").await;
        server
            .insert_app_full("app1", "shop", ENV_JSON, LAST_ERROR, "error")
            .await;
        server
            .insert_deployment("app1", "deploy", "failed", DEPLOY_DETAIL)
            .await;
        let session = server.login("alice", USER_PASSWORD).await;

        // List: status/metadata only, env values replaced by counts.
        let (status, _, body) = server.get("/api/apps", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let item = &body["items"][0];
        assert!(item.get("env").is_none(), "env values leaked: {item}");
        assert!(item.get("compose").is_none(), "compose leaked: {item}");
        assert_eq!(item["envCount"], 2, "{item}");
        assert_eq!(item["secretCount"], 1, "{item}");
        assert_eq!(item["lastError"], "deploy failed: exit code 1", "{item}");
        assert!(!item.to_string().contains("not-a-real-ciphertext"));

        // Detail: same restriction, plus containers/resources metadata.
        let (status, _, body) = server.get("/api/apps/app1", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("env").is_none(), "env values leaked: {body}");
        assert!(body.get("compose").is_none(), "compose leaked: {body}");
        assert_eq!(body["envCount"], 2);
        assert_eq!(body["secretCount"], 1);
        assert_eq!(body["lastError"], "deploy failed: exit code 1");
        assert!(body.get("containers").is_some());

        // History: action/status/timestamps, no free-form detail.
        let (status, _, body) = server.get("/api/apps/app1/history", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let entry = &body["items"][0];
        assert!(
            entry.get("detail").is_none(),
            "history detail leaked: {entry}"
        );
        assert_eq!(entry["action"], "deploy");
        assert_eq!(entry["status"], "failed");
        assert!(entry["createdAt"].as_i64().is_some());

        // F2: container logs and the live log stream are administrator-only.
        let (status, _, body) = server.get("/api/apps/app1/logs?tail=10", &session).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body["error"]["message"],
            "container logs require an administrator"
        );
        let (status, _, body) = server
            .get("/api/apps/app1/logs/stream?tail=10", &session)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body["error"]["message"],
            "container logs require an administrator"
        );

        // Docker status stays readable but without host-path reason details.
        let (status, _, body) = server.get("/api/docker/status", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["available"], true);
        assert!(
            body["reason"].is_null(),
            "reason leaked host detail: {body}"
        );

        // Host-wide container discovery stays administrator-only (import flow).
        let (status, _, _) = server.get("/api/docker/containers", &session).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        worker.abort();
    }

    #[tokio::test]
    async fn admin_keeps_full_application_payload() {
        let dir = tempfile::tempdir().expect("socket dir");
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "admin log".into(),
            },
        );
        let server = TestServer::start_with_docker(&socket).await;
        server
            .create_user("admin", ADMIN_PASSWORD, crate::auth::Role::Admin)
            .await;
        server
            .insert_app_full("app1", "shop", ENV_JSON, LAST_ERROR, "error")
            .await;
        server
            .insert_deployment("app1", "deploy", "failed", DEPLOY_DETAIL)
            .await;
        let session = server.login("admin", ADMIN_PASSWORD).await;

        let (status, _, body) = server.get("/api/apps/app1", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("compose").is_some(), "admins keep compose: {body}");
        assert_eq!(body["env"][0]["value"], "https://example.test");
        assert_eq!(body["env"][1]["value"], MASKED_ENV_VALUE);
        assert!(
            body["lastError"].as_str().unwrap().contains("full stderr"),
            "admins keep full error text: {body}"
        );

        let (status, _, body) = server.get("/api/apps/app1/history", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["items"][0]["detail"], DEPLOY_DETAIL);

        let (status, _, body) = server.get("/api/apps/app1/logs?tail=10", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["lines"][0]["message"], "access-test log line");

        let (status, _, body) = server.get("/api/docker/status", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["reason"].as_str().is_some(),
            "admins keep reason: {body}"
        );

        worker.abort();
    }

    fn auth_user(id: i64, username: &str, role: crate::auth::Role) -> super::AuthUser {
        super::AuthUser {
            id,
            username: username.into(),
            display_name: username.into(),
            role,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    #[tokio::test]
    async fn read_only_list_hides_docker_socket_paths() {
        let server = TestServer::start().await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "applications", "allow").await;
        let session = server.login("alice", USER_PASSWORD).await;
        let (status, _, body) = server.get("/api/apps", &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["docker"]["available"], false);
        let reason = body["docker"]["reason"].as_str().unwrap_or_default();
        assert!(
            !reason.contains('/') && !reason.contains("socket") && !reason.contains("worker"),
            "read-only docker reason leaked host detail: {reason:?}"
        );
    }

    #[tokio::test]
    async fn applications_grant_controls_monitor_container_inventory() {
        use axum::extract::State;

        let server = TestServer::start().await;
        let allowed_id = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server
            .grant_group_app(allowed_id, "applications", "allow")
            .await;
        let denied_id = server
            .create_user("bob", "Bob-Password-123!", crate::auth::Role::User)
            .await;

        // Accounts allowed to use Applications keep the (possibly empty)
        // container inventory shape; others get it redacted.
        let allowed = auth_user(allowed_id, "alice", crate::auth::Role::User);
        let snapshot = crate::api::monitor::snapshot(State(server.state.clone()), allowed)
            .await
            .0;
        assert!(
            snapshot.get("containersRestricted").is_none(),
            "applications grant should keep container inventory: {snapshot}"
        );
        assert!(snapshot["containers"].is_array());

        let denied = auth_user(denied_id, "bob", crate::auth::Role::User);
        let snapshot = crate::api::monitor::snapshot(State(server.state.clone()), denied)
            .await
            .0;
        assert_eq!(snapshot["containers"], json!([]));
        assert_eq!(snapshot["containersRestricted"], json!(true));

        let denied = auth_user(denied_id, "bob", crate::auth::Role::User);
        let containers = crate::api::monitor::containers(State(server.state.clone()), denied)
            .await
            .0;
        assert_eq!(containers["containers"], json!([]));
        assert_eq!(containers["containersRestricted"], json!(true));
    }

    #[tokio::test]
    async fn log_stream_ends_when_admin_is_demoted() {
        use http_body_util::BodyExt;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("socket dir");
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "revocation log".into(),
            },
        );
        let server = TestServer::start_with_docker(&socket).await;
        let admin_id = server
            .create_user("admin", ADMIN_PASSWORD, crate::auth::Role::Admin)
            .await;
        server.insert_app("app1", "shop").await;
        let session = server.login("admin", ADMIN_PASSWORD).await;

        let response = server
            .raw_get("/api/apps/app1/logs/stream?tail=10", &session)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("initial SSE frame must arrive")
            .expect("stream must yield a frame")
            .expect("frame must be ok");
        let text = String::from_utf8_lossy(&first.into_data().expect("data frame")).into_owned();
        assert!(text.contains("app.logs"), "initial frame: {text}");

        // Demote the account between polls: F7 must close the stream.
        sqlx::query("UPDATE users SET role = 'user' WHERE id = ?")
            .bind(admin_id)
            .execute(&server.state.db)
            .await
            .expect("demote admin");

        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(frame) = body.frame().await {
                if frame.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "log stream kept delivering after the administrator was demoted"
        );
        worker.abort();
    }

    #[tokio::test]
    async fn explicit_none_blocks_the_group_grant() {
        let server = TestServer::start().await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "applications", "allow").await;
        server
            .set_user_app_rule(alice, "applications", "none")
            .await;
        let session = server.login("alice", USER_PASSWORD).await;
        let (status, _, body) = server.get("/api/apps", &session).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn read_only_grant_still_forbids_mutations() {
        let dir = tempfile::tempdir().expect("socket dir");
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "access log".into(),
            },
        );
        let server = TestServer::start_with_docker(&socket).await;
        let alice = server
            .create_user("alice", USER_PASSWORD, crate::auth::Role::User)
            .await;
        server.grant_group_app(alice, "applications", "allow").await;
        server.insert_app("app1", "shop").await;
        let session = server.login("alice", USER_PASSWORD).await;

        let cases: [(&str, &str, Option<serde_json::Value>); 5] = [
            (
                "POST",
                "/api/apps",
                Some(
                    json!({"name": "x", "compose": "services:\n  web:\n    image: nginx\n", "env": []}),
                ),
            ),
            ("POST", "/api/apps/app1/start", None),
            ("POST", "/api/apps/app1/stop", None),
            ("DELETE", "/api/apps/app1", None),
            (
                "POST",
                "/api/docker/import",
                Some(json!({"containerIds": ["0123456789ab"]})),
            ),
        ];
        for (method, path, body) in cases {
            let (status, _, value) = server
                .request(method, path, body, session.headers_with_csrf())
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} -> {value}");
        }

        worker.abort();
    }

    #[tokio::test]
    async fn admin_behaviour_is_unchanged() {
        let dir = tempfile::tempdir().expect("socket dir");
        let socket = dir.path().join("worker.sock");
        let worker = start_fake_worker(
            socket.clone(),
            FakeWorkerMode::Initial {
                message: "access log".into(),
            },
        );
        let server = TestServer::start_with_docker(&socket).await;
        server
            .create_user("admin", ADMIN_PASSWORD, crate::auth::Role::Admin)
            .await;
        server.insert_app("app1", "shop").await;
        let admin = server.login("admin", ADMIN_PASSWORD).await;

        let (status, _, body) = server.get("/api/apps", &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, body) = server.get("/api/docker/containers", &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, body) = server.get("/api/apps/app1", &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.get("compose").is_some(),
            "admins keep the editable Compose document: {body}"
        );
        let (status, _, body) = server
            .request(
                "POST",
                "/api/apps",
                Some(json!({
                    "name": "Admin App",
                    "compose": "services:\n  web:\n    image: nginx:alpine\n",
                    "env": [],
                })),
                admin.headers_with_csrf(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["jobId"].as_str().is_some(), "{body}");

        worker.abort();
    }
}
