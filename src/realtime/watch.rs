//! Bounded recursive filesystem watcher for File Station realtime updates.
//!
//! Only `<storage>/homes` and `<storage>/shared` are watched, recursively.
//! Raw `notify` events are debounced (~400 ms), deduplicated and rate-limited;
//! physical paths are mapped to logical File Station paths and scoped audiences
//! (home owner, or share owner + members) before anything is published. Host
//! paths never leave this module.
//!
//! Failures and bursts degrade to a broadcast `files.resync` (at most once per
//! two seconds) so clients refetch an authorized snapshot instead of receiving a
//! flood or a stale view.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use notify::{RecursiveMode, Watcher};
use sqlx::SqlitePool;

use crate::{
    config::Config,
    events::EventBus,
    realtime::{publish_file_changed, publish_resync, Audience},
    storage::{LogicalPath, TEMP_PREFIX},
};

/// Coalescing window for filesystem events.
pub const DEBOUNCE: Duration = Duration::from_millis(400);
/// How long username/share lookups are cached.
pub const CACHE_TTL: Duration = Duration::from_secs(30);
/// Minimum interval between two `files.resync` broadcasts.
pub const RESYNC_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// Maximum published events per wall-clock second.
pub const MAX_EVENTS_PER_SECOND: usize = 200;
/// Maximum paths buffered before falling back to a resync.
pub const MAX_PENDING_PATHS: usize = 4096;
/// Bounded channel between the OS watcher callback and the async task.
pub const CHANNEL_CAPACITY: usize = 4096;
/// How often the async loop checks debounce/overflow state.
const TICK: Duration = Duration::from_millis(100);
/// Paths per `file.changed` payload (keeps SSE frames bounded).
const MAX_PATHS_PER_EVENT: usize = 100;

/// Start the watcher task. Watches only the storage homes/shared directories.
pub fn spawn(cfg: &Config, db: SqlitePool, events: EventBus) {
    let homes = cfg.homes_dir();
    let shared = cfg.shared_dir();
    tokio::spawn(async move {
        if let Err(error) = run(homes, shared, db, events).await {
            tracing::warn!(error = %error, "filesystem watcher stopped");
        }
    });
}

async fn run(
    homes: PathBuf,
    shared: PathBuf,
    db: SqlitePool,
    events: EventBus,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Event>(CHANNEL_CAPACITY);
    let overflow = Arc::new(AtomicBool::new(false));

    let mut watcher = notify::recommended_watcher({
        let tx = tx.clone();
        let overflow = overflow.clone();
        let homes = homes.clone();
        let shared = shared.clone();
        move |result: notify::Result<notify::Event>| match result {
            Ok(event) => {
                if !event_relevant(&event, &homes, &shared) {
                    return;
                }
                // Never block the OS watcher thread: on a full channel record
                // an overflow and let the async loop emit a resync.
                if tx.try_send(event).is_err() {
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => tracing::debug!(error = %error, "filesystem watcher error"),
        }
    })?;

    if let Err(error) = watcher.watch(&homes, RecursiveMode::Recursive) {
        tracing::warn!(path = %homes.display(), error = %error, "cannot watch homes directory");
    }
    if let Err(error) = watcher.watch(&shared, RecursiveMode::Recursive) {
        tracing::warn!(path = %shared.display(), error = %error, "cannot watch shared directory");
    }

    let mut mapper = PathMapper::default();
    let mut debouncer = Debouncer::default();
    let mut rate = RateWindow::new(Instant::now());
    let mut resync = ResyncThrottle::default();
    let mut flush_at: Option<Instant> = None;
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { return Ok(()) };
                for path in event.paths {
                    debouncer.record(path);
                }
                flush_at = Some(Instant::now() + DEBOUNCE);
            }
            _ = ticker.tick() => {
                if overflow.swap(false, Ordering::Relaxed) || debouncer.overflowed() {
                    debouncer.clear();
                    flush_at = None;
                    if resync.allow(Instant::now()) {
                        publish_resync(&events, "overflow");
                    }
                    continue;
                }
                let due = flush_at.is_some_and(|deadline| Instant::now() >= deadline);
                if !due {
                    continue;
                }
                flush_at = None;
                let paths = debouncer.take();
                flush_batch(
                    paths,
                    &homes,
                    &shared,
                    &db,
                    &events,
                    &mut mapper,
                    &mut rate,
                    &mut resync,
                )
                .await;
            }
        }
    }
}

/// Ignore non-mutating access events (open/close/execute) — they would flood
/// the stream without changing any listing.
fn event_relevant(event: &notify::Event, homes: &Path, shared: &Path) -> bool {
    let touches_watched_root = event
        .paths
        .iter()
        .any(|path| path.starts_with(homes) || path.starts_with(shared));
    if !touches_watched_root {
        return false;
    }
    matches!(
        event.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Any
            | notify::EventKind::Other
    )
}

#[allow(clippy::too_many_arguments)]
async fn flush_batch(
    paths: Vec<PathBuf>,
    homes: &Path,
    shared: &Path,
    db: &SqlitePool,
    events: &EventBus,
    mapper: &mut PathMapper,
    rate: &mut RateWindow,
    resync: &mut ResyncThrottle,
) {
    {
        if paths.is_empty() {
            return;
        }
        mapper.ensure_fresh(db).await;
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        let mut unmapped: Vec<PathBuf> = Vec::new();
        for path in &paths {
            collect_mapped(mapper, path, homes, shared, &mut groups, &mut unmapped);
        }
        if !unmapped.is_empty() {
            // A new user or share may have appeared after the last cache load:
            // refresh once before skipping the path ("skip unknown mappings").
            if mapper.refresh(db).await.is_ok() {
                let retry = std::mem::take(&mut unmapped);
                for path in retry {
                    collect_mapped(mapper, &path, homes, shared, &mut groups, &mut unmapped);
                }
            }
        }
        let event_count: usize = groups
            .values()
            .map(|group| group.paths.len().div_ceil(MAX_PATHS_PER_EVENT))
            .sum();
        if event_count == 0 {
            return;
        }
        if !rate.allow(Instant::now(), event_count) {
            if resync.allow(Instant::now()) {
                publish_resync(events, "overflow");
            }
            return;
        }
        for (_, mut group) in groups {
            group.paths.sort();
            group.paths.dedup();
            for chunk in group.paths.chunks(MAX_PATHS_PER_EVENT) {
                publish_file_changed(
                    events,
                    &group.audience,
                    None,
                    "changed",
                    chunk,
                    None,
                    "external",
                );
            }
        }
    }
}

struct Group {
    audience: Audience,
    paths: Vec<String>,
}

fn collect_mapped(
    mapper: &PathMapper,
    path: &Path,
    homes: &Path,
    shared: &Path,
    groups: &mut BTreeMap<String, Group>,
    unmapped: &mut Vec<PathBuf>,
) {
    match mapper.map(path, homes, shared) {
        Some(mapped) => {
            let group = groups.entry(mapped.group_key).or_insert_with(|| Group {
                audience: mapped.audience,
                paths: Vec::new(),
            });
            group.paths.push(mapped.logical);
        }
        None => unmapped.push(path.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

/// Debounces and deduplicates physical paths between flushes.
#[derive(Debug, Default)]
pub struct Debouncer {
    pending: BTreeSet<PathBuf>,
    overflow: bool,
}

impl Debouncer {
    pub fn record(&mut self, path: PathBuf) {
        if self.pending.len() >= MAX_PENDING_PATHS && !self.pending.contains(&path) {
            self.overflow = true;
            return;
        }
        self.pending.insert(path);
    }

    /// Returns (and clears) the overflow flag.
    pub fn overflowed(&mut self) -> bool {
        std::mem::take(&mut self.overflow)
    }

    pub fn take(&mut self) -> Vec<PathBuf> {
        self.overflow = false;
        std::mem::take(&mut self.pending).into_iter().collect()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.overflow = false;
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Sliding one-second publish budget.
#[derive(Debug)]
pub struct RateWindow {
    window_start: Instant,
    published: usize,
}

impl RateWindow {
    pub fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            published: 0,
        }
    }

    pub fn allow(&mut self, now: Instant, count: usize) -> bool {
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.published = 0;
        }
        if self.published.saturating_add(count) > MAX_EVENTS_PER_SECOND {
            return false;
        }
        self.published = self.published.saturating_add(count);
        true
    }
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

/// At most one resync per [`RESYNC_MIN_INTERVAL`].
#[derive(Debug, Default)]
pub struct ResyncThrottle {
    last: Option<Instant>,
}

impl ResyncThrottle {
    pub fn allow(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            if now.saturating_duration_since(last) < RESYNC_MIN_INTERVAL {
                return false;
            }
        }
        self.last = Some(now);
        true
    }
}

/// Names that must never produce events (internal bookkeeping/temp files).
pub(crate) fn should_ignore_name(name: &str) -> bool {
    name == ".trash"
        || name.starts_with(TEMP_PREFIX)
        || name.starts_with(".rast-")
        || name.starts_with(".tmp-")
        || name.ends_with(".part")
}

#[derive(Debug, Clone)]
struct CachedShare {
    id: String,
    name: String,
    members: Vec<i64>,
}

#[derive(Debug)]
struct MappedPath {
    logical: String,
    audience: Audience,
    group_key: String,
}

/// Caches the (small) user and shared-folder tables so a burst of filesystem
/// events does not hammer SQLite.
#[derive(Debug, Default)]
struct PathMapper {
    /// lowercase username -> user id
    users: HashMap<String, i64>,
    /// `shares.container_path` -> cached share + audience
    shares: HashMap<String, CachedShare>,
    loaded_at: Option<Instant>,
}

impl PathMapper {
    async fn ensure_fresh(&mut self, pool: &SqlitePool) {
        let stale = self
            .loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= CACHE_TTL);
        if stale {
            if let Err(error) = self.refresh(pool).await {
                tracing::warn!(error = %error, "cannot refresh filesystem watcher mapping cache");
            }
        }
    }

    async fn refresh(&mut self, pool: &SqlitePool) -> Result<(), sqlx::Error> {
        use sqlx::Row;
        let user_rows = sqlx::query("SELECT id, username FROM users")
            .fetch_all(pool)
            .await?;
        let mut users = HashMap::with_capacity(user_rows.len());
        for row in user_rows {
            let username: String = row.get("username");
            users.insert(username.to_ascii_lowercase(), row.get("id"));
        }

        let share_rows = sqlx::query("SELECT id, name, container_path FROM shares")
            .fetch_all(pool)
            .await?;
        let mut shares = HashMap::with_capacity(share_rows.len());
        for row in share_rows {
            let id: String = row.get("id");
            let container_path: String = row.get("container_path");
            // The evaluator-filtered audience includes legacy members,
            // direct-rule holders and members of groups with folder rules
            // (explicit denials excluded). Share creators get no implicit
            // access (F6); administrators still receive Users-scoped events.
            let audience = match crate::storage::shares::member_ids(pool, &id).await {
                Ok(audience) => audience,
                Err(error) => {
                    tracing::warn!(share = %id, error = %error, "could not resolve share audience; skipping");
                    Vec::new()
                }
            };
            shares.insert(
                container_path,
                CachedShare {
                    id,
                    name: row.get("name"),
                    members: audience,
                },
            );
        }

        self.users = users;
        self.shares = shares;
        self.loaded_at = Some(Instant::now());
        Ok(())
    }

    fn map(&self, physical: &Path, homes: &Path, shared: &Path) -> Option<MappedPath> {
        if let Ok(relative) = physical.strip_prefix(homes) {
            return self.map_home(relative);
        }
        if let Ok(relative) = physical.strip_prefix(shared) {
            return self.map_shared(relative);
        }
        None
    }

    fn map_home(&self, relative: &Path) -> Option<MappedPath> {
        let mut components = relative.components();
        let Component::Normal(first) = components.next()? else {
            return None;
        };
        let username = first.to_str()?.to_ascii_lowercase();
        let user_id = *self.users.get(&username)?;
        let rest = normal_components(components)?;
        if rest.is_empty() || rest.iter().any(|name| should_ignore_name(name)) {
            return None;
        }
        let raw = format!("/{}", rest.join("/"));
        let logical = LogicalPath::parse(&raw).ok()?.as_string();
        Some(MappedPath {
            logical,
            audience: Audience::User(user_id),
            group_key: format!("u:{user_id}"),
        })
    }

    fn map_shared(&self, relative: &Path) -> Option<MappedPath> {
        let mut components = relative.components();
        let Component::Normal(first) = components.next()? else {
            return None;
        };
        let container = first.to_str()?;
        let share = self.shares.get(container)?;
        let rest = normal_components(components)?;
        if rest.is_empty() || rest.iter().any(|name| should_ignore_name(name)) {
            return None;
        }
        let raw = format!("/shared/{}/{}", share.name, rest.join("/"));
        let logical = LogicalPath::parse(&raw).ok()?.as_string();
        Some(MappedPath {
            logical,
            audience: Audience::Users(share.members.clone()),
            group_key: format!("s:{}", share.id),
        })
    }
}

fn normal_components<'a>(components: impl Iterator<Item = Component<'a>>) -> Option<Vec<&'a str>> {
    let mut parts = Vec::new();
    for component in components {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventScope;

    fn mapper() -> PathMapper {
        let mut users = HashMap::new();
        users.insert("alice".to_string(), 1_i64);
        users.insert("bob".to_string(), 2);
        let mut shares = HashMap::new();
        shares.insert(
            "team".to_string(),
            CachedShare {
                id: "share-team".to_string(),
                name: "Team Files".to_string(),
                members: vec![1, 2],
            },
        );
        PathMapper {
            users,
            shares,
            loaded_at: Some(Instant::now()),
        }
    }

    #[test]
    fn maps_home_paths_to_private_audiences() {
        let mapper = mapper();
        let homes = Path::new("/srv/rast/homes");
        let shared = Path::new("/srv/rast/shared");
        let mapped = mapper
            .map(&homes.join("alice/docs/report.txt"), homes, shared)
            .unwrap();
        assert_eq!(mapped.logical, "/docs/report.txt");
        assert_eq!(mapped.audience, Audience::User(1));
        assert_eq!(mapped.group_key, "u:1");

        // Home root itself, unknown users and ignored names are skipped.
        assert!(mapper.map(&homes.join("alice"), homes, shared).is_none());
        assert!(mapper
            .map(&homes.join("nobody/docs/a.txt"), homes, shared)
            .is_none());
        assert!(mapper
            .map(&homes.join("alice/.rast-tmp-part"), homes, shared)
            .is_none());
        assert!(mapper
            .map(&homes.join("alice/.trash/old.txt"), homes, shared)
            .is_none());
        assert!(mapper
            .map(&homes.join("alice/docs/file.part"), homes, shared)
            .is_none());
        // A path outside both roots is never mapped.
        assert!(mapper
            .map(Path::new("/etc/passwd"), homes, shared)
            .is_none());
    }

    #[test]
    fn maps_shared_paths_to_member_audiences() {
        let mapper = mapper();
        let homes = Path::new("/srv/rast/homes");
        let shared = Path::new("/srv/rast/shared");
        let mapped = mapper
            .map(&shared.join("team/docs/budget.xlsx"), homes, shared)
            .unwrap();
        assert_eq!(mapped.logical, "/shared/Team Files/docs/budget.xlsx");
        assert_eq!(mapped.audience, Audience::Users(vec![1, 2]));
        assert_eq!(mapped.group_key, "s:share-team");
        assert!(mapper.map(&shared.join("team"), homes, shared).is_none());
        assert!(mapper
            .map(&shared.join("unknown-container/x.txt"), homes, shared)
            .is_none());
        assert!(mapper
            .map(&shared.join("team/.tmp-123"), homes, shared)
            .is_none());
    }

    #[test]
    fn trigger_names_are_ignored() {
        for name in [
            ".trash",
            ".rast-tmp-abc",
            ".rast-upload",
            ".tmp-file",
            "download.part",
        ] {
            assert!(should_ignore_name(name), "{name} should be ignored");
        }
        for name in ["report.txt", "Photos", ".hidden-but-real"] {
            assert!(!should_ignore_name(name), "{name} should be watched");
        }
    }

    #[test]
    fn debouncer_coalesces_and_bounds() {
        let mut debouncer = Debouncer::default();
        debouncer.record(PathBuf::from("/srv/rast/homes/alice/a.txt"));
        debouncer.record(PathBuf::from("/srv/rast/homes/alice/a.txt"));
        debouncer.record(PathBuf::from("/srv/rast/homes/alice/b.txt"));
        assert_eq!(debouncer.len(), 2);
        assert!(!debouncer.overflowed());
        let batch = debouncer.take();
        assert_eq!(batch.len(), 2);
        assert!(debouncer.is_empty());
        assert!(!debouncer.overflowed());

        // Exceeding the pending cap flags an overflow instead of growing.
        let mut debouncer = Debouncer::default();
        for index in 0..=MAX_PENDING_PATHS {
            debouncer.record(PathBuf::from(format!("/srv/rast/homes/alice/f{index}")));
        }
        assert!(debouncer.overflowed());
        assert_eq!(debouncer.len(), MAX_PENDING_PATHS);
        debouncer.clear();
        assert!(debouncer.is_empty());
        assert!(!debouncer.overflowed());
    }

    #[test]
    fn rate_window_limits_and_resets() {
        let start = Instant::now();
        let mut rate = RateWindow::new(start);
        assert!(rate.allow(start, MAX_EVENTS_PER_SECOND));
        assert!(!rate.allow(start, 1));
        assert!(rate.allow(start + Duration::from_millis(900), 0));
        assert!(!rate.allow(start + Duration::from_millis(900), 1));
        // A new one-second window opens again.
        assert!(rate.allow(start + Duration::from_millis(1_100), MAX_EVENTS_PER_SECOND));
    }

    #[test]
    fn resync_is_throttled_to_one_per_two_seconds() {
        let start = Instant::now();
        let mut throttle = ResyncThrottle::default();
        assert!(throttle.allow(start));
        assert!(!throttle.allow(start + Duration::from_millis(500)));
        assert!(!throttle.allow(start + Duration::from_millis(1_999)));
        assert!(throttle.allow(start + Duration::from_millis(2_000)));
    }

    fn test_config(dir: &Path) -> Config {
        crate::config::Config::from_parts(
            None,
            crate::config::FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                monitor_interval_secs: Some(300),
                ..Default::default()
            },
            |_| None,
        )
        .expect("test config")
    }

    async fn wait_for_event(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        file: &Path,
        content: &[u8],
        expected_path: &str,
    ) -> Option<crate::events::Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _ = tokio::fs::write(file, content).await;
            let caught = tokio::time::timeout(Duration::from_millis(500), async {
                loop {
                    match rx.recv().await {
                        Ok(event)
                            if event.kind == crate::realtime::FILE_CHANGED
                                && event.data["source"] == "external"
                                && event.data["path"] == expected_path =>
                        {
                            return Some(event);
                        }
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return None,
                    }
                }
            })
            .await;
            if let Ok(Some(event)) = caught {
                return Some(event);
            }
            if Instant::now() >= deadline {
                return None;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_publishes_scoped_external_events() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        cfg.ensure_dirs().expect("storage directories");
        let pool = crate::db::connect(&dir.path().join("watch.db"), 4)
            .await
            .unwrap();
        for (id, username) in [(1_i64, "alice"), (2, "bob")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', 'user', 1, 1)",
            )
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .unwrap();
            std::fs::create_dir_all(cfg.homes_dir().join(username)).unwrap();
        }
        std::fs::create_dir_all(cfg.shared_dir().join("team")).unwrap();
        sqlx::query(
            "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
             VALUES ('share-team', 'team', '', 'team', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES ('share-team', 2, 'write')")
            .execute(&pool)
            .await
            .unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        spawn(&cfg, pool.clone(), bus.clone());
        // The cache starts empty; the watcher refreshes it on the first flush.
        let _ = &pool;

        // Home change -> private event for alice only.
        let home_file = cfg.homes_dir().join("alice/note.txt");
        let event = wait_for_event(&mut rx, &home_file, b"hello", "/note.txt")
            .await
            .expect("watcher did not publish a home event");
        assert_eq!(event.scope, EventScope::User(1));
        assert_eq!(event.data["op"], "changed");
        assert_eq!(event.data["paths"], serde_json::json!(["/"]));
        assert_eq!(event.data["userId"], 1);
        assert!(!event.scope.allows(2, false));
        assert!(!event.scope.allows(9, true));

        // Shared change -> member event (creator has no implicit access, F6).
        let shared_file = cfg.shared_dir().join("team/doc.txt");
        let event = wait_for_event(&mut rx, &shared_file, b"shared", "/shared/team/doc.txt")
            .await
            .expect("watcher did not publish a shared event");
        match &event.scope {
            EventScope::Users(ids) => {
                assert!(ids.contains(&2), "share member must receive the event");
                assert!(
                    !ids.contains(&1),
                    "creator without a grant is not an implicit audience (F6)"
                );
                // Admins are still delivered Users-scoped events by scope rules.
                assert!(event.scope.allows(9, true));
            }
            other => panic!("expected Users scope, got {other:?}"),
        }
        assert_eq!(event.data["source"], "external");
        assert_eq!(event.data["paths"], serde_json::json!(["/shared/team"]));
        assert!(event.data.get("entry").is_none());
        // Only logical paths, never host paths.
        let serialized = event.data.to_string();
        assert!(!serialized.contains(dir.path().to_str().unwrap()));
    }
}
