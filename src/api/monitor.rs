//! Resource monitor API: latest snapshot, bounded history series and container
//! resource samples.
//!
//! These endpoints are available to every authenticated user (the dashboard is
//! read-only monitoring data); no administrative role is required.

use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    monitor::{DEFAULT_HISTORY_POINTS, HISTORY_CAPACITY},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/monitor/snapshot", get(snapshot))
        .route("/api/monitor/history", get(history))
        .route("/api/monitor/containers", get(containers))
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub metric: Option<String>,
    pub points: Option<String>,
}

/// May this account see host-wide container inventory?
///
/// Only administrators and accounts allowed to use Applications may see
/// container names/ids; everyone else still gets host metrics (the monitor
/// itself stays available by default).
pub async fn can_see_containers(state: &AppState, user: &AuthUser) -> bool {
    if user.is_admin() {
        return true;
    }
    crate::access::app_access(&state.db, user, "applications")
        .await
        .map(|access| access.allowed)
        .unwrap_or(false)
}

/// Validate the requested history metric. Kept separate so it can be unit
/// tested without building an `AppState`.
pub fn parse_metric(metric: Option<&str>) -> AppResult<&'static str> {
    match metric.unwrap_or("cpu").trim().to_ascii_lowercase().as_str() {
        "cpu" => Ok("cpu"),
        "mem" | "memory" => Ok("mem"),
        "net" | "network" => Ok("net"),
        other => Err(AppError::bad_request(format!(
            "unsupported monitor metric {other:?} (expected cpu, mem or net)"
        ))),
    }
}

/// GET /api/monitor/snapshot — latest sample, sampling on demand if the
/// maintenance timer has not produced one yet.
pub async fn snapshot(State(state): State<AppState>, user: AuthUser) -> Json<Value> {
    let can_see_containers = can_see_containers(&state, &user).await;
    let snapshot = match state.monitor.latest() {
        Some(snapshot) => snapshot,
        None => state.monitor.sample().await,
    };
    Json(crate::monitor::redact_for(can_see_containers, &snapshot))
}

/// GET /api/monitor/history?metric=cpu|mem|net&points=120
pub async fn history(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Json<Value>> {
    let metric = parse_metric(query.metric.as_deref())?;
    let points = crate::api::parse_bounded(
        query.points.as_deref(),
        DEFAULT_HISTORY_POINTS,
        HISTORY_CAPACITY,
    );
    let series = state
        .monitor
        .history(metric, points)
        .await
        .ok_or_else(|| AppError::bad_request("unsupported monitor metric"))?;
    Ok(Json(series))
}

/// GET /api/monitor/containers — container resource samples. Returns an empty
/// list (with a reason) when Docker is disabled or the worker is unavailable.
pub async fn containers(State(state): State<AppState>, user: AuthUser) -> Json<Value> {
    let can_see_containers = can_see_containers(&state, &user).await;
    let mut body = state.monitor.containers();
    if let Some(snapshot) = state.monitor.latest() {
        if let Some(object) = body.as_object_mut() {
            object.insert("ts".to_string(), snapshot["ts"].clone());
            object.insert(
                "docker".to_string(),
                snapshot["components"]
                    .as_array()
                    .and_then(|components| {
                        components
                            .iter()
                            .find(|component| component["id"] == "docker")
                            .cloned()
                    })
                    .unwrap_or(Value::Null),
            );
        }
    }
    Json(crate::monitor::redact_for(can_see_containers, &body))
}

/// Small endpoint summary used by tests and documentation tooling.
pub fn endpoint_list() -> Value {
    json!([
        "GET /api/monitor/snapshot",
        "GET /api/monitor/history?metric=cpu|mem|net&points=120",
        "GET /api/monitor/containers"
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    fn test_config(dir: &std::path::Path) -> crate::config::Config {
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

    fn normal_user() -> AuthUser {
        AuthUser {
            id: 2,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: crate::auth::Role::User,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
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

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn metric_parsing_is_strict() {
        assert_eq!(parse_metric(None).unwrap(), "cpu");
        assert_eq!(parse_metric(Some("CPU")).unwrap(), "cpu");
        assert_eq!(parse_metric(Some("mem")).unwrap(), "mem");
        assert_eq!(parse_metric(Some("memory")).unwrap(), "mem");
        assert_eq!(parse_metric(Some("network")).unwrap(), "net");
        let err = parse_metric(Some("disk")).unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn snapshot_history_and_containers_respond() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();

        let response = snapshot(State(state.clone()), admin())
            .await
            .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = body_json(response).await;
        assert!(body["host"].is_object());
        assert!(body["host"]["cpuCores"].as_u64().unwrap() >= 1);
        assert_eq!(body["unavailable"], false);
        // Administrators keep the full container inventory payload.
        assert!(body.get("containersRestricted").is_none(), "{body}");

        // A regular account (no Applications grant) gets host metrics only.
        let response = snapshot(State(state.clone()), normal_user())
            .await
            .into_response();
        let body = body_json(response).await;
        assert!(body["host"].is_object());
        assert_eq!(body["containers"], json!([]));
        assert_eq!(body["containersRestricted"], json!(true));

        let response = history(
            State(state.clone()),
            admin(),
            Query(HistoryQuery {
                metric: Some("mem".into()),
                points: Some("5".into()),
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["metric"], "mem");
        assert!(!body["points"].as_array().unwrap().is_empty());

        let err = history(
            State(state.clone()),
            admin(),
            Query(HistoryQuery {
                metric: Some("gpu".into()),
                points: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);

        let response = containers(State(state.clone()), admin())
            .await
            .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["available"], false);
        assert!(body["containers"].as_array().unwrap().is_empty());
        assert!(body["reason"].as_str().is_some());
        assert!(body.get("containersRestricted").is_none(), "{body}");

        // Restricted accounts get an empty inventory plus the marker.
        let response = containers(State(state.clone()), normal_user())
            .await
            .into_response();
        let body = body_json(response).await;
        assert_eq!(body["containers"], json!([]));
        assert_eq!(body["containersRestricted"], json!(true));
    }
}
