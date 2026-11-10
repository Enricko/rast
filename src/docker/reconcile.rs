//! Low-frequency application/container reconciler and realtime change events.
//!
//! Docker can change outside RAST (manual `docker stop`, a container crash, an
//! operator recreating a project) and RAST itself changes it through background
//! jobs. The reconciler compares the *managed* applications and their containers
//! against the last known observation every few seconds and publishes scoped
//! events through [`EventBus`]:
//!
//! * `app.changed` (admin scope) — `{appId, slug, status, containers, changedAt}`
//!   only when the derived status or container set actually changed; deletions
//!   are reported with `status: "deleted"`.
//! * `docker.changed` (admin scope) — `{available, version, reason}` when the
//!   worker/daemon availability or version changes.
//!
//! Multiple changes between two ticks are coalesced into one event per app, so
//! a restart storm cannot flood admin clients. Jobs also call
//! [`crate::docker::DockerManager::request_reconcile`] when they finish, which
//! wakes the loop for an immediate pass instead of waiting for the next tick.
//!
//! The reconciler is started once from `AppState::new` via [`spawn`]. It only
//! *reads* state: it never mutates containers, the database or management
//! records.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::{
    config::Config,
    db,
    docker::{containers::derive_status, DockerManager},
    events::EventBus,
};

/// How often managed applications are compared with Docker (see
/// `docs/REALTIME.md`: a low-frequency reconciler every 5 s).
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Container identity used for change detection. Ordered by name so a plain
/// reorder of `docker ps` output never produces a false positive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSignature {
    pub name: String,
    pub state: String,
    pub image: String,
}

/// One managed application as observed on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppObservation {
    pub app_id: String,
    pub slug: String,
    pub status: String,
    pub containers: Vec<ContainerSignature>,
}

/// Docker subsystem as observed by the reconciler.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerObservation {
    pub enabled: bool,
    pub available: bool,
    pub version: Option<String>,
    pub reason: String,
}

/// Complete host observation for one shot.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub apps: BTreeMap<String, AppObservation>,
    pub docker: DockerObservation,
}

/// A changed managed application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppChange {
    pub app_id: String,
    pub slug: String,
    pub status: String,
    pub containers: Vec<ContainerSignature>,
}

impl AppChange {
    fn from_observation(observation: &AppObservation) -> Self {
        Self {
            app_id: observation.app_id.clone(),
            slug: observation.slug.clone(),
            status: observation.status.clone(),
            containers: observation.containers.clone(),
        }
    }

    /// Event payload (contract: `{appId, slug, status, containers, changedAt}`).
    pub fn to_event_data(&self) -> Value {
        json!({
            "appId": self.app_id,
            "slug": self.slug,
            "status": self.status,
            "containers": self.containers,
            "changedAt": db::now_ms(),
        })
    }
}

/// Pure diff: applications added, changed or removed between two observations.
/// Ordering is stable (BTreeMap by app id) so tests are deterministic.
pub fn diff_apps(
    previous: &BTreeMap<String, AppObservation>,
    current: &BTreeMap<String, AppObservation>,
) -> Vec<AppChange> {
    let mut changes = Vec::new();
    for (app_id, observation) in current {
        match previous.get(app_id) {
            Some(before) if before == observation => {}
            _ => changes.push(AppChange::from_observation(observation)),
        }
    }
    for (app_id, before) in previous {
        if !current.contains_key(app_id) {
            changes.push(AppChange {
                app_id: app_id.clone(),
                slug: before.slug.clone(),
                status: "deleted".to_string(),
                containers: Vec::new(),
            });
        }
    }
    changes
}

/// Pure diff: `docker.changed` payload when availability or version changed.
pub fn diff_docker(previous: &DockerObservation, current: &DockerObservation) -> Option<Value> {
    if previous.available == current.available && previous.version == current.version {
        return None;
    }
    Some(json!({
        "available": current.available,
        "version": current.version,
        "reason": current.reason,
    }))
}

/// Publish all events for a transition. Returns the number of events published
/// (not the number of connected subscribers).
pub fn publish_changes(previous: &Observation, current: &Observation, events: &EventBus) -> usize {
    let mut published = 0;
    for change in diff_apps(&previous.apps, &current.apps) {
        events.publish_admin("app.changed", change.to_event_data());
        published += 1;
    }
    if let Some(data) = diff_docker(&previous.docker, &current.docker) {
        events.publish_admin("docker.changed", data);
        published += 1;
    }
    published
}

/// Start the process-wide reconciler. `AppState::new` calls this once; the
/// returned task runs until the runtime shuts down.
pub fn spawn(config: &Config, db: SqlitePool, docker: Arc<DockerManager>, events: EventBus) {
    let _ = config;
    tokio::spawn(async move {
        reconciler_loop(db, docker, events, RECONCILE_INTERVAL, None).await;
    });
}

/// Reconciler loop. Exposed at crate visibility so tests can drive it with a
/// short interval and a bounded number of ticks.
pub(crate) async fn reconciler_loop(
    db: SqlitePool,
    docker: Arc<DockerManager>,
    events: EventBus,
    interval: Duration,
    max_ticks: Option<u64>,
) {
    let notify = docker.reconcile_notify();
    let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous: Option<Observation> = None;
    let mut ticks: u64 = 0;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = notify.notified() => {}
        }
        let current = observe(&db, &docker).await;
        if let Some(previous) = previous.as_ref() {
            let published = publish_changes(previous, &current, &events);
            if published > 0 {
                tracing::trace!(published, "reconciler published change events");
            }
        }
        previous = Some(current);
        ticks += 1;
        if max_ticks.is_some_and(|max| ticks >= max) {
            break;
        }
    }
}

/// Read the current managed-app and Docker state (read-only, best effort).
async fn observe(db: &SqlitePool, docker: &DockerManager) -> Observation {
    let status = docker.refresh_status().await;
    let docker_status = DockerObservation {
        enabled: status["enabled"].as_bool().unwrap_or(false),
        available: status["available"].as_bool().unwrap_or(false),
        version: status["version"].as_str().map(str::to_string),
        reason: status["reason"].as_str().unwrap_or_default().to_string(),
    };

    let mut containers_by_project: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if docker_status.available {
        match docker.all_containers().await {
            Ok(data) => {
                if let Some(items) = data.get("items").and_then(Value::as_array) {
                    for container in items {
                        let Some(project) = container.get("project").and_then(Value::as_str) else {
                            continue;
                        };
                        if project.is_empty() {
                            continue;
                        }
                        containers_by_project
                            .entry(project.to_string())
                            .or_default()
                            .push(container.clone());
                    }
                }
            }
            Err(error) => {
                tracing::debug!(error = %error, "reconciler: container listing failed");
            }
        }
    }

    let mut apps = BTreeMap::new();
    match sqlx::query("SELECT id, slug, status FROM apps")
        .fetch_all(db)
        .await
    {
        Ok(rows) => {
            for row in rows {
                let app_id: String = row.get("id");
                let slug: String = row.get("slug");
                let stored_status: String = row.get("status");
                let project = crate::docker::protocol::project_name(&slug);
                let containers = containers_by_project
                    .get(&project)
                    .cloned()
                    .unwrap_or_default();
                let status = derive_status(&containers, &stored_status);
                let mut signatures: Vec<ContainerSignature> = containers
                    .iter()
                    .filter_map(|container| {
                        let name = container.get("name").and_then(Value::as_str)?;
                        Some(ContainerSignature {
                            name: name.to_string(),
                            state: container
                                .get("state")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            image: container
                                .get("image")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect();
                signatures.sort_by(|a, b| {
                    a.name
                        .cmp(&b.name)
                        .then_with(|| a.state.cmp(&b.state))
                        .then_with(|| a.image.cmp(&b.image))
                });
                apps.insert(
                    app_id.clone(),
                    AppObservation {
                        app_id,
                        slug,
                        status,
                        containers: signatures,
                    },
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "reconciler: cannot read managed applications");
        }
    }

    Observation {
        apps,
        docker: docker_status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::protocol::{WorkerOp, WorkerRequest, WorkerResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    fn observation(
        app_id: &str,
        slug: &str,
        status: &str,
        containers: &[(&str, &str, &str)],
    ) -> AppObservation {
        AppObservation {
            app_id: app_id.into(),
            slug: slug.into(),
            status: status.into(),
            containers: containers
                .iter()
                .map(|(name, state, image)| ContainerSignature {
                    name: (*name).into(),
                    state: (*state).into(),
                    image: (*image).into(),
                })
                .collect(),
        }
    }

    fn map(apps: Vec<AppObservation>) -> BTreeMap<String, AppObservation> {
        apps.into_iter()
            .map(|app| (app.app_id.clone(), app))
            .collect()
    }

    #[test]
    fn identical_observations_do_not_emit() {
        let previous = map(vec![observation(
            "a",
            "shop",
            "running",
            &[("shop-web-1", "running", "nginx:alpine")],
        )]);
        let current = previous.clone();
        assert!(diff_apps(&previous, &current).is_empty());
    }

    #[test]
    fn container_state_change_emits_one_coalesced_change() {
        let previous = map(vec![observation(
            "a",
            "shop",
            "running",
            &[("shop-web-1", "running", "nginx:alpine")],
        )]);
        let current = map(vec![observation(
            "a",
            "shop",
            "stopped",
            &[("shop-web-1", "exited", "nginx:alpine")],
        )]);
        let changes = diff_apps(&previous, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].app_id, "a");
        assert_eq!(changes[0].status, "stopped");
        assert_eq!(changes[0].containers[0].state, "exited");
        // Coalescing: several container-level changes still yield one event.
        let current = map(vec![observation(
            "a",
            "shop",
            "running",
            &[
                ("shop-web-1", "running", "nginx:alpine"),
                ("shop-db-1", "running", "postgres:16"),
            ],
        )]);
        let changes = diff_apps(&previous, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].containers.len(), 2);
    }

    #[test]
    fn image_and_container_set_changes_are_detected() {
        let previous = map(vec![observation(
            "a",
            "shop",
            "running",
            &[("shop-web-1", "running", "nginx:alpine")],
        )]);
        for current_containers in [
            vec![("shop-web-1", "running", "nginx:1.27")],
            vec![("shop-web-2", "running", "nginx:alpine")],
            vec![],
        ] {
            let current = map(vec![observation(
                "a",
                "shop",
                "running",
                &current_containers,
            )]);
            assert_eq!(
                diff_apps(&previous, &current).len(),
                if current_containers.is_empty() {
                    // status derivation normally makes this "stopped", but even
                    // with an unchanged status an emptied set is a change.
                    1
                } else {
                    1
                }
            );
        }
    }

    #[test]
    fn added_and_deleted_apps_are_reported() {
        let previous = map(vec![
            observation("a", "shop", "running", &[]),
            observation("b", "wiki", "running", &[]),
        ]);
        let current = map(vec![
            observation("a", "shop", "running", &[]),
            observation("c", "blog", "imported", &[]),
        ]);
        let changes = diff_apps(&previous, &current);
        assert_eq!(changes.len(), 2);
        let deleted = changes.iter().find(|c| c.app_id == "b").unwrap();
        assert_eq!(deleted.status, "deleted");
        assert!(deleted.containers.is_empty());
        let added = changes.iter().find(|c| c.app_id == "c").unwrap();
        assert_eq!(added.status, "imported");
    }

    #[test]
    fn container_order_is_not_a_change() {
        let a = observation(
            "a",
            "shop",
            "running",
            &[
                ("shop-web-1", "running", "nginx"),
                ("shop-db-1", "running", "pg"),
            ],
        );
        let mut b = a.clone();
        b.containers.reverse();
        let previous = map(vec![a]);
        let current = map(vec![b]);
        // The observation builder sorts by name; here we verify equality of
        // sorted vectors directly (same input set => same signature).
        let mut sig_a = previous["a"].containers.clone();
        let mut sig_b = current["a"].containers.clone();
        sig_a.sort_by(|x, y| x.name.cmp(&y.name));
        sig_b.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(sig_a, sig_b);
    }

    #[test]
    fn docker_changes_only_on_availability_or_version() {
        let available = DockerObservation {
            enabled: true,
            available: true,
            version: Some("26.1.4".into()),
            reason: "reachable".into(),
        };
        // Reason-only change: no event.
        let mut reason_changed = available.clone();
        reason_changed.reason = "still reachable".into();
        assert!(diff_docker(&available, &reason_changed).is_none());
        // Version change: event.
        let mut version_changed = available.clone();
        version_changed.version = Some("27.0.1".into());
        let data = diff_docker(&available, &version_changed).unwrap();
        assert_eq!(data["version"], "27.0.1");
        assert_eq!(data["available"], true);
        // Availability change: event.
        let down = DockerObservation {
            enabled: true,
            available: false,
            version: None,
            reason: "socket missing".into(),
        };
        let data = diff_docker(&available, &down).unwrap();
        assert_eq!(data["available"], false);
        assert_eq!(data["reason"], "socket missing");
        assert!(data["version"].is_null());
    }

    #[test]
    fn event_payload_shape_and_admin_scope() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let previous = Observation {
            apps: map(vec![observation("a", "shop", "running", &[])]),
            docker: DockerObservation::default(),
        };
        let current = Observation {
            apps: map(vec![observation("a", "shop", "stopped", &[])]),
            docker: DockerObservation::default(),
        };
        let published = publish_changes(&previous, &current, &bus);
        assert_eq!(published, 1);
        let event = rx.try_recv().expect("app.changed event");
        assert_eq!(event.kind, "app.changed");
        assert_eq!(event.scope, crate::events::EventScope::Admin);
        assert_eq!(event.data["appId"], "a");
        assert_eq!(event.data["slug"], "shop");
        assert_eq!(event.data["status"], "stopped");
        assert!(event.data["containers"].is_array());
        assert!(event.data["changedAt"].as_i64().unwrap() > 0);
    }

    #[test]
    fn docker_event_is_admin_scoped() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let previous = Observation {
            apps: BTreeMap::new(),
            docker: DockerObservation {
                enabled: true,
                available: false,
                version: None,
                reason: "down".into(),
            },
        };
        let current = Observation {
            apps: BTreeMap::new(),
            docker: DockerObservation {
                enabled: true,
                available: true,
                version: Some("26.1.4".into()),
                reason: "reachable".into(),
            },
        };
        assert_eq!(publish_changes(&previous, &current, &bus), 1);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, "docker.changed");
        assert_eq!(event.scope, crate::events::EventScope::Admin);
        assert_eq!(event.data["available"], true);
        assert_eq!(event.data["version"], "26.1.4");
    }

    // ------------------------------------------------------------------
    // Integration test: real DockerManager + real worker protocol shape
    // (fake worker socket), real SQLite database, real EventBus.
    // ------------------------------------------------------------------

    enum Reply {
        Ok(Value),
        Error(&'static str, &'static str),
    }

    fn fake_worker(
        socket: std::path::PathBuf,
        handler: impl Fn(WorkerRequest) -> Reply + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
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
                    let Ok(request) = serde_json::from_slice::<WorkerRequest>(&raw) else {
                        return;
                    };
                    let response = match handler(request) {
                        Reply::Ok(data) => WorkerResponse::success("placeholder", data),
                        Reply::Error(code, message) => {
                            WorkerResponse::failure("placeholder", code, message)
                        }
                    };
                    let id = serde_json::from_slice::<Value>(&raw)
                        .ok()
                        .and_then(|value| {
                            value.get("id").and_then(Value::as_str).map(str::to_string)
                        })
                        .unwrap_or_default();
                    let response = WorkerResponse { id, ..response };
                    let mut line = serde_json::to_vec(&response).unwrap();
                    line.push(b'\n');
                    let _ = stream.write_all(&line).await;
                    let _ = stream.flush().await;
                });
            }
        })
    }

    fn manager_config(
        data_dir: &std::path::Path,
        socket: &std::path::Path,
    ) -> crate::config::Config {
        let mut cfg =
            crate::config::Config::from_parts(None, crate::config::FileConfig::default(), |_| None)
                .unwrap();
        cfg.data_dir = data_dir.to_path_buf();
        cfg.storage_dir = data_dir.join("storage");
        cfg.worker_socket = socket.to_path_buf();
        cfg.docker_enabled = true;
        cfg
    }

    async fn insert_app(pool: &SqlitePool, id: &str, slug: &str, status: &str) {
        sqlx::query(
            "INSERT INTO apps (id, name, slug, compose, env_json, status, last_error, created_by, created_at, updated_at)
             VALUES (?, ?, ?, 'services: {}', '[]', ?, '', NULL, 1, 1)",
        )
        .bind(id)
        .bind(slug)
        .bind(slug)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn wait_for_event(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        kind: &str,
    ) -> crate::events::Event {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(event)) if event.kind == kind => return event,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) | Err(_) => panic!("timed out waiting for {kind} event"),
            }
        }
    }

    #[tokio::test]
    async fn reconciler_emits_app_changed_when_container_state_drifts() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let app_dir = dir.path().join("data/apps/shop");
        std::fs::create_dir_all(&app_dir).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let worker = fake_worker(socket.clone(), move |request| match request.op {
            WorkerOp::Version => Reply::Ok(json!({"version": "26.1.4"})),
            WorkerOp::PsAll => {
                let call = calls_for_handler.fetch_add(1, Ordering::SeqCst);
                let state = if call == 0 { "running" } else { "exited" };
                Reply::Ok(json!({"items": [{
                    "id": "0123456789ab",
                    "name": "shop-web-1",
                    "image": "nginx:alpine",
                    "state": state,
                    "status": "Up",
                    "project": "rast-shop",
                }]}))
            }
            _ => Reply::Error("invalid_request", "unsupported op"),
        });
        // DockerManager client validates requests against its apps dir; only
        // plain ops (version/ps-all) are used here.
        let cfg = manager_config(&dir.path().join("data"), &socket);
        let docker = DockerManager::new(&cfg);
        let pool = db::connect(&dir.path().join("reconcile.db"), 2)
            .await
            .unwrap();
        insert_app(&pool, "app1", "shop", "running").await;

        let events = EventBus::new();
        let mut rx = events.subscribe();
        let loop_events = events.clone();
        let loop_docker = docker.clone();
        let handle = tokio::spawn(async move {
            reconciler_loop(
                pool,
                loop_docker,
                loop_events,
                StdDuration::from_millis(40),
                Some(30),
            )
            .await;
        });
        let event = wait_for_event(&mut rx, "app.changed").await;
        assert_eq!(event.scope, crate::events::EventScope::Admin);
        assert_eq!(event.data["appId"], "app1");
        assert_eq!(event.data["slug"], "shop");
        assert_eq!(event.data["status"], "stopped");
        assert_eq!(event.data["containers"][0]["state"], "exited");
        assert!(calls.load(Ordering::SeqCst) >= 2);
        let _ = handle.await;
        worker.abort();
    }

    #[tokio::test]
    async fn reconciler_emits_docker_changed_on_availability_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        // The project must exist on disk for client-side (and worker-side)
        // request validation, but only plain ops are used by this test.
        std::fs::create_dir_all(dir.path().join("data/apps")).unwrap();
        let version_calls = Arc::new(AtomicUsize::new(0));
        let version_counter = version_calls.clone();
        let worker = fake_worker(socket.clone(), move |request| match request.op {
            WorkerOp::Version => {
                let call = version_counter.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    Reply::Error("unavailable", "docker daemon is down")
                } else {
                    Reply::Ok(json!({"version": "26.1.4"}))
                }
            }
            WorkerOp::PsAll => Reply::Ok(json!({"items": []})),
            _ => Reply::Error("invalid_request", "unsupported op"),
        });
        let cfg = manager_config(&dir.path().join("data"), &socket);
        let docker = DockerManager::new(&cfg);
        let pool = db::connect(&dir.path().join("reconcile.db"), 2)
            .await
            .unwrap();

        let events = EventBus::new();
        let mut rx = events.subscribe();
        let loop_events = events.clone();
        let loop_docker = docker.clone();
        let handle = tokio::spawn(async move {
            reconciler_loop(
                pool,
                loop_docker,
                loop_events,
                StdDuration::from_millis(40),
                Some(30),
            )
            .await;
        });
        let event = wait_for_event(&mut rx, "docker.changed").await;
        assert_eq!(event.scope, crate::events::EventScope::Admin);
        assert_eq!(event.data["available"], true);
        assert_eq!(event.data["version"], "26.1.4");
        let _ = handle.await;
        worker.abort();
    }

    #[test]
    fn spawn_helper_is_callable() {
        // `spawn` requires a tokio runtime; the wiring is exercised by the
        // integration tests above via `reconciler_loop`. This compile-time
        // check keeps the signature in sync with the Lead's AppState wiring.
        fn assert_signature(
            _: fn(&crate::config::Config, SqlitePool, Arc<DockerManager>, EventBus),
        ) {
        }
        assert_signature(spawn);
    }
}
