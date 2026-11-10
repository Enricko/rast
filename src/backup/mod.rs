//! Backup engine: consistent SQLite snapshots, configuration archives and
//! user-folder backups, with retention and restore planning.
//!
//! OWNER: lead. Placeholder to be replaced with the real implementation.

use std::path::Path;

use serde_json::{json, Value};

use crate::{config::Config, error::AppResult};

/// Create a backup archive; returns metadata about the created backup.
pub async fn create(
    _cfg: &Config,
    _scopes: &[String],
    _note: &str,
    _out_dir: Option<&Path>,
) -> AppResult<Value> {
    Ok(json!({"status": "not implemented"}))
}

/// Verify an existing backup archive without modifying anything.
pub async fn verify(_path: &Path) -> AppResult<Value> {
    Ok(json!({"ok": false, "reason": "backup engine not implemented"}))
}

/// Restore the database from an archive (offline operation).
pub async fn restore_database(_cfg: &Config, _path: &Path) -> AppResult<Value> {
    Ok(json!({"status": "not implemented"}))
}
