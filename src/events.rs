//! Server-sent event bus. A single broadcast channel feeds every connected UI;
//! the monitor sampler runs once for the whole process regardless of tabs.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

pub const BUS_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Machine-readable event kind, e.g. `monitor`, `job.progress`, `job.done`.
    pub kind: String,
    /// Unix epoch milliseconds.
    pub ts: i64,
    pub data: Value,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publish an event; returns the number of receivers that were notified.
    pub fn publish(&self, kind: &str, data: Value) -> usize {
        let event = Event {
            kind: kind.to_string(),
            ts: crate::db::now_ms(),
            data,
        };
        // No receivers is not an error.
        self.tx.send(event).unwrap_or(0)
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
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_fine() {
        let bus = EventBus::new();
        assert_eq!(bus.publish("noop", json!({})), 0);
    }
}
