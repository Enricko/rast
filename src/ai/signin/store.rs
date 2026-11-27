//! Sign-ins in flight, keyed by the random `state` the browser (or the device
//! page) carries back: the PKCE verifier, the device code and whose sign-in
//! it is. They live in this server's memory, not the database, because they
//! last minutes and are worth nothing once used.
//!
//! The store is deliberately small and self-cleaning: entries older than 20
//! minutes are dropped whenever it is touched, and its box is capped so a
//! script cannot grow it forever.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use super::Pending;

/// How long a started sign-in waits for its answer.
pub(crate) const PENDING_FOR: Duration = Duration::from_secs(20 * 60);
/// The most sign-ins that may wait at once (the oldest is dropped first).
const MAX_PENDING: usize = 64;
/// What a state that names nothing (or someone else's) answers.
pub(crate) const NOT_A_SIGN_IN: &str =
    "this sign-in is not the one started here or it timed out; start again";

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

fn locked() -> MutexGuard<'static, HashMap<String, Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drop the entries that have waited too long.
fn sweep(pending: &mut HashMap<String, Pending>) {
    pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
}

/// Remember a just-started sign-in under its `state`.
pub(crate) fn insert(state: String, pending: Pending) {
    let mut pending_map = locked();
    sweep(&mut pending_map);
    if pending_map.len() >= MAX_PENDING {
        if let Some(oldest) = pending_map
            .iter()
            .min_by_key(|(_, p)| p.started)
            .map(|(key, _)| key.clone())
        {
            pending_map.remove(&oldest);
        }
    }
    pending_map.insert(state, pending);
}

/// The sign-in a state names, if it is this user's. A clone: the caller must
/// not hold the lock while it waits on the network.
pub(crate) fn get(state: &str, user_id: i64) -> Result<Pending, String> {
    let mut pending_map = locked();
    sweep(&mut pending_map);
    match pending_map.get(state) {
        Some(pending) if pending.user_id == user_id => Ok(pending.clone()),
        _ => Err(NOT_A_SIGN_IN.into()),
    }
}

/// Take a sign-in: an answer is used once, and only by the person who
/// started it.
pub(crate) fn take(state: &str, user_id: i64) -> Result<Pending, String> {
    let mut pending_map = locked();
    sweep(&mut pending_map);
    match pending_map.get(state) {
        Some(pending) if pending.user_id == user_id => {
            Ok(pending_map.remove(state).expect("present"))
        }
        _ => Err(NOT_A_SIGN_IN.into()),
    }
}

/// Move the next allowed poll of a device sign-in forward.
pub(crate) fn schedule_next_poll(state: &str, user_id: i64, next: Instant) -> Result<(), String> {
    let mut pending_map = locked();
    sweep(&mut pending_map);
    match pending_map.get_mut(state) {
        Some(pending) if pending.user_id == user_id => {
            pending.next_poll = next;
            Ok(())
        }
        _ => Err(NOT_A_SIGN_IN.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::signin::{Flow, Plan};

    fn browser_pending(plan: Plan, user_id: i64) -> Pending {
        Pending::browser(
            plan,
            user_id,
            None,
            "a plan".into(),
            "state-for-test".into(),
            "verifier".into(),
            "http://localhost/callback".into(),
        )
    }

    #[test]
    fn a_state_is_answered_once_and_only_by_its_owner() {
        insert("store-test-state".into(), browser_pending(Plan::Codex, 7));
        assert_eq!(
            take("store-test-state", 8).unwrap_err(),
            NOT_A_SIGN_IN.to_string()
        );
        let pending = take("store-test-state", 7).unwrap();
        assert_eq!(pending.plan, Plan::Codex);
        assert_eq!(pending.plan.flow(), Flow::Browser);
        assert_eq!(
            take("store-test-state", 7).unwrap_err(),
            NOT_A_SIGN_IN.to_string()
        );
    }

    #[test]
    fn the_oldest_sign_in_makes_room() {
        // Fill past the cap; the box never grows beyond MAX_PENDING.
        for i in 0..MAX_PENDING + 3 {
            insert(format!("store-fill-{i}"), browser_pending(Plan::Codex, 7));
        }
        assert_eq!(locked().len(), MAX_PENDING);
    }
}
