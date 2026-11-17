//! Size limits of shared folders (ADR 0005, D3): `shares.quota_bytes`,
//! `NULL` for no limit (every share that existed before migration 0015).
//!
//! RAST checks a limit for File Station writes into the share (uploads,
//! copies and moves from outside it): the bytes the container holds, plus
//! the bytes of uploads still running, plus the incoming bytes must fit.
//! The container's usage is measured (apparent sizes), kept for a minute
//! and raised by the writes RAST accounts in between: the service walks
//! the container except the recycle bin folders at its top (`.Trash`,
//! `.Trash-<uid>`), and the helper measures those, which are private to
//! each member (`share_trash_usage`, through [`TrashMeasure`]). An upload
//! *reserves* its bytes for as long as it runs ([`ShareReservation`]), as
//! `quota::reserve` does for a user's quota, so two uploads that start
//! together cannot both use the same free space. Bytes that members' apps
//! write are not seen by this check: with the RAST fallback the label is
//! "RAST only", with group quota
//! on the shared filesystem "filesystem, best effort" (the kernel limit is
//! the rw group's, see `quota_sync` in the server).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::storage::{shares, xdg_trash, Storage};

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

/// Measures the bytes in the recycle bin folders at the top of a shared
/// folder's container (the server's helper; nothing in process, where the
/// service walk sees every byte it can read).
pub trait TrashMeasure: Send + Sync {
    /// The bytes, `None` when they cannot be measured now (counted as 0
    /// until the next measurement).
    fn trash_bytes(&self, share_id: &str, container: &str) -> BoxFuture<'static, Option<u64>>;
}

/// Where a [`Storage`] finds its [`TrashMeasure`], set once at start.
#[derive(Clone, Default)]
pub struct TrashMeasureSlot(Arc<OnceLock<Arc<dyn TrashMeasure>>>);

impl TrashMeasureSlot {
    /// Install the measure (later calls are ignored).
    pub fn set(&self, measure: Arc<dyn TrashMeasure>) {
        let _ = self.0.set(measure);
    }

    fn get(&self) -> Option<&Arc<dyn TrashMeasure>> {
        self.0.get()
    }
}

impl std::fmt::Debug for TrashMeasureSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TrashMeasureSlot")
            .field(&self.0.get().is_some())
            .finish()
    }
}

/// The bytes of the regular files in a container, without the recycle bin
/// folders at its top (the helper measures those) and without upload part
/// files. Symlinks are never followed.
fn container_bytes_blocking(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let walker = walkdir::WalkDir::new(path)
        .follow_links(false)
        .max_depth(64)
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_symlink()
                && !(entry.depth() == 1
                    && entry.file_type().is_dir()
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(xdg_trash::is_trash_dir_name))
        });
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file()
            || entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(crate::storage::TEMP_PREFIX))
        {
            continue;
        }
        total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
    }
    total
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

/// The bytes a share's container holds, its members' recycle bins
/// included (cached for a minute).
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
    let walked = tokio::task::spawn_blocking(move || container_bytes_blocking(&path))
        .await
        .map_err(AppError::from)?;
    let trash = match storage.share_trash.get() {
        Some(measure) => measure.trash_bytes(share_id, &container).await.unwrap_or(0),
        None => 0,
    };
    let bytes = walked.saturating_add(trash);
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

    struct FixedTrash(Option<u64>);

    impl TrashMeasure for FixedTrash {
        fn trash_bytes(&self, _: &str, container: &str) -> BoxFuture<'static, Option<u64>> {
            assert_eq!(container, "team");
            let bytes = self.0;
            Box::pin(async move { bytes })
        }
    }

    #[tokio::test]
    async fn the_members_recycle_bins_count_once_through_the_measure() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&crate::storage::tests::test_config(dir.path())).unwrap();
        let team = storage.shared_root.join("team");
        std::fs::create_dir_all(team.join(".Trash-200005/files")).unwrap();
        std::fs::create_dir_all(team.join("docs/.Trash")).unwrap();
        std::fs::write(team.join("plain"), [0u8; 4]).unwrap();
        std::fs::write(team.join("docs/.Trash/x"), [0u8; 8]).unwrap();
        // The measure counts this one (readable here, private on a host).
        std::fs::write(team.join(".Trash-200005/files/y"), [0u8; 100]).unwrap();
        // A file of that name at the top is the service's to count.
        std::fs::write(team.join(".Trash-200006"), [0u8; 16]).unwrap();
        std::fs::write(
            team.join(format!("{}part", crate::storage::TEMP_PREFIX)),
            [0u8; 32],
        )
        .unwrap();
        let id = "00000000-0000-4000-8000-00000000beef";
        assert_eq!(used(&storage, id, "team").await.unwrap(), 4 + 8 + 16);
        forget(id);
        storage.share_trash.set(Arc::new(FixedTrash(Some(100))));
        assert_eq!(used(&storage, id, "team").await.unwrap(), 4 + 8 + 16 + 100);
        // Cached for a minute.
        std::fs::write(team.join("later"), [0u8; 64]).unwrap();
        assert_eq!(used(&storage, id, "team").await.unwrap(), 128);
        forget(id);
        let other = Storage::new(&crate::storage::tests::test_config(dir.path())).unwrap();
        other.share_trash.set(Arc::new(FixedTrash(None)));
        // Not measurable now: the walk alone.
        assert_eq!(used(&other, id, "team").await.unwrap(), 4 + 8 + 16 + 64);
        forget(id);
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
