//! Size limits of shared folders (ADR 0005, D3): `shares.quota_bytes`,
//! `NULL` for no limit (every share that existed before migration 0015).
//!
//! RAST checks a limit for File Station writes into the share (uploads,
//! copies and moves from outside it): the bytes the container holds plus
//! the incoming bytes must fit. The container's usage is measured by a walk
//! (apparent sizes, what the service can read), kept for a minute and
//! raised by the writes RAST accounts in between. Bytes that members' apps
//! write and members' own recycle bins (`.Trash-<uid>`, 0700) are not seen
//! by this check: with the RAST fallback the label is "RAST only", with
//! group quota on the shared filesystem "filesystem, best effort" (the
//! kernel limit is the rw group's, see `quota_sync` in the server).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::storage::{quota, shares, Storage};

/// How long a measured usage is trusted.
const MEASURE_TTL: Duration = Duration::from_secs(60);
/// Largest limit accepted (2^53, exact in JSON).
pub const MAX_SHARE_QUOTA: i64 = 1 << 53;
/// The 413 message of a write that does not fit a share's limit (in
/// process, and from a per-user file agent).
pub const LIMIT_EXCEEDED: &str = "the shared folder's size limit would be exceeded";

#[derive(Debug, Clone, Copy)]
struct Measured {
    bytes: u64,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Measured>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Measured>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Validate a requested limit: 0 or absent removes it.
pub fn normalize(quota_bytes: Option<i64>) -> AppResult<Option<i64>> {
    match quota_bytes {
        None | Some(0) => Ok(None),
        Some(bytes) if (1..=MAX_SHARE_QUOTA).contains(&bytes) => Ok(Some(bytes)),
        Some(_) => Err(AppError::bad_request(
            "quotaBytes must be 0 (no limit) or a positive number of bytes",
        )),
    }
}

/// The limit of a share, `None` for none.
pub async fn limit(pool: &SqlitePool, share_id: &str) -> AppResult<Option<i64>> {
    let limit: Option<Option<i64>> =
        sqlx::query_scalar("SELECT quota_bytes FROM shares WHERE id = ?")
            .bind(share_id)
            .fetch_optional(pool)
            .await?;
    Ok(limit.flatten().filter(|bytes| *bytes > 0))
}

/// The bytes a share's container holds (cached for a minute).
pub async fn used(storage: &Storage, share_id: &str, container: &str) -> AppResult<u64> {
    if let Some(found) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(share_id)
        .copied()
        .filter(|measured| measured.at.elapsed() < MEASURE_TTL)
    {
        return Ok(found.bytes);
    }
    let container = shares::validated_container(container)?;
    let path = storage.shared_root.join(container);
    let bytes = tokio::task::spawn_blocking(move || quota::dir_used_bytes_blocking(&path))
        .await
        .map_err(AppError::from)?;
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            share_id.to_string(),
            Measured {
                bytes,
                at: Instant::now(),
            },
        );
    Ok(bytes)
}

/// Bytes RAST wrote into a share since its last measurement.
pub fn note_written(share_id: &str, bytes: u64) {
    if let Some(measured) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_mut(share_id)
    {
        measured.bytes = measured.bytes.saturating_add(bytes);
    }
}

/// Forget a share's measurement (after deletions, or when it is removed).
pub fn forget(share_id: &str) {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(share_id);
}

/// The bytes still free below a share's limit, `None` when it has none.
pub async fn free(pool: &SqlitePool, storage: &Storage, share_id: &str) -> AppResult<Option<u64>> {
    let Some(limit) = limit(pool, share_id).await? else {
        return Ok(None);
    };
    let Some(record) = shares::find_by_id(pool, share_id).await? else {
        return Ok(None);
    };
    let used = used(storage, share_id, &record.container_path).await?;
    Ok(Some((limit as u64).saturating_sub(used)))
}

/// Refuse a write of `bytes` into a share over its limit (413). On success
/// the bytes count as written for the next check.
pub async fn check(
    pool: &SqlitePool,
    storage: &Storage,
    share_id: &str,
    bytes: u64,
) -> AppResult<()> {
    match free(pool, storage, share_id).await? {
        Some(free) if bytes > free => Err(AppError::PayloadTooLarge(LIMIT_EXCEEDED)),
        Some(_) => {
            note_written(share_id, bytes);
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_normalized() {
        assert_eq!(normalize(None).unwrap(), None);
        assert_eq!(normalize(Some(0)).unwrap(), None);
        assert_eq!(normalize(Some(5)).unwrap(), Some(5));
        assert!(normalize(Some(-1)).is_err());
        assert!(normalize(Some(MAX_SHARE_QUOTA + 1)).is_err());
    }

    #[test]
    fn written_bytes_raise_a_measurement_only() {
        let id = "00000000-0000-4000-8000-00000000abcd";
        note_written(id, 5);
        assert!(cache().lock().unwrap().get(id).is_none());
        cache().lock().unwrap().insert(
            id.into(),
            Measured {
                bytes: 10,
                at: Instant::now(),
            },
        );
        note_written(id, 5);
        assert_eq!(cache().lock().unwrap()[id].bytes, 15);
        forget(id);
        assert!(cache().lock().unwrap().get(id).is_none());
    }
}
