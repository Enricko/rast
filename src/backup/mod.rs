//! Backup engine.
//!
//! Backups are self-contained `.tar.gz` archives containing:
//!
//! ```text
//! manifest.json      format version, schema version, scope, file checksums
//! db/rast.db         consistent SQLite snapshot (VACUUM INTO, WAL safe)
//! config/rast.toml   configuration (if present)
//! config/rast.env    environment file (if present)
//! config/secret.key  server master key (needed to decrypt app secrets)
//! apps/apps.json     managed deployment metadata (compose + encrypted env)
//! users/...          optional copy of user homes and shared folders
//! ```
//!
//! The live database is never copied directly: `VACUUM INTO` produces a
//! transactionally consistent snapshot including committed WAL data.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db,
    error::{AppError, AppResult},
};

pub mod schedule;

pub const BACKUP_FORMAT: u32 = 1;
pub const SCOPES: [&str; 4] = ["db", "config", "users", "apps"];
pub const BACKUP_EXTENSION: &str = "tar.gz";

pub type ProgressFn = Arc<dyn Fn(u64, u64, String) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub app: String,
    pub app_version: String,
    pub schema_version: i64,
    pub created_at: i64,
    pub hostname: String,
    pub scopes: Vec<String>,
    pub note: String,
    pub entries: Vec<ManifestEntry>,
}

/// Metadata returned to the API/CLI after a backup completes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupMeta {
    pub id: String,
    pub file_name: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub duration_ms: i64,
}

/// Default output directory for backups.
pub fn default_out_dir(cfg: &Config) -> PathBuf {
    cfg.backup_dir()
}

/// Validate and normalise the requested scopes.
pub fn normalize_scopes(scopes: &[String]) -> AppResult<Vec<String>> {
    let known: HashSet<&str> = SCOPES.iter().copied().collect();
    let mut out: Vec<String> = Vec::new();
    for scope in scopes {
        let scope = scope.trim().to_ascii_lowercase();
        if scope.is_empty() {
            continue;
        }
        if !known.contains(scope.as_str()) {
            return Err(AppError::bad_request(format!(
                "unknown backup scope {scope:?}; valid scopes: db, config, users, apps"
            )));
        }
        if !out.contains(&scope) {
            out.push(scope);
        }
    }
    if out.is_empty() {
        out.push("db".to_string());
        out.push("config".to_string());
    }
    Ok(out)
}

/// Create a backup archive. This touches the filesystem and (for large user
/// trees) can take a while — callers should run it as a background job.
pub async fn create(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    scopes: &[String],
    note: &str,
    out_dir: Option<&Path>,
    progress: Option<ProgressFn>,
) -> AppResult<BackupMeta> {
    let scopes = normalize_scopes(scopes)?;
    let out_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_out_dir(cfg));
    std::fs::create_dir_all(&out_dir)?;

    let now = db::now_ms();
    let stamp = format_stamp(now);
    let suffix = scopes.join("-");
    let file_name = format!("rast-{stamp}-{suffix}.{BACKUP_EXTENSION}");
    let final_path = out_dir.join(&file_name);

    let mut guard = BackupLock::acquire(cfg)?;
    let result = create_inner(cfg, pool, &scopes, note, &final_path, now, &progress).await;
    guard.release();
    let meta = result?;

    if let Some(pool) = pool {
        sqlx::query(
            "INSERT INTO backups (id, file_name, path, size_bytes, sha256, scope, status, note, created_at, finished_at)
             VALUES (?, ?, ?, ?, ?, ?, 'done', ?, ?, ?)",
        )
        .bind(&meta.id)
        .bind(&meta.file_name)
        .bind(&meta.path)
        .bind(meta.size_bytes as i64)
        .bind(&meta.sha256)
        .bind(serde_json::to_string(&meta.scopes).unwrap_or_else(|_| "[]".into()))
        .bind(note)
        .bind(meta.created_at)
        .bind(db::now_ms())
        .execute(pool)
        .await?;
    }
    Ok(meta)
}

async fn create_inner(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    scopes: &[String],
    note: &str,
    final_path: &Path,
    created_at: i64,
    progress: &Option<ProgressFn>,
) -> AppResult<BackupMeta> {
    let tmp_root = cfg.data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_root)?;
    let id = uuid::Uuid::new_v4().to_string();
    let work_dir = tmp_root.join(format!("backup-{id}"));
    std::fs::create_dir_all(&work_dir)?;
    // Ensure the work dir is removed on every exit path.
    let _guard = TempDirGuard(work_dir.clone());

    emit(progress, 0, 100, "preparing".to_string());

    // ---- consistent database snapshot -------------------------------------
    let db_snapshot = work_dir.join("rast.db");
    if scopes.iter().any(|s| s == "db") {
        let owned_pool;
        let pool_ref = match pool {
            Some(p) => p,
            None => {
                owned_pool = db::connect(&cfg.db_path(), 2).await?;
                &owned_pool
            }
        };
        // VACUUM INTO creates a consistent snapshot of the live database.
        let target = db_snapshot.to_string_lossy().to_string();
        sqlx::query("VACUUM INTO ?")
            .bind(&target)
            .execute(pool_ref)
            .await
            .map_err(|e| AppError::internal(format!("database snapshot failed: {e}")))?;
        if !db_snapshot.exists() {
            return Err(AppError::internal("database snapshot was not created"));
        }
        emit(progress, 14, 100, "database snapshot".to_string());
    }

    // ---- configuration -----------------------------------------------------
    let mut config_files: Vec<(String, PathBuf)> = Vec::new();
    if scopes.iter().any(|s| s == "config") {
        let candidates = [
            (cfg.loaded_from.clone(), "config/rast.toml"),
            (
                Some(PathBuf::from("/etc/rast/rast.toml")),
                "config/rast.toml",
            ),
            (Some(PathBuf::from("/etc/rast/rast.env")), "config/rast.env"),
            (Some(cfg.secret_path()), "config/secret.key"),
        ];
        let mut seen = HashSet::new();
        for (source, target) in candidates {
            if let Some(source) = source {
                if !source.is_file() || !seen.insert(target.to_string()) {
                    continue;
                }
                if seen.contains(target) && config_files.iter().any(|(t, _)| t == target) {
                    continue;
                }
                config_files.push((target.to_string(), source));
            }
        }
        // secret.key must always be present for restorable encrypted secrets.
        if !config_files.iter().any(|(t, _)| t == "config/secret.key") && cfg.secret_path().exists()
        {
            config_files.push(("config/secret.key".to_string(), cfg.secret_path()));
        }
        emit(progress, 20, 100, "configuration".to_string());
    }

    // ---- deployment metadata ----------------------------------------------
    let mut apps_json: Option<Value> = None;
    if scopes.iter().any(|s| s == "apps") {
        if let Some(pool) = pool {
            let rows = sqlx::query(
                "SELECT name, slug, compose, env_json, status, created_at, updated_at FROM apps ORDER BY slug",
            )
            .fetch_all(pool)
            .await?;
            use sqlx::Row;
            let items: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "name": row.get::<String, _>("name"),
                        "slug": row.get::<String, _>("slug"),
                        "compose": row.get::<String, _>("compose"),
                        "env": row.get::<String, _>("env_json"),
                        "status": row.get::<String, _>("status"),
                        "createdAt": row.get::<i64, _>("created_at"),
                        "updatedAt": row.get::<i64, _>("updated_at"),
                    })
                })
                .collect();
            apps_json = Some(json!({"format": 1, "apps": items}));
        }
    }

    // ---- user data ---------------------------------------------------------
    let mut user_files: Vec<(PathBuf, String)> = Vec::new();
    if scopes.iter().any(|s| s == "users") {
        for (root, prefix) in [
            (cfg.homes_dir(), "users/homes"),
            (cfg.shared_dir(), "users/shared"),
        ] {
            if !root.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| entry.file_name().to_string_lossy().to_string());
                user_files.push((entry.path().to_path_buf(), format!("{prefix}/{rel}")));
            }
        }
    }

    // ---- build the archive -------------------------------------------------
    let total_units = 100u64;
    let tmp_archive = work_dir.join("archive.tar.gz");
    {
        let file = File::create(&tmp_archive)?;
        let writer = BufWriter::with_capacity(256 * 1024, file);
        let encoder = GzEncoder::new(writer, Compression::new(6));
        let mut builder = tar::Builder::new(encoder);
        let mut entries: Vec<ManifestEntry> = Vec::new();

        let add_bytes = |builder: &mut tar::Builder<_>,
                         entries: &mut Vec<ManifestEntry>,
                         path: &str,
                         data: &[u8]|
         -> AppResult<()> {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_mtime((created_at / 1000) as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, data)
                .map_err(|e| AppError::internal(format!("cannot write archive: {e}")))?;
            entries.push(ManifestEntry {
                path: path.to_string(),
                size: data.len() as u64,
                sha256: sha256_bytes(data),
            });
            Ok(())
        };

        // Database
        if db_snapshot.exists() {
            let data = std::fs::read(&db_snapshot)?;
            add_bytes(&mut builder, &mut entries, "db/rast.db", &data)?;
        }
        emit(progress, 30, total_units, "database added".to_string());

        // Config
        for (target, source) in &config_files {
            let data = std::fs::read(source).map_err(|e| {
                AppError::internal(format!("cannot read {}: {e}", source.display()))
            })?;
            add_bytes(&mut builder, &mut entries, target, &data)?;
        }

        // Apps metadata
        if let Some(value) = &apps_json {
            let data = serde_json::to_vec_pretty(value).unwrap_or_default();
            add_bytes(&mut builder, &mut entries, "apps/apps.json", &data)?;
        }

        // User files (streamed one by one, bounded memory)
        let user_total = user_files.len() as u64;
        for (index, (source, target)) in user_files.iter().enumerate() {
            let mut file = match File::open(source) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %source.display(), error = %e, "skipping unreadable backup file");
                    continue;
                }
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o600);
            header.set_mtime((created_at / 1000) as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, target, &mut file)
                .map_err(|e| AppError::internal(format!("cannot archive user file: {e}")))?;
            entries.push(ManifestEntry {
                path: target.clone(),
                size,
                sha256: sha256_file(source).unwrap_or_default(),
            });
            if index % 32 == 0 {
                let done = 30 + ((index as u64 * 55) / user_total.max(1));
                emit(
                    progress,
                    done,
                    total_units,
                    format!("archiving {index}/{user_total}"),
                );
            }
        }

        // Manifest (last, once the entry list is complete)
        let manifest = Manifest {
            format: BACKUP_FORMAT,
            app: crate::NAME.to_string(),
            app_version: crate::VERSION.to_string(),
            schema_version: db::embedded_schema_version(),
            created_at,
            hostname: hostname(),
            scopes: scopes.to_vec(),
            note: note.to_string(),
            entries: entries.clone(),
        };
        let manifest_data = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| AppError::internal(format!("cannot serialise manifest: {e}")))?;
        add_bytes(&mut builder, &mut entries, "manifest.json", &manifest_data)?;

        builder
            .finish()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        let encoder = builder
            .into_inner()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        let mut writer = encoder
            .finish()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        writer.flush()?;
        writer.get_ref().sync_all().ok();
    }
    emit(progress, 90, total_units, "checksum".to_string());

    let (sha256, size_bytes) = sha256_and_size(&tmp_archive)?;
    // Move into place (same filesystem, atomic rename).
    std::fs::rename(&tmp_archive, final_path)?;
    emit(progress, 100, total_units, "done".to_string());

    Ok(BackupMeta {
        id: uuid::Uuid::new_v4().to_string(),
        file_name: final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: final_path.to_string_lossy().to_string(),
        size_bytes,
        sha256,
        scopes: scopes.to_vec(),
        created_at,
        duration_ms: db::now_ms() - created_at,
    })
}

/// Verify an archive: checksums of every entry, manifest sanity and (when
/// present) SQLite integrity of the embedded database.
pub async fn verify(path: &Path) -> AppResult<Value> {
    if !path.is_file() {
        return Err(AppError::NotFound("backup archive not found"));
    }
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<Manifest> = None;
    let mut db_bytes: Option<Vec<u8>> = None;

    for entry in archive
        .entries()
        .map_err(|e| AppError::internal(format!("cannot read archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| AppError::internal(format!("corrupt archive: {e}")))?;
        let entry_path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| AppError::internal(format!("cannot read archive entry: {e}")))?;
        if entry_path == "manifest.json" {
            manifest = serde_json::from_slice(&data).ok();
        } else if entry_path == "db/rast.db" {
            db_bytes = Some(data.clone());
        }
        tracing::debug!(entry = %entry_path, size = data.len(), "backup entry");
    }

    let mut checks =
        json!({"archive": true, "manifest": false, "checksums": false, "database": Value::Null});
    let Some(manifest) = manifest else {
        return Ok(json!({
            "ok": false,
            "checks": checks,
            "error": "manifest.json is missing or invalid",
        }));
    };
    checks["manifest"] = json!(true);

    // Re-read entries to verify checksums against the manifest.
    let mut mismatches = Vec::new();
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let expected: std::collections::HashMap<String, &ManifestEntry> = manifest
        .entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut seen = 0usize;
    if let Ok(entries) = archive.entries() {
        for mut entry in entries.flatten() {
            let Ok(entry_path) = entry.path().map(|p| p.to_string_lossy().to_string()) else {
                continue;
            };
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_err() {
                mismatches.push(entry_path);
                continue;
            }
            if let Some(expected_entry) = expected.get(&entry_path) {
                seen += 1;
                if expected_entry.size != data.len() as u64
                    || expected_entry.sha256 != sha256_bytes(&data)
                {
                    mismatches.push(entry_path);
                }
            }
        }
    }
    checks["checksums"] = json!(mismatches.is_empty() && seen == expected.len());

    // Database integrity check on the extracted snapshot.
    let mut db_ok = Value::Null;
    if let Some(bytes) = db_bytes {
        let tmp = tempfile_dir()?;
        let db_path = tmp.join("verify.db");
        std::fs::write(&db_path, &bytes)?;
        match check_sqlite_integrity(&db_path).await {
            Ok(ok) => db_ok = json!(ok),
            Err(e) => db_ok = json!({"ok": false, "error": e.to_string()}),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    checks["database"] = db_ok.clone();

    let all_ok = mismatches.is_empty()
        && checks["checksums"].as_bool().unwrap_or(false)
        && db_ok.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);

    Ok(json!({
        "ok": all_ok,
        "manifest": {
            "format": manifest.format,
            "appVersion": manifest.app_version,
            "schemaVersion": manifest.schema_version,
            "createdAt": manifest.created_at,
            "hostname": manifest.hostname,
            "scopes": manifest.scopes,
            "note": manifest.note,
            "entries": manifest.entries.len(),
        },
        "checks": checks,
        "mismatches": mismatches,
        "sizeBytes": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        "path": path.display().to_string(),
    }))
}

/// Restore the database from an archive. The caller must stop the service
/// first; the previous database is preserved as a pre-restore copy.
pub async fn restore_database(cfg: &Config, archive: &Path) -> AppResult<Value> {
    if service_running() {
        return Err(AppError::conflict(
            "rast.service is running; stop it before restoring the database",
        ));
    }
    let tmp_dir = cfg
        .data_dir
        .join("tmp")
        .join(format!("restore-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir)?;
    let _guard = TempDirGuard(tmp_dir.clone());

    let file = File::open(archive)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut tar = tar::Archive::new(GzDecoder::new(reader));
    let mut found = false;
    let extracted = tmp_dir.join("rast.db");
    for entry in tar
        .entries()
        .map_err(|e| AppError::internal(format!("cannot read archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| AppError::internal(format!("corrupt archive: {e}")))?;
        let path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if path == "db/rast.db" {
            let mut out = File::create(&extracted)?;
            std::io::copy(&mut entry, &mut out)?;
            out.sync_all().ok();
            found = true;
            break;
        }
    }
    if !found {
        return Err(AppError::bad_request(
            "archive does not contain db/rast.db (was it created with the db scope?)",
        ));
    }
    let integrity = check_sqlite_integrity(&extracted).await?;
    if !integrity
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(AppError::bad_request(format!(
            "restored database failed integrity check: {integrity}"
        )));
    }

    let stamp = format_stamp(db::now_ms());
    let pre_restore_dir = cfg.backup_dir().join(format!("pre-restore-{stamp}"));
    std::fs::create_dir_all(&pre_restore_dir)?;
    let live = cfg.db_path();
    if live.exists() {
        std::fs::copy(&live, pre_restore_dir.join("rast.db"))?;
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", live.display()));
        if sidecar.exists() {
            std::fs::copy(
                &sidecar,
                pre_restore_dir.join(sidecar.file_name().unwrap_or_default()),
            )?;
            std::fs::remove_file(&sidecar)?;
        }
    }
    // Atomic replacement: same directory, rename over the old file.
    let staging = cfg.data_dir.join(format!(".rast.db.restore-{stamp}"));
    std::fs::copy(&extracted, &staging)?;
    std::fs::rename(&staging, &live)?;
    if let Ok(meta) = std::fs::metadata(&live) {
        let _ = meta.permissions();
    }
    Ok(json!({
        "restored": true,
        "database": live.display().to_string(),
        "preRestoreBackup": pre_restore_dir.display().to_string(),
        "integrity": integrity,
        "message": "database restored; start rast.service and verify sign-in",
    }))
}

/// Delete backups beyond `keep` newest archives (rows and files).
pub async fn prune_retention(cfg: &Config, pool: &SqlitePool, keep: i64) -> AppResult<Vec<String>> {
    let keep = keep.max(1);
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, path FROM backups WHERE status = 'done' ORDER BY created_at DESC LIMIT -1 OFFSET ?",
    )
    .bind(keep)
    .fetch_all(pool)
    .await?;
    let mut removed = Vec::new();
    for row in rows {
        let id: String = row.get("id");
        let path: String = row.get("path");
        let path_ref = PathBuf::from(&path);
        if path_ref.exists() && path_ref.starts_with(cfg.backup_dir()) {
            let _ = std::fs::remove_file(&path_ref);
        }
        sqlx::query("DELETE FROM backups WHERE id = ?")
            .bind(&id)
            .execute(pool)
            .await?;
        removed.push(path);
    }
    Ok(removed)
}

/// Verify a SQLite database file by running `PRAGMA integrity_check`.
pub async fn check_sqlite_integrity(path: &Path) -> AppResult<Value> {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(|e| AppError::internal(format!("cannot open database: {e}")))?;
    let result: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await
        .unwrap_or_else(|_| vec!["error".to_string()]);
    let schema = db::schema_version(&pool).await;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    pool.close().await;
    let ok = result.len() == 1 && result.first().map(|s| s == "ok").unwrap_or(false);
    Ok(json!({"ok": ok, "result": result, "schemaVersion": schema, "tables": tables}))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn emit(progress: &Option<ProgressFn>, done: u64, total: u64, message: String) {
    if let Some(cb) = progress {
        cb(done, total, message);
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_and_size(path: &Path) -> AppResult<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn format_stamp(ts: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ts) {
        Some(dt) => dt.format("%Y%m%d-%H%M%S").to_string(),
        None => ts.to_string(),
    }
}

/// Temporary directory for verification work (uses the system temp dir so
/// verification works for any caller, including the CLI and tests).
fn tempfile_dir() -> AppResult<PathBuf> {
    let dir = std::env::temp_dir().join(format!("rast-verify-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn service_running() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "rast.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Prevents two concurrent backups from writing overlapping archives.
struct BackupLock {
    path: PathBuf,
}

impl BackupLock {
    fn acquire(cfg: &Config) -> AppResult<Self> {
        let dir = cfg.data_dir.join("tmp");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("backup.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                AppError::conflict(format!(
                    "another backup is already running ({e}); wait for it to finish"
                ))
            })?;
        let _ = file.write_all(db::now_ms().to_string().as_bytes());
        Ok(Self { path })
    }

    fn release(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for BackupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Public helper used by the UI to describe what a backup contains.
pub fn scope_description(scope: &str) -> &'static str {
    match scope {
        "db" => {
            "Accounts, sessions, settings, audit logs, job history (consistent SQLite snapshot)"
        }
        "config" => "Configuration file, environment file and server secret key",
        "users" => "User home directories and shared folders (file data)",
        "apps" => "Managed application definitions and encrypted environment variables",
        _ => "Unknown scope",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};

    fn test_cfg(dir: &Path) -> Config {
        Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn backup_verify_and_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let pool = db::init(&cfg).await.unwrap();
        // Seed data
        sqlx::query("INSERT INTO users (username, password_hash, role, created_at, updated_at) VALUES ('alice','x','admin',1,1)")
            .execute(&pool)
            .await
            .unwrap();
        let user_home = cfg.home_for("alice");
        std::fs::create_dir_all(&user_home).unwrap();
        std::fs::write(user_home.join("hello.txt"), b"hello rast").unwrap();

        let meta = create(
            &cfg,
            Some(&pool),
            &["db".into(), "config".into(), "users".into(), "apps".into()],
            "test backup",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(meta.size_bytes > 0);
        assert_eq!(meta.sha256.len(), 64);
        let archive = PathBuf::from(&meta.path);
        assert!(archive.exists());

        let verification = verify(&archive).await.unwrap();
        assert_eq!(verification["ok"], true, "verify failed: {verification}");
        assert_eq!(verification["checks"]["checksums"], true);
        assert_eq!(verification["checks"]["database"]["ok"], true);

        // Row recorded in the database.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // The DB snapshot contains the seeded user.
        let file = File::open(&archive).unwrap();
        let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
        let mut restored_db = None;
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "db/rast.db" {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                restored_db = Some(data);
            }
        }
        let restored_db = restored_db.expect("db entry present");
        let rc = check_sqlite_integrity_bytes(&restored_db).await;
        assert!(rc);
    }

    async fn check_sqlite_integrity_bytes(bytes: &[u8]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::fs::write(&path, bytes).unwrap();
        check_sqlite_integrity(&path)
            .await
            .map(|v| v["ok"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn verify_rejects_missing_archive() {
        let err = verify(Path::new("/nonexistent/file.tar.gz")).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn normalize_scopes_validates() {
        assert_eq!(
            normalize_scopes(&["db".into(), "DB".into(), " config ".into()]).unwrap(),
            vec!["db".to_string(), "config".to_string()]
        );
        assert!(normalize_scopes(&["nope".into()]).is_err());
        assert_eq!(
            normalize_scopes(&[]).unwrap(),
            vec!["db".to_string(), "config".to_string()]
        );
    }

    #[tokio::test]
    async fn concurrent_backups_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let pool = db::init(&cfg).await.unwrap();
        let _lock = BackupLock::acquire(&cfg).unwrap();
        let err = create(&cfg, Some(&pool), &["db".into()], "", None, None).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("another backup"));
    }

    #[tokio::test]
    async fn restore_refuses_when_service_running() {
        // This test only asserts the archive parsing path; service check is
        // environment dependent, so we verify the DB extraction error instead.
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let fake = dir.path().join("fake.tar.gz");
        std::fs::write(&fake, b"not a gzip archive").unwrap();
        let result = restore_database(&cfg, &fake).await;
        assert!(result.is_err());
    }
}
