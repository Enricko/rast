//! Control-panel settings (admin), jobs and SSE endpoints.


use axum::{
    extract::State,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    audit::{self, AuditLog},
    auth::AdminUser,
    db,
    error::{AppError, AppResult},
    state::AppState,
};

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route("/api/settings/storage", get(storage_settings))
}

const WRITABLE_SETTINGS: &[&str] = &[
    "siteName",
    "loginMessage",
    "auditRetentionDays",
    "trashRetentionDays",
    "defaultQuotaBytes",
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    pub site_name: String,
    pub login_message: String,
    pub audit_retention_days: i64,
    pub trash_retention_days: i64,
    pub default_quota_bytes: i64,
    pub config: Value,
}

async fn read_setting(pool: &sqlx::SqlitePool, key: &str, default: &str) -> String {
    db::get_setting(pool, key)
        .await
        .unwrap_or_else(|| default.to_string())
}

/// GET /api/settings
pub async fn get_settings(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<SettingsDto>> {
    let site_name = read_setting(&state.db, "siteName", crate::NAME).await;
    let login_message = read_setting(&state.db, "loginMessage", "").await;
    let audit_retention_days = read_setting(&state.db, "auditRetentionDays", "90")
        .await
        .parse()
        .unwrap_or(90);
    let trash_retention_days = read_setting(&state.db, "trashRetentionDays", "30")
        .await
        .parse()
        .unwrap_or(30);
    let default_quota_bytes = read_setting(
        &state.db,
        "defaultQuotaBytes",
        &state.config.default_quota_bytes.to_string(),
    )
    .await
    .parse()
    .unwrap_or(state.config.default_quota_bytes as i64);

    Ok(Json(SettingsDto {
        site_name,
        login_message,
        audit_retention_days,
        trash_retention_days,
        default_quota_bytes,
        config: json!({
            "bind": state.config.bind.to_string(),
            "publicUrl": state.config.public_url,
            "dataDir": state.config.data_dir.display().to_string(),
            "storageDir": state.config.storage_dir.display().to_string(),
            "sessionTtlHours": state.config.session_ttl_hours,
            "monitorIntervalSecs": state.config.monitor_interval_secs,
            "maxUploadBytes": state.config.max_upload_bytes,
            "dockerEnabled": state.config.docker_enabled,
            "secureCookies": state.config.secure_cookies,
            "registrationEnabled": false,
            "readOnly": ["bind", "dataDir", "storageDir", "sessionTtlHours",
                         "monitorIntervalSecs", "maxUploadBytes", "dockerEnabled"],
        }),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsRequest {
    pub site_name: Option<String>,
    pub login_message: Option<String>,
    pub audit_retention_days: Option<i64>,
    pub trash_retention_days: Option<i64>,
    pub default_quota_bytes: Option<i64>,
}

/// PATCH /api/settings
pub async fn update_settings(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<UpdateSettingsRequest>,
) -> AppResult<Json<SettingsDto>> {
    let mut changed: Vec<&str> = Vec::new();
    if let Some(name) = body.site_name {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(AppError::bad_request("site name must be 1-64 characters"));
        }
        db::set_setting(&state.db, "siteName", name).await?;
        changed.push("siteName");
    }
    if let Some(msg) = body.login_message {
        if msg.chars().count() > 200 {
            return Err(AppError::bad_request("login message must be at most 200 characters"));
        }
        db::set_setting(&state.db, "loginMessage", msg.trim()).await?;
        changed.push("loginMessage");
    }
    if let Some(days) = body.audit_retention_days {
        if !(1..=3650).contains(&days) {
            return Err(AppError::bad_request(
                "audit retention must be between 1 and 3650 days",
            ));
        }
        db::set_setting(&state.db, "auditRetentionDays", &days.to_string()).await?;
        changed.push("auditRetentionDays");
    }
    if let Some(days) = body.trash_retention_days {
        if !(1..=3650).contains(&days) {
            return Err(AppError::bad_request(
                "trash retention must be between 1 and 3650 days",
            ));
        }
        db::set_setting(&state.db, "trashRetentionDays", &days.to_string()).await?;
        changed.push("trashRetentionDays");
    }
    if let Some(quota) = body.default_quota_bytes {
        if !(0..=1024i64.pow(4)).contains(&quota) {
            return Err(AppError::bad_request("invalid default quota"));
        }
        db::set_setting(&state.db, "defaultQuotaBytes", &quota.to_string()).await?;
        changed.push("defaultQuotaBytes");
    }
    audit::record(
        &state.db,
        AuditLog::new("settings.update")
            .actor(&admin.0)
            .object("settings", changed.join(","))
            .success(),
    )
    .await;
    get_settings(State(state), admin).await
}

/// GET /api/settings/storage — storage locations and filesystem capacity.
pub async fn storage_settings(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Value>> {
    let cfg = &state.config;
    let homes: Vec<String> = std::fs::read_dir(cfg.homes_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .take(1000)
                .collect()
        })
        .unwrap_or_default();
    let shared: Vec<String> = std::fs::read_dir(cfg.shared_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .take(1000)
                .collect()
        })
        .unwrap_or_default();
    let mut total_size: u64 = 0;
    for entry in walkdir::WalkDir::new(cfg.storage_dir.clone())
        .max_depth(12)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(Json(json!({
        "dataDir": cfg.data_dir.display().to_string(),
        "storageDir": cfg.storage_dir.display().to_string(),
        "homesDir": cfg.homes_dir().display().to_string(),
        "sharedDir": cfg.shared_dir().display().to_string(),
        "trashDir": cfg.trash_dir().display().to_string(),
        "homes": homes,
        "sharedFolders": shared,
        "usedBytes": total_size,
        "filesystem": filesystem_usage(&cfg.storage_dir),
    })))
}

/// Filesystem capacity for a path, resolved through the deepest matching mount.
pub fn filesystem_usage(path: &std::path::Path) -> Value {
    match sysinfo::Disks::new_with_refreshed_list() {
        Ok(disks) => {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let mut best: Option<&sysinfo::Disk> = None;
            for disk in &disks {
                let mount = disk.mount_point();
                if canonical.starts_with(mount)
                    && best.map(|b| mount.as_os_str().len() > b.mount_point().as_os_str().len()).unwrap_or(true)
                {
                    best = Some(disk);
                }
            }
            match best {
                Some(d) => json!({
                    "available": true,
                    "mountPoint": d.mount_point().display().to_string(),
                    "filesystem": d.name().to_string_lossy(),
                    "totalBytes": d.total_space(),
                    "availableBytes": d.available_space(),
                    "usedBytes": d.total_space().saturating_sub(d.available_space()),
                }),
                None => json!({"available": false, "reason": "no matching mount"}),
            }
        }
        Err(e) => json!({"available": false, "reason": format!("{e}")}),
    }
}
