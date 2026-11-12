//! Docker engine administration (admin only): every container on the host,
//! images and volumes — the "Docker Desktop" side of the Docker app.
//!
//! Managed applications keep their own lifecycle in [`super::apps`]; this
//! module works on raw Docker objects through the privileged worker.
//!
//! Safety rules enforced here (the worker re-validates every argument):
//! * admin role required for every endpoint;
//! * container ids, image references and volume names are validated before
//!   they reach the worker, which never runs a shell;
//! * removing a container never removes its volumes, and containers of a
//!   managed app can only be removed by deleting the app;
//! * removing a container or a volume requires its exact name as `confirm`;
//! * images and volumes are never force-removed: anything in use stays;
//! * every change is audited and announced with `docker.inventory`.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    extract::{Path as AxumPath, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    audit::{self, AuditLog},
    auth::AdminUser,
    docker::{
        exec::terminal_policy,
        protocol::{
            validate_container_id, validate_image_ref, validate_volume_name, WorkerOp,
            WorkerRequest, MAX_LOG_TAIL,
        },
        DockerError,
    },
    error::{AppError, AppResult},
    jobs::JobSpec,
    state::AppState,
};

/// Realtime event published after any change made through this module.
pub const INVENTORY_EVENT: &str = "docker.inventory";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/docker/containers/{id}",
            get(container_detail).delete(remove_container),
        )
        .route("/api/docker/containers/{id}/start", post(start_container))
        .route("/api/docker/containers/{id}/stop", post(stop_container))
        .route(
            "/api/docker/containers/{id}/restart",
            post(restart_container),
        )
        .route("/api/docker/containers/{id}/logs", get(container_logs))
        .route("/api/docker/stats", get(stats))
        .route("/api/docker/images", get(list_images))
        .route("/api/docker/images/pull", post(pull_image))
        .route("/api/docker/images/remove", post(remove_image))
        .route("/api/docker/images/prune", post(prune_images))
        .route("/api/docker/volumes", get(list_volumes))
        .route("/api/docker/volumes/remove", post(remove_volume))
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ConfirmQuery {
    pub confirm: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub tail: Option<u32>,
    /// Unix seconds: only lines at or after this time.
    pub since: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRequest {
    pub image: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRemoveRequest {
    pub name: String,
    pub confirm: String,
}

// ---------------------------------------------------------------------------
// Inventory helpers
// ---------------------------------------------------------------------------

/// Slugs of managed applications.
async fn managed_slugs(state: &AppState) -> AppResult<BTreeSet<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT slug FROM apps")
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect())
}

/// The managed app slug that owns a Compose project, if any.
pub(crate) fn managed_slug_for(project: &str, slugs: &BTreeSet<String>) -> Option<String> {
    if project.is_empty() {
        return None;
    }
    if let Some(slug) = project.strip_prefix("rast-") {
        if slugs.contains(slug) {
            return Some(slug.to_string());
        }
    }
    slugs.contains(project).then(|| project.to_string())
}

/// Every container on the host (`docker ps -a`).
pub(super) async fn all_containers(state: &AppState) -> AppResult<Vec<Value>> {
    let data = state
        .docker
        .all_containers()
        .await
        .map_err(AppError::from)?;
    Ok(data
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

pub(super) fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Find one container by full id or by a unique id prefix (at least 12
/// characters, like `docker ps` short ids).
pub(super) fn find_container<'a>(containers: &'a [Value], id: &str) -> AppResult<&'a Value> {
    validate_container_id(id).map_err(AppError::bad_request)?;
    if id.len() < 12 {
        return Err(AppError::bad_request(
            "container id must have at least 12 characters",
        ));
    }
    let id = id.to_ascii_lowercase();
    let mut matches = containers
        .iter()
        .filter(|container| str_field(container, "id").starts_with(&id));
    let found = matches
        .next()
        .ok_or(AppError::NotFound("container not found"))?;
    if matches.next().is_some() {
        return Err(AppError::bad_request("container id is ambiguous"));
    }
    Ok(found)
}

/// Map a worker failure for a user-requested change onto an HTTP error: a
/// refusal by Docker (image in use, container gone…) is a conflict the user
/// can act on, not a server outage.
pub(super) fn change_error(error: DockerError) -> AppError {
    match error {
        DockerError::Failed(message) => AppError::conflict(message),
        other => AppError::from(other),
    }
}

/// Parsed `docker inspect` data of one container.
pub(super) async fn inspect_container(state: &AppState, full_id: &str) -> AppResult<Value> {
    let data = state
        .docker
        .call(WorkerRequest::new(WorkerOp::Inspect).ids(vec![full_id.to_string()]))
        .await
        .map_err(AppError::from)?;
    data.get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
        .ok_or(AppError::NotFound("container not found"))
}

fn announce(state: &AppState, what: &str) {
    state
        .events
        .publish_admin(INVENTORY_EVENT, json!({"kind": what}));
    // App statuses follow their containers; refresh them right away.
    state.docker.request_reconcile();
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// GET /api/docker/containers/{id} — configuration of one container.
pub async fn container_detail(
    State(state): State<AppState>,
    _admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let containers = all_containers(&state).await?;
    let summary = find_container(&containers, &id)?.clone();
    let inspect = inspect_container(&state, str_field(&summary, "id")).await?;
    let slugs = managed_slugs(&state).await?;
    let slug = managed_slug_for(str_field(&summary, "project"), &slugs);
    let app_id: Option<String> = match &slug {
        Some(slug) => {
            sqlx::query_scalar("SELECT id FROM apps WHERE slug = ?")
                .bind(slug)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    };
    let (allowed, reason) = terminal_policy(&inspect);
    Ok(Json(json!({
        "container": summary,
        "inspect": inspect,
        "managed": slug.is_some(),
        "slug": slug,
        "appId": app_id,
        "terminal": {"allowed": allowed, "reason": reason},
    })))
}

async fn container_action(
    state: AppState,
    admin: AdminUser,
    id: String,
    op: WorkerOp,
    verb: &'static str,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let containers = all_containers(&state).await?;
    let container = find_container(&containers, &id)?;
    let full_id = str_field(container, "id").to_string();
    let name = str_field(container, "name").to_string();
    let outcome = state
        .docker
        .call(WorkerRequest::new(op).ids(vec![full_id.clone()]))
        .await;
    let action = format!("docker.container.{verb}");
    let entry = AuditLog::new(&action)
        .actor(&admin.0)
        .object("container", &full_id)
        .path(&name);
    match outcome {
        Ok(_) => {
            audit::record_with_events(&state.db, &state.events, entry.success()).await;
            announce(&state, "containers");
            Ok(Json(json!({"ok": true, "id": full_id, "name": name})))
        }
        Err(error) => {
            let error = change_error(error);
            audit::record_with_events(
                &state.db,
                &state.events,
                entry.failure().detail(error.public_message()),
            )
            .await;
            Err(error)
        }
    }
}

/// POST /api/docker/containers/{id}/start
pub async fn start_container(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    container_action(state, admin, id, WorkerOp::ContainerStart, "start").await
}

/// POST /api/docker/containers/{id}/stop
pub async fn stop_container(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    container_action(state, admin, id, WorkerOp::ContainerStop, "stop").await
}

/// POST /api/docker/containers/{id}/restart
pub async fn restart_container(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    container_action(state, admin, id, WorkerOp::ContainerRestart, "restart").await
}

/// DELETE /api/docker/containers/{id}?confirm=<container name>
///
/// Stops and removes the container. Its volumes always stay. Containers of a
/// managed app are refused: delete the app instead, so RAST's record and the
/// Compose project stay consistent.
pub async fn remove_container(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ConfirmQuery>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let containers = all_containers(&state).await?;
    let container = find_container(&containers, &id)?;
    let name = str_field(container, "name").to_string();
    let slugs = managed_slugs(&state).await?;
    if let Some(slug) = managed_slug_for(str_field(container, "project"), &slugs) {
        return Err(AppError::conflict(format!(
            "this container belongs to the app \"{slug}\"; delete the app instead"
        )));
    }
    if query.confirm.as_deref().map(str::trim) != Some(name.as_str()) {
        return Err(AppError::bad_request(
            "type the container name to confirm the deletion",
        ));
    }
    container_action(state, admin, id, WorkerOp::ContainerRemove, "remove").await
}

/// GET /api/docker/containers/{id}/logs?tail=&since=
pub async fn container_logs(
    State(state): State<AppState>,
    _admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let containers = all_containers(&state).await?;
    let container = find_container(&containers, &id)?;
    let full_id = str_field(container, "id").to_string();
    let tail = query.tail.unwrap_or(300).clamp(1, MAX_LOG_TAIL);
    let mut request = WorkerRequest::new(WorkerOp::ContainerLogs)
        .ids(vec![full_id])
        .tail(tail);
    if let Some(since) = query.since {
        if since < 0 {
            return Err(AppError::bad_request("since must not be negative"));
        }
        request = request.since_secs(since);
    }
    let data = state.docker.call(request).await.map_err(change_error)?;
    Ok(Json(data))
}

/// GET /api/docker/stats — one `docker stats` sample for every running
/// container.
pub async fn stats(State(state): State<AppState>, _admin: AdminUser) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let data = state
        .docker
        .container_stats()
        .await
        .map_err(AppError::from)?;
    Ok(Json(json!({
        "items": data.get("items").cloned().unwrap_or_else(|| json!([])),
        "sampledAt": crate::db::now_ms(),
    })))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Names of the containers that use an image. `docker ps` reports the image
/// as it was referenced at creation (`nginx`, `nginx:alpine`) or by id when
/// the tag has since moved.
fn image_users(image: &Value, containers: &[Value]) -> Vec<String> {
    let id = str_field(image, "id");
    let short = id.trim_start_matches("sha256:");
    let reference = str_field(image, "reference");
    let repository = str_field(image, "repository");
    let tag = str_field(image, "tag");
    let mut names: Vec<String> = containers
        .iter()
        .filter(|container| {
            let used = str_field(container, "image");
            if used.is_empty() {
                return false;
            }
            (!reference.is_empty() && used == reference)
                || (tag == "latest" && !repository.is_empty() && used == repository)
                || used == id
                || (used.len() >= 12 && short.starts_with(used.trim_start_matches("sha256:")))
        })
        .map(|container| str_field(container, "name").to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// GET /api/docker/images
pub async fn list_images(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let (images, containers) = tokio::join!(
        state.docker.call(WorkerRequest::new(WorkerOp::ImageList)),
        all_containers(&state),
    );
    let images = images.map_err(AppError::from)?;
    let containers = containers?;
    let mut total: u64 = 0;
    let items: Vec<Value> = images
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut image| {
            let users = image_users(&image, &containers);
            let count = image
                .get("containers")
                .and_then(Value::as_u64)
                .unwrap_or(users.len() as u64)
                .max(users.len() as u64);
            total =
                total.saturating_add(image.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0));
            if let Some(object) = image.as_object_mut() {
                object.insert("usedBy".into(), json!(users));
                object.insert("inUse".into(), json!(count > 0));
            }
            image
        })
        .collect();
    Ok(Json(json!({"items": items, "totalBytes": total})))
}

fn clean_image(value: &str) -> AppResult<String> {
    let image = value.trim();
    validate_image_ref(image).map_err(AppError::bad_request)?;
    Ok(image.to_string())
}

/// POST /api/docker/images/pull `{image}` — downloads in the background and
/// returns a job id.
pub async fn pull_image(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<ImageRequest>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let image = clean_image(&body.image)?;
    let job_state = state.clone();
    let job_image = image.clone();
    let job_actor = admin.0.clone();
    let spec = JobSpec::new("docker.pull", move |ctx| async move {
        ctx.progress(0, 1, "downloading").await;
        let outcome = job_state
            .docker
            .call(WorkerRequest::new(WorkerOp::ImagePull).image(&job_image))
            .await
            .map_err(change_error);
        // Audited when the download ends, so a failed pull is recorded as one.
        let entry = AuditLog::new("docker.image.pull")
            .actor(&job_actor)
            .object("image", &job_image);
        match outcome {
            Ok(data) => {
                audit::record_with_events(&job_state.db, &job_state.events, entry.success()).await;
                ctx.progress(1, 1, "done").await;
                announce(&job_state, "images");
                Ok(json!({"image": job_image, "digest": data.get("digest").cloned()}))
            }
            Err(error) => {
                audit::record_with_events(
                    &job_state.db,
                    &job_state.events,
                    entry.failure().detail(error.public_message()),
                )
                .await;
                Err(error)
            }
        }
    })
    .actor(&admin.0)
    .message(format!("Pulling {image}"));
    let job_id = state.jobs.submit(spec).await?;
    Ok(Json(json!({"jobId": job_id, "image": image})))
}

/// POST /api/docker/images/remove `{image}` — by reference (removes that tag,
/// and the image once no tag is left) or by id. Never forced.
pub async fn remove_image(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<ImageRequest>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let image = clean_image(&body.image)?;
    let outcome = state
        .docker
        .call(WorkerRequest::new(WorkerOp::ImageRemove).image(&image))
        .await;
    let entry = AuditLog::new("docker.image.remove")
        .actor(&admin.0)
        .object("image", &image);
    match outcome {
        Ok(data) => {
            audit::record_with_events(&state.db, &state.events, entry.success()).await;
            announce(&state, "images");
            Ok(Json(
                json!({"ok": true, "image": image, "output": data.get("output").cloned()}),
            ))
        }
        Err(error) => {
            let error = change_error(error);
            audit::record_with_events(
                &state.db,
                &state.events,
                entry.failure().detail(error.public_message()),
            )
            .await;
            Err(error)
        }
    }
}

/// POST /api/docker/images/prune — removes untagged images no container uses.
pub async fn prune_images(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let data = state
        .docker
        .call(WorkerRequest::new(WorkerOp::ImagePrune))
        .await
        .map_err(change_error)?;
    let deleted = data.get("deleted").and_then(Value::as_u64).unwrap_or(0);
    let reclaimed = str_field(&data, "reclaimed").to_string();
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("docker.image.prune")
            .actor(&admin.0)
            .success()
            .detail(format!("{deleted} removed, {reclaimed} freed")),
    )
    .await;
    announce(&state, "images");
    Ok(Json(json!({"deleted": deleted, "reclaimed": reclaimed})))
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

/// Volume name → names of the containers that mount it. `docker ps` lists a
/// named volume by its name and a bind mount by its host path.
fn volume_users(containers: &[Value]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for container in containers {
        let name = str_field(container, "name");
        let Some(mounts) = container.get("mounts").and_then(Value::as_array) else {
            continue;
        };
        for mount in mounts.iter().filter_map(Value::as_str) {
            if mount.starts_with('/') {
                continue;
            }
            let users = out.entry(mount.to_string()).or_default();
            if !users.iter().any(|existing| existing == name) {
                users.push(name.to_string());
            }
        }
    }
    out
}

/// GET /api/docker/volumes
pub async fn list_volumes(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let (volumes, containers) = tokio::join!(
        state.docker.call(WorkerRequest::new(WorkerOp::VolumeList)),
        all_containers(&state),
    );
    let volumes = volumes.map_err(AppError::from)?;
    let users = volume_users(&containers?);
    let slugs = managed_slugs(&state).await?;
    let items: Vec<Value> = volumes
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut volume| {
            let name = str_field(&volume, "name").to_string();
            let app = managed_slug_for(str_field(&volume, "project"), &slugs);
            let used_by = users.get(&name).cloned().unwrap_or_default();
            if let Some(object) = volume.as_object_mut() {
                object.insert("inUse".into(), json!(!used_by.is_empty()));
                object.insert("usedBy".into(), json!(used_by));
                object.insert("app".into(), json!(app));
            }
            volume
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

/// POST /api/docker/volumes/remove `{name, confirm}` — deletes the data in an
/// unused volume. Volumes of a managed app are removed with the app.
pub async fn remove_volume(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<VolumeRemoveRequest>,
) -> AppResult<Json<Value>> {
    state.docker.ensure_available().await?;
    let name = body.name.trim().to_string();
    validate_volume_name(&name).map_err(AppError::bad_request)?;
    if body.confirm.trim() != name {
        return Err(AppError::bad_request(
            "type the volume name to confirm the deletion",
        ));
    }
    let (volumes, containers) = tokio::join!(
        state.docker.call(WorkerRequest::new(WorkerOp::VolumeList)),
        all_containers(&state),
    );
    let volumes = volumes.map_err(AppError::from)?;
    let volume = volumes
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find(|v| str_field(v, "name") == name))
        .cloned()
        .ok_or(AppError::NotFound("volume not found"))?;
    if let Some(users) = volume_users(&containers?).get(&name) {
        return Err(AppError::conflict(format!(
            "this volume is used by {}; remove those containers first",
            users.join(", ")
        )));
    }
    let slugs = managed_slugs(&state).await?;
    if let Some(slug) = managed_slug_for(str_field(&volume, "project"), &slugs) {
        return Err(AppError::conflict(format!(
            "this volume holds the data of the app \"{slug}\"; delete the app with its data instead"
        )));
    }
    let outcome = state
        .docker
        .call(WorkerRequest::new(WorkerOp::VolumeRemove).volume(&name))
        .await;
    let entry = AuditLog::new("docker.volume.remove")
        .actor(&admin.0)
        .object("volume", &name);
    match outcome {
        Ok(_) => {
            audit::record_with_events(&state.db, &state.events, entry.success()).await;
            announce(&state, "volumes");
            Ok(Json(json!({"ok": true, "name": name})))
        }
        Err(error) => {
            let error = change_error(error);
            audit::record_with_events(
                &state.db,
                &state.events,
                entry.failure().detail(error.public_message()),
            )
            .await;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_slugs_match_compose_projects() {
        let slugs: BTreeSet<String> = ["shop".to_string()].into_iter().collect();
        assert_eq!(managed_slug_for("rast-shop", &slugs), Some("shop".into()));
        assert_eq!(managed_slug_for("shop", &slugs), Some("shop".into()));
        assert_eq!(managed_slug_for("rast-other", &slugs), None);
        assert_eq!(managed_slug_for("", &slugs), None);
    }

    #[test]
    fn containers_are_found_by_unique_prefix() {
        let containers = vec![
            json!({"id": "0123456789abcdef", "name": "one"}),
            json!({"id": "0123456789abffff", "name": "two"}),
        ];
        assert_eq!(
            str_field(
                find_container(&containers, "0123456789abc").unwrap(),
                "name"
            ),
            "one"
        );
        assert!(
            find_container(&containers, "0123456789ab").is_err(),
            "ambiguous"
        );
        assert!(find_container(&containers, "0123").is_err(), "too short");
        assert!(
            find_container(&containers, "fedcba987654").is_err(),
            "missing"
        );
        assert!(
            find_container(&containers, "one").is_err(),
            "names are not ids"
        );
    }

    #[test]
    fn image_users_match_reference_repository_and_id() {
        let containers = vec![
            json!({"name": "a", "image": "nginx:alpine"}),
            json!({"name": "b", "image": "redis"}),
            json!({"name": "c", "image": "sha256:cadade8215507a33088ee588bff19e2261e6f8c04416c6fa4629a75239e541f4"}),
            json!({"name": "d", "image": "postgres:17"}),
        ];
        let nginx = json!({"id": "sha256:713f", "reference": "nginx:alpine", "repository": "nginx", "tag": "alpine"});
        assert_eq!(image_users(&nginx, &containers), vec!["a"]);
        let redis = json!({"id": "sha256:0bf6", "reference": "redis:latest", "repository": "redis", "tag": "latest"});
        assert_eq!(image_users(&redis, &containers), vec!["b"]);
        let untagged = json!({"id": "sha256:cadade8215507a33088ee588bff19e2261e6f8c04416c6fa4629a75239e541f4", "reference": "", "repository": "", "tag": ""});
        assert_eq!(image_users(&untagged, &containers), vec!["c"]);
    }

    #[test]
    fn volume_users_skip_bind_mounts() {
        let containers = vec![
            json!({"name": "web", "mounts": ["shop_data", "/srv/shop/conf"]}),
            json!({"name": "worker", "mounts": ["shop_data"]}),
        ];
        let users = volume_users(&containers);
        assert_eq!(users.get("shop_data").unwrap(), &vec!["web", "worker"]);
        assert!(!users.contains_key("/srv/shop/conf"));
    }
}
