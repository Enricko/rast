//! Server-sent event bus with authorization scopes and global ordering.
//!
//! Every subscriber (one per browser tab) receives a filtered stream:
//! events are tagged with an [`EventScope`] and the stream handler drops events
//! the authenticated user may not see. A process-wide monotonic sequence number
//! lets clients detect gaps (and then reconcile with a fresh snapshot instead of
//! silently applying out-of-order state).
//!
//! A single broadcast channel feeds every connected UI; the monitor sampler runs
//! once for the whole process regardless of tabs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

pub const BUS_CAPACITY: usize = 512;

/// Who is allowed to receive an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// Everyone with a valid session (monitor samples, system notices).
    Broadcast,
    /// Administrators only (users, applications, backups, settings, audit of all).
    Admin,
    /// One account, by id.
    User(i64),
    /// An explicit set of accounts (e.g. members of a shared folder).
    Users(Vec<i64>),
    /// The owning account and every administrator (job progress, own audit).
    UserAndAdmin(i64),
    /// Everyone who may currently read a shared folder, identified by its
    /// stable `shares.id`. The audience is decided **at delivery time** by the
    /// folder evaluator (`access::folder_access_by_id`): administrators,
    /// legacy members, direct and group grants, minus explicit denials. A
    /// grant or revocation therefore applies to the very next event without
    /// re-publishing, and no audience list is ever serialised.
    Share(String),
}

impl EventScope {
    /// Does this scope include the given authenticated principal?
    ///
    /// [`EventScope::Share`] cannot be decided synchronously (it needs the
    /// folder evaluator) and returns `false` here; the SSE handler evaluates it
    /// with [`EventScope::share_id`].
    pub fn allows(&self, user_id: i64, is_admin: bool) -> bool {
        match self {
            Self::Broadcast => true,
            Self::Admin => is_admin,
            Self::User(id) => *id == user_id,
            Self::Users(ids) => is_admin || ids.contains(&user_id),
            Self::UserAndAdmin(id) => is_admin || *id == user_id,
            Self::Share(_) => false,
        }
    }

    /// The shared folder whose readers form this scope's audience.
    pub fn share_id(&self) -> Option<&str> {
        match self {
            Self::Share(id) => Some(id),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Broadcast => "broadcast",
            Self::Admin => "admin",
            Self::User(_) => "user",
            Self::Users(_) => "users",
            Self::UserAndAdmin(_) => "user+admin",
            Self::Share(_) => "share",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Monotonic sequence number; clients ignore duplicates and detect gaps.
    pub seq: u64,
    /// Machine-readable event kind, e.g. `monitor`, `job.progress`, `file.changed`.
    pub kind: String,
    /// Unix epoch milliseconds.
    pub ts: i64,
    /// Authorization scope (never serialised to clients).
    #[serde(skip)]
    pub scope: EventScope,
    pub data: Value,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
    /// Serialises "take the next sequence number" with "send", so concurrent
    /// publishers can never deliver `n + 1` before `n` (which subscribers would
    /// otherwise report as a gap and drop).
    order: Arc<Mutex<()>>,
    /// Flipped once when the process starts shutting down; long-lived streams
    /// end so a restart is not held open by connected browsers.
    shutdown: Arc<watch::Sender<bool>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        let (shutdown, _) = watch::channel(false);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(1)),
            order: Arc::new(Mutex::new(())),
            shutdown: Arc::new(shutdown),
        }
    }

    /// Signal every open stream that the server is shutting down.
    pub fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Resolves once [`EventBus::begin_shutdown`] was called (immediately if
    /// it already was).
    pub fn shutdown_signal(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|stopping| *stopping).await;
        }
    }

    /// A receiver for the shutdown flag (for `select!` loops).
    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Sequence number of the last published event (`0` before the first one).
    ///
    /// The internal counter holds the *next* sequence value, so the last
    /// published value is `counter - 1`. The SSE `hello` frame advertises this
    /// number; using the raw counter would make the first event after a
    /// connection look like a duplicate and be dropped by gap detection.
    pub fn sequence(&self) -> u64 {
        self.seq.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// Publish to every authenticated subscriber.
    pub fn publish(&self, kind: &str, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::Broadcast, data)
    }

    pub fn publish_scoped(&self, kind: &str, scope: EventScope, data: Value) -> usize {
        let kind = kind.to_string();
        let ts = crate::db::now_ms();
        // Poisoning cannot leave the counter inconsistent; keep publishing.
        let _order = self
            .order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            seq,
            kind,
            ts,
            scope,
            data,
        };
        self.tx.send(event).unwrap_or(0)
    }

    pub fn publish_user(&self, kind: &str, user_id: i64, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::User(user_id), data)
    }

    pub fn publish_user_and_admin(&self, kind: &str, user_id: i64, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::UserAndAdmin(user_id), data)
    }

    pub fn publish_users(&self, kind: &str, user_ids: Vec<i64>, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::Users(user_ids), data)
    }

    pub fn publish_admin(&self, kind: &str, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::Admin, data)
    }

    /// Publish to everyone who may currently read the shared folder `share_id`.
    pub fn publish_share(&self, kind: &str, share_id: &str, data: Value) -> usize {
        self.publish_scoped(kind, EventScope::Share(share_id.to_string()), data)
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn publishes_to_subscribers() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        assert_eq!(bus.publish("test", json!({"a": 1})), 1);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, "test");
        assert_eq!(ev.data["a"], 1);
        assert!(ev.ts > 0);
        assert_eq!(ev.scope, EventScope::Broadcast);
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_fine() {
        let bus = EventBus::new();
        assert_eq!(bus.publish("noop", json!({})), 0);
    }

    #[tokio::test]
    async fn sequence_is_monotonic() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish("a", json!({}));
        bus.publish_user("b", 7, json!({}));
        bus.publish_admin("c", json!({}));
        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        let c = rx.recv().await.unwrap();
        assert!(a.seq < b.seq && b.seq < c.seq);
        assert_eq!(bus.sequence(), c.seq);
    }

    #[test]
    fn scope_authorization() {
        assert!(EventScope::Broadcast.allows(1, false));
        assert!(!EventScope::Admin.allows(1, false));
        assert!(EventScope::Admin.allows(1, true));
        assert!(EventScope::User(5).allows(5, false));
        assert!(!EventScope::User(5).allows(6, true));
        assert!(EventScope::Users(vec![2, 3]).allows(3, false));
        assert!(!EventScope::Users(vec![2, 3]).allows(4, false));
        // Admins may observe a shared-folder event even when not a member.
        assert!(EventScope::Users(vec![2]).allows(9, true));
        assert!(EventScope::UserAndAdmin(7).allows(7, false));
        assert!(EventScope::UserAndAdmin(7).allows(1, true));
        assert!(!EventScope::UserAndAdmin(7).allows(8, false));
        // Share scopes are decided by the folder evaluator, never synchronously.
        let share = EventScope::Share("s1".into());
        assert!(!share.allows(1, true));
        assert!(!share.allows(1, false));
        assert_eq!(share.share_id(), Some("s1"));
        assert_eq!(EventScope::Admin.share_id(), None);
    }

    #[tokio::test]
    async fn concurrent_publishers_deliver_in_sequence_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut tasks = Vec::new();
        for worker in 0..8 {
            let bus = bus.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                for i in 0..50 {
                    bus.publish("load", json!({"worker": worker, "i": i}));
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut last = 0;
        for _ in 0..400 {
            let event = rx.recv().await.unwrap();
            assert_eq!(event.seq, last + 1, "events must arrive in sequence order");
            last = event.seq;
        }
    }

    #[tokio::test]
    async fn shutdown_signal_resolves_for_existing_and_late_waiters() {
        let bus = EventBus::new();
        assert!(!bus.is_shutting_down());
        let early = bus.shutdown_signal();
        bus.begin_shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(1), early)
            .await
            .expect("waiter registered before shutdown resolves");
        tokio::time::timeout(std::time::Duration::from_secs(1), bus.shutdown_signal())
            .await
            .expect("waiter registered after shutdown resolves immediately");
        assert!(bus.is_shutting_down());
    }
}
