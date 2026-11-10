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
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

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
}

impl EventScope {
    /// Does this scope include the given authenticated principal?
    pub fn allows(&self, user_id: i64, is_admin: bool) -> bool {
        match self {
            Self::Broadcast => true,
            Self::Admin => is_admin,
            Self::User(id) => *id == user_id,
            Self::Users(ids) => is_admin || ids.contains(&user_id),
            Self::UserAndAdmin(id) => is_admin || *id == user_id,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Broadcast => "broadcast",
            Self::Admin => "admin",
            Self::User(_) => "user",
            Self::Users(_) => "users",
            Self::UserAndAdmin(_) => "user+admin",
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
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(1)),
        }
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
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            seq,
            kind: kind.to_string(),
            ts: crate::db::now_ms(),
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
    }
}
