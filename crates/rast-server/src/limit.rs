//! Sign-in throttling: after a few wrong passwords for an account (or from one
//! address), further attempts wait, doubling up to 15 minutes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FREE_ATTEMPTS: u32 = 5;
const FIRST_WAIT: Duration = Duration::from_secs(30);
const LONGEST_WAIT: Duration = Duration::from_secs(15 * 60);
/// Failures older than this are forgotten.
const WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub(crate) struct Limiter {
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    failures: u32,
    last: Instant,
    locked_until: Option<Instant>,
}

impl Limiter {
    /// Seconds to wait before `keys` may try again, if any of them is locked.
    pub(crate) fn wait(&self, keys: &[String]) -> Option<u64> {
        let now = Instant::now();
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        keys.iter()
            .filter_map(|key| entries.get(key)?.locked_until)
            .filter(|until| *until > now)
            .map(|until| (until - now).as_secs().max(1))
            .max()
    }

    pub(crate) fn fail(&self, keys: &[String]) {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if entries.len() > 10_000 {
            entries.retain(|_, entry| now.duration_since(entry.last) < WINDOW);
        }
        for key in keys {
            let entry = entries.entry(key.clone()).or_insert(Entry {
                failures: 0,
                last: now,
                locked_until: None,
            });
            if now.duration_since(entry.last) >= WINDOW {
                entry.failures = 0;
            }
            entry.failures += 1;
            entry.last = now;
            if entry.failures >= FREE_ATTEMPTS {
                let doublings = (entry.failures - FREE_ATTEMPTS).min(5);
                let wait = (FIRST_WAIT * 2u32.pow(doublings)).min(LONGEST_WAIT);
                entry.locked_until = Some(now + wait);
            }
        }
    }

    pub(crate) fn succeed(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_failures_lock_the_key_and_success_clears_it() {
        let limiter = Limiter::default();
        let keys = ["user:admin".to_owned()];
        for _ in 0..4 {
            limiter.fail(&keys);
        }
        assert_eq!(limiter.wait(&keys), None, "four failures are free");
        limiter.fail(&keys);
        let wait = limiter.wait(&keys).expect("the fifth locks");
        assert!((1..=30).contains(&wait));
        limiter.succeed("user:admin");
        assert_eq!(limiter.wait(&keys), None);
    }
}
