//! In-memory sliding-window rate limiting for login and other abuse-prone
//! endpoints. Cheap, bounded, and reset on restart by design.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use crate::db;

#[derive(Debug)]
struct Window {
    hits: VecDeque<i64>,
    last_prune: i64,
}

/// Sliding-window limiter keyed by arbitrary strings (IP, username, ...).
#[derive(Debug)]
pub struct RateLimiter {
    max_hits: usize,
    window_ms: i64,
    inner: Mutex<HashMap<String, Window>>,
    max_keys: usize,
}

impl RateLimiter {
    pub fn new(max_hits: usize, window: std::time::Duration) -> Self {
        Self {
            max_hits: max_hits.max(1),
            window_ms: window.as_millis() as i64,
            inner: Mutex::new(HashMap::new()),
            max_keys: 10_000,
        }
    }

    /// Record a hit and report whether it is allowed.
    pub fn check(&self, key: &str) -> Decision {
        let now = db::now_ms();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > self.max_keys {
            map.retain(|_, w| now - w.last_prune < self.window_ms);
            if map.len() > self.max_keys {
                // Hard bound: drop arbitrary entries rather than grow unbounded.
                let keys: Vec<String> = map.keys().take(map.len() / 2).cloned().collect();
                for k in keys {
                    map.remove(&k);
                }
            }
        }
        let entry = map.entry(key.to_string()).or_insert_with(|| Window {
            hits: VecDeque::new(),
            last_prune: now,
        });
        while let Some(front) = entry.hits.front() {
            if now - *front >= self.window_ms {
                entry.hits.pop_front();
            } else {
                break;
            }
        }
        entry.last_prune = now;
        if entry.hits.len() >= self.max_hits {
            let oldest = *entry.hits.front().unwrap_or(&now);
            let retry_ms = (oldest + self.window_ms - now).max(0);
            return Decision::Limited {
                retry_after_secs: (retry_ms as u64).div_ceil(1000).max(1),
            };
        }
        entry.hits.push_back(now);
        Decision::Allowed
    }

    /// Forget the history for a key (e.g. after a successful login).
    pub fn reset(&self, key: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(key);
    }

    pub fn prune(&self) {
        let now = db::now_ms();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, w| now - w.last_prune < self.window_ms);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Limited { retry_after_secs: u64 },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_up_to_limit_then_blocks() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        assert!(rl.check("ip").is_allowed());
        assert!(rl.check("ip").is_allowed());
        assert!(rl.check("ip").is_allowed());
        match rl.check("ip") {
            Decision::Limited { retry_after_secs } => assert!(retry_after_secs >= 1),
            Decision::Allowed => panic!("fourth hit must be limited"),
        }
        // Different keys are independent.
        assert!(rl.check("other").is_allowed());
        rl.reset("ip");
        assert!(rl.check("ip").is_allowed());
    }

    #[test]
    fn window_expires() {
        let rl = RateLimiter::new(1, Duration::from_millis(1));
        assert!(rl.check("k").is_allowed());
        std::thread::sleep(Duration::from_millis(5));
        assert!(rl.check("k").is_allowed());
    }
}
