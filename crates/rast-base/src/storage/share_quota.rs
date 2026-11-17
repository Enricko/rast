//! Size limits of shared folders (ADR 0005, D3): `shares.quota_bytes`,
//! `NULL` for no limit (every share that existed before migration 0015).
//!
//! RAST checks a limit for File Station writes into the share (uploads,
//! copies and moves from outside it): the bytes the container holds, plus
//! the bytes of uploads still running, plus the incoming bytes must fit.
//! The container's usage is measured by a walk (apparent sizes, what the
//! service can read), kept for a minute and raised by the writes RAST
//! accounts in between. An upload *reserves* its bytes for as long as it
//! runs ([`ShareReservation`]), as `quota::reserve` does for a user's
//! quota, so two uploads that start together cannot both use the same free
//! space. Bytes that members' apps write and members' own recycle bins
//! (`.Trash-<uid>`, 0700) are not seen by this check: with the RAST
//! fallback the label is "RAST only", with group quota
//! on the shared filesystem "filesystem, best effort" (the kernel limit is
//! the rw group's, see `quota_sync` in the server).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
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

/// What RAST knows about one limited share's usage.
#[derive(Debug, Default)]
struct ShareUsage {
    /// The last walk of the container (plus what RAST wrote since).
    measured: Option<Measured>,
    /// Bytes of uploads that passed the check and have not finished.
    reserved: u64,
}

fn cache() -> MutexGuard<'static, HashMap<String, ShareUsage>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ShareUsage>>> = OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
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
        .get(share_id)
        .and_then(|usage| usage.measured)
        .filter(|measured| measured.at.elapsed() < MEASURE_TTL)
    {
        return Ok(found.bytes);
    }
    let container = shares::validated_container(container)?;
    let path = storage.shared_root.join(&container);
    let bytes = tokio::task::spawn_blocking(move || quota::dir_used_bytes_blocking(&path))
        .await
        .map_err(AppError::from)?;
    cache().entry(share_id.to_string()).or_default().measured = Some(Measured {
        bytes,
        at: Instant::now(),
    });
    Ok(bytes)
}

/// Bytes RAST wrote into a share since its last measurement.
pub fn note_written(share_id: &str, bytes: u64) {
    if let Some(measured) = cache()
        .get_mut(share_id)
        .and_then(|usage| usage.measured.as_mut())
    {
        measured.bytes = measured.bytes.saturating_add(bytes);
    }
}

/// Forget a share's measurement (after a limit change). Running uploads
/// keep their reservations.
pub fn forget(share_id: &str) {
    let mut cache = cache();
    let keep = cache.get_mut(share_id).is_some_and(|usage| {
        usage.measured = None;
        usage.reserved > 0
    });
    if !keep {
        cache.remove(share_id);
    }
}

/// The bytes still free below a share's limit, `None` when it has none:
/// the limit less the measured usage and the running uploads.
pub async fn free(pool: &SqlitePool, storage: &Storage, share_id: &str) -> AppResult<Option<u64>> {
    let Some(limit) = limit(pool, share_id).await? else {
        return Ok(None);
    };
    let Some(record) = shares::find_by_id(pool, share_id).await? else {
        return Ok(None);
    };
    let measured = used(storage, share_id, &record.container_path).await?;
    let taken = cache().get(share_id).map_or(measured, |usage| {
        usage
            .measured
            .map_or(measured, |m| m.bytes)
            .saturating_add(usage.reserved)
    });
    Ok(Some((limit as u64).saturating_sub(taken)))
}

/// Bytes an upload into a limited share holds while it runs (see
/// [`reserve`]). [`commit`](Self::commit) when the file is in place;
/// dropping it without that gives the bytes back (a failed, cancelled or
/// expired upload).
#[derive(Debug)]
pub struct ShareReservation {
    share_id: String,
    bytes: u64,
    settled: AtomicBool,
}

impl ShareReservation {
    /// The share the bytes are reserved in.
    pub fn share_id(&self) -> &str {
        &self.share_id
    }

    /// The bytes reserved.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The upload is in place: `written` bytes are in the share now (they
    /// count until the next measurement sees them).
    pub fn commit(&self, written: u64) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            self.settle();
            note_written(&self.share_id, written);
        }
    }

    fn settle(&self) {
        let mut cache = cache();
        if let Some(usage) = cache.get_mut(&self.share_id) {
            usage.reserved = usage.reserved.saturating_sub(self.bytes);
            if usage.reserved == 0 && usage.measured.is_none() {
                cache.remove(&self.share_id);
            }
        }
    }
}

impl Drop for ShareReservation {
    fn drop(&mut self) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            self.settle();
        }
    }
}

/// Reserve `bytes` of a share's limit for an upload, or refuse it (413).
/// `Ok(None)`: the share has no limit (nothing to reserve).
pub async fn reserve(
    pool: &SqlitePool,
    storage: &Storage,
    share_id: &str,
    bytes: u64,
) -> AppResult<Option<ShareReservation>> {
    let Some(limit) = limit(pool, share_id).await? else {
        return Ok(None);
    };
    let Some(record) = shares::find_by_id(pool, share_id).await? else {
        return Ok(None);
    };
    let measured = used(storage, share_id, &record.container_path).await?;
    // Check and reserve under one lock: the second of two uploads that
    // start together sees the first one's bytes.
    let mut cache = cache();
    let usage = cache.entry(share_id.to_string()).or_default();
    let current = usage.measured.map_or(measured, |m| m.bytes);
    let taken = current.saturating_add(usage.reserved);
    if taken.saturating_add(bytes) > limit as u64 {
        if usage.reserved == 0 && usage.measured.is_none() {
            cache.remove(share_id);
        }
        return Err(AppError::PayloadTooLarge(LIMIT_EXCEEDED));
    }
    usage.reserved = usage.reserved.saturating_add(bytes);
    Ok(Some(ShareReservation {
        share_id: share_id.to_string(),
        bytes,
        settled: AtomicBool::new(false),
    }))
}

/// Refuse a write of `bytes` into a share over its limit (413), counting
/// the uploads still running. On success the bytes count as written for
/// the next check (copies and moves, which write at once).
pub async fn check(
    pool: &SqlitePool,
    storage: &Storage,
    share_id: &str,
    bytes: u64,
) -> AppResult<()> {
    if let Some(reservation) = reserve(pool, storage, share_id, bytes).await? {
        reservation.commit(bytes);
    }
    Ok(())
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
        assert!(cache().get(id).is_none());
        cache().entry(id.into()).or_default().measured = Some(Measured {
            bytes: 10,
            at: Instant::now(),
        });
        note_written(id, 5);
        assert_eq!(cache()[id].measured.unwrap().bytes, 15);
        forget(id);
        assert!(cache().get(id).is_none());
    }
}
