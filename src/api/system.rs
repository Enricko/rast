//! System information, component health and public bootstrap endpoints.

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{auth::AdminUser, db, error::AppResult, state::AppState};

pub fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/api/system/info", get(system_info))
        .route("/api/system/components", get(components))
}

/// GET /healthz — minimal liveness probe for reverse proxies/monitoring.
pub async fn healthz(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query("SELECT 1").fetch_one(&state.db).await.is_ok();
    let mut response = if db_ok {
        (axum::http::StatusCode::OK, "ok").into_response()
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "degraded").into_response()
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub product: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub setup_required: bool,
    pub setup_token_required: bool,
    pub registration_enabled: bool,
    pub max_upload_bytes: u64,
    pub session_ttl_hours: i64,
    pub server_time: i64,
}

/// GET /api/bootstrap — unauthenticated metadata needed by the login screen.
pub async fn bootstrap(State(state): State<AppState>) -> Json<Bootstrap> {
    Json(Bootstrap {
        product: crate::NAME,
        description: crate::DESCRIPTION,
        version: crate::VERSION,
        setup_required: !db::has_admin(&state.db).await,
        setup_token_required: state.config.setup_token_required,
        registration_enabled: false,
        max_upload_bytes: state.config.max_upload_bytes,
        session_ttl_hours: state.config.session_ttl_hours,
        server_time: db::now_ms(),
    })
}

fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn hostname() -> String {
    let h = read_trimmed("/proc/sys/kernel/hostname");
    if h.is_empty() {
        "unknown".to_string()
    } else {
        h
    }
}

fn os_pretty_name() -> String {
    let text = read_trimmed("/etc/os-release");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
            return rest.trim_matches('"').to_string();
        }
    }
    "Linux".to_string()
}

fn meminfo() -> (u64, u64) {
    let text = read_trimmed("/proc/meminfo");
    let mut total = 0u64;
    let mut available = 0u64;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                * 1024;
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                * 1024;
        }
    }
    (total, available)
}

fn uptime_secs() -> u64 {
    read_trimmed("/proc/uptime")
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
        .unwrap_or(0)
}

/// Filesystem usage of the storage directory.
fn disk_usage(path: &std::path::Path) -> Value {
    match sysinfo::Disks::new_with_refreshed_list() {
        Ok(disks) => {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let mut best: Option<&sysinfo::Disk> = None;
            for disk in &disks {
                let mount = disk.mount_point();
                if canonical.starts_with(mount) {
                    match best {
                        Some(current)
                            if mount.as_os_str().len()
                                <= current.mount_point().as_os_str().len() => {}
                        _ => best = Some(disk),
                    }
                }
            }
            match best {
                Some(disk) => json!({
                    "available": true,
                    "mountPoint": disk.mount_point().display().to_string(),
                    "filesystem": disk.name().to_string_lossy(),
                    "totalBytes": disk.total_space(),
                    "availableBytes": disk.available_space(),
                    "usedBytes": disk.total_space().saturating_sub(disk.available_space()),
                    "kind": format!("{:?}", disk.kind()),
                    "removable": disk.is_removable(),
                }),
                None => json!({
                    "available": false,
                    "reason": "no filesystem matched the storage directory"
                }),
            }
        }
        Err(e) => json!({"available": false, "reason": format!("cannot read disks: {e}")}),
    }
}

fn db_file_size(path: &std::path::Path) -> Value {
    let main = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let wal = std::fs::metadata(format!("{}-wal", path.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    json!({"mainBytes": main, "walBytes": wal, "totalBytes": main + wal})
}

/// GET /api/system/info
pub async fn system_info(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Value>> {
    let (mem_total, mem_available) = meminfo();
    let users_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await?;
    let admin_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1")
            .fetch_one(&state.db)
            .await?;
    let db_size = db_file_size(&state.config.db_path());
    let app_dirs: i64 = std::fs::read_dir(state.config.apps_dir())
        .map(|d| d.count() as i64)
        .unwrap_or(0);

    Ok(Json(json!({
        "product": crate::NAME,
        "description": crate::DESCRIPTION,
        "version": crate::VERSION,
        "host": {
            "hostname": hostname(),
            "os": os_pretty_name(),
            "kernel": read_trimmed("/proc/sys/kernel/osrelease"),
            "arch": std::env::consts::ARCH,
            "cpuCount": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "memoryTotalBytes": mem_total,
            "memoryAvailableBytes": mem_available,
            "uptimeSecs": uptime_secs(),
        },
        "server": {
            "bind": state.config.bind.to_string(),
            "publicUrl": state.config.public_url,
            "dataDir": state.config.data_dir.display().to_string(),
            "storageDir": state.config.storage_dir.display().to_string(),
            "configFile": state.config.loaded_from.as_ref().map(|p| p.display().to_string()),
            "sessionTtlHours": state.config.session_ttl_hours,
            "monitorIntervalSecs": state.config.monitor_interval_secs,
            "maxUploadBytes": state.config.max_upload_bytes,
            "dockerEnabled": state.config.docker_enabled,
            "secureCookies": state.config.secure_cookies,
            "trustedProxies": state.config.trusted_proxies.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            "startedAt": state.started_at,
        },
        "counts": {
            "users": users_count,
            "admins": admin_count,
            "managedApps": app_dirs,
        },
        "database": db_size,
        "storage": disk_usage(&state.config.storage_dir),
        "dataDisk": disk_usage(&state.config.data_dir),
        "serverTime": db::now_ms(),
    })))
}

/// GET /api/system/components — health of internal components.
pub async fn components(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Value>> {
    let db_ok = sqlx::query("SELECT 1").fetch_one(&state.db).await.is_ok();
    let db_latency_ms = {
        let start = std::time::Instant::now();
        let _ = sqlx::query("SELECT 1").fetch_one(&state.db).await;
        start.elapsed().as_millis() as u64
    };
    let storage_ok = state.config.storage_dir.is_dir()
        && std::fs::metadata(&state.config.storage_dir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
    let data_ok = state.config.data_dir.is_dir();
    let monitor = state.monitor.latest();
    let monitor_fresh = state.monitor.is_fresh();
    let docker = state.docker.status().await;
    let jobs_running: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status = 'running'")
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    Ok(Json(json!({
        "components": [
            {"id": "database", "name": "Database", "status": if db_ok { "ok" } else { "error" },
             "detail": format!("SQLite, {db_latency_ms} ms ping")},
            {"id": "storage", "name": "Storage", "status": if storage_ok { "ok" } else { "error" },
             "detail": state.config.storage_dir.display().to_string()},
            {"id": "data", "name": "Data directory", "status": if data_ok { "ok" } else { "error" },
             "detail": state.config.data_dir.display().to_string()},
            {"id": "monitor", "name": "Resource monitor", "status": if monitor_fresh { "ok" } else { "warning" },
             "detail": if monitor_fresh { "sampling".to_string() } else { "no recent samples".to_string() },
             "latest": monitor},
            {"id": "docker", "name": "Docker worker", "status": if state.docker.enabled { "ok" } else { "disabled" },
             "detail": docker.get("reason").and_then(|v| v.as_str()).unwrap_or("optional").to_string()},
        ],
        "jobsRunning": jobs_running,
        "serverTime": db::now_ms(),
    })))
}

/// Helper for row decoding in other modules.
pub fn row_i64(row: &sqlx::sqlite::SqliteRow, col: &str) -> i64 {
    row.get::<i64, _>(col)
}
