//! Server-sent events stream: authenticated, authorization-filtered and
//! gap-signalling.
//!
//! One connection per browser tab. The stream starts with a `hello` snapshot
//! (sequence cursor + identity) so the client can detect gaps, sends `ping`
//! heartbeats for staleness detection, and polls the session every 20 seconds so
//! revocation, expiry or deactivation closes open connections promptly. When
//! the broadcast receiver lags it emits `resync` instead of silently dropping
//! events; the client then refetches authorized snapshots.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::StreamExt;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    access,
    auth::{self, session, AuthUser},
    db,
    state::AppState,
};

const PING_INTERVAL: Duration = Duration::from_secs(15);
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

struct StreamState {
    rx: tokio::sync::broadcast::Receiver<crate::events::Event>,
    user: AuthUser,
    role: String,
    db: sqlx::SqlitePool,
    token: String,
    session_id: String,
    ttl_hours: i64,
    last_seq: u64,
    ping: tokio::time::Interval,
    session_check: tokio::time::Interval,
}

/// GET /api/events
pub async fn stream(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let token = auth::cookie_value(&headers, session::SESSION_COOKIE).unwrap_or_default();
    let role = user.role.as_str().to_string();
    let rx = state.events.subscribe();
    let hello_seq = state.events.sequence();

    let monitor_allowed = access::event_allowed(&state.db, &user, "monitor", &json!({})).await;
    let initial = {
        let snapshot = state.monitor.latest().unwrap_or_else(
            || json!({"ts": db::now_ms(), "unavailable": true, "reason": "no sample yet"}),
        );
        let hello = Event::default()
            .event("hello")
            .id(hello_seq.to_string())
            .data(
                json!({
                    "seq": hello_seq,
                    "ts": db::now_ms(),
                    "userId": user.id,
                    "admin": user.is_admin(),
                    "sessionId": user.session_id,
                    "version": crate::VERSION,
                })
                .to_string(),
            );
        // Container inventory is limited to administrators and accounts that
        // may use Applications (audit finding F8).
        let can_see_containers = crate::api::monitor::can_see_containers(&state, &user).await;
        let monitor = Event::default()
            .event("monitor")
            .id(hello_seq.to_string())
            .data(crate::monitor::redact_for(can_see_containers, &snapshot).to_string());
        if monitor_allowed {
            futures::stream::iter(vec![Ok::<Event, Infallible>(hello), Ok(monitor)])
        } else {
            futures::stream::iter(vec![Ok::<Event, Infallible>(hello)])
        }
    };

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut session_check = tokio::time::interval(SESSION_CHECK_INTERVAL);
    session_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; consume it so the first heartbeat and
    // revalidation happen after a full interval.
    ping.tick().await;
    session_check.tick().await;

    let session_id = user.session_id.clone();
    let state_data = StreamState {
        rx,
        user,
        role,
        db: state.db.clone(),
        token,
        session_id,
        ttl_hours: state.config.session_ttl_hours,
        last_seq: hello_seq,
        ping,
        session_check,
    };

    let live = futures::stream::unfold(state_data, move |mut st| {
        async move {
            loop {
                tokio::select! {
                    biased;
                    received = st.rx.recv() => {
                        match received {
                            Ok(event) => {
                                if event.seq <= st.last_seq {
                                    continue; // duplicate / already seen
                                }
                                let gap = event.seq > st.last_seq + 1;
                                st.last_seq = event.seq;
                                if gap {
                                    // Events were lost (slow consumer or restart).
                                    let resync = Event::default()
                                        .event("resync")
                                        .id(event.seq.to_string())
                                        .data(json!({"reason": "gap", "seq": event.seq}).to_string());
                                    return Some((Ok(resync), st));
                                }
                                if !event.scope.allows(st.user.id, st.user.is_admin()) {
                                    continue;
                                }
                                // Application policy: only deliver events for apps the
                                // account may currently use (cached, generation-aware).
                                if !access::event_allowed(&st.db, &st.user, &event.kind, &event.data)
                                    .await
                                {
                                    continue;
                                }
                                // Monitor samples are published once for everyone;
                                // redact the container inventory per connection with the
                                // account's *current* Applications access (cached,
                                // generation-aware), so a revocation applies to the very
                                // next sample without reconnecting.
                                let payload = if event.kind == "monitor" {
                                    let can_see = st.user.is_admin()
                                        || access::app_access(&st.db, &st.user, "applications")
                                            .await
                                            .map(|access| access.allowed)
                                            .unwrap_or(false);
                                    crate::monitor::redact_for(can_see, &event.data)
                                } else {
                                    event.data
                                };
                                let sse = Event::default()
                                    .event(event.kind)
                                    .id(event.seq.to_string())
                                    .data(serde_json::to_string(&payload).unwrap_or_else(|_| "null".into()));
                                return Some((Ok(sse), st));
                            }
                            Err(RecvError::Lagged(skipped)) => {
                                let resync = Event::default()
                                    .event("resync")
                                    .data(json!({"reason": "lag", "skipped": skipped}).to_string());
                                return Some((Ok(resync), st));
                            }
                            Err(RecvError::Closed) => return None,
                        }
                    }
                    _ = st.ping.tick() => {
                        let sse = Event::default()
                            .event("ping")
                            .id(st.last_seq.to_string())
                            .data(json!({"ts": db::now_ms(), "seq": st.last_seq}).to_string());
                        return Some((Ok(sse), st));
                    }
                    _ = st.session_check.tick() => {
                        let valid = check_session(&st).await;
                        match valid {
                            SessionState::Valid => continue,
                            SessionState::Revoked(reason) => {
                                let sse = Event::default()
                                    .event("session.revoked")
                                    .data(json!({"reason": reason}).to_string());
                                return Some((Ok(sse), st));
                            }
                        }
                    }
                }
            }
        }
    });
    Sse::new(initial.chain(live)).keep_alive(
        KeepAlive::new()
            .interval(KEEPALIVE_INTERVAL)
            .text("keep-alive"),
    )
}

enum SessionState {
    Valid,
    Revoked(&'static str),
}

async fn check_session(st: &StreamState) -> SessionState {
    let record = match session::validate(&st.db, &st.token, st.ttl_hours).await {
        Ok(Some(record)) => record,
        Ok(None) => return SessionState::Revoked("session expired"),
        Err(error) => {
            tracing::warn!(error = %error, "session revalidation failed; keeping stream");
            return SessionState::Valid;
        }
    };
    if record.session_id != st.session_id {
        return SessionState::Revoked("session replaced");
    }
    let row = sqlx::query_as::<_, (bool, String)>("SELECT active, role FROM users WHERE id = ?")
        .bind(st.user.id)
        .fetch_optional(&st.db)
        .await;
    match row {
        Ok(Some((active, role))) => {
            if !active {
                SessionState::Revoked("account disabled")
            } else if role != st.role {
                SessionState::Revoked("permissions changed")
            } else {
                SessionState::Valid
            }
        }
        Ok(None) => SessionState::Revoked("account removed"),
        Err(_) => SessionState::Valid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_interval_is_bounded() {
        // Heartbeats must be frequent enough for clients to detect staleness but
        // not so frequent that an idle stream costs measurable CPU.
        assert!(PING_INTERVAL.as_secs() >= 10 && PING_INTERVAL.as_secs() <= 30);
        assert!(SESSION_CHECK_INTERVAL.as_secs() >= 10);
    }
}
