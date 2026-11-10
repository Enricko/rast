//! Server-sent events stream for live monitor and job updates.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::StreamExt;
use serde_json::json;

use crate::{auth::AuthUser, db, state::AppState};

// ---------------------------------------------------------------------------

/// GET /api/events — live updates (monitor samples, job progress, notifications).
pub async fn stream(
    State(state): State<AppState>,
    user: AuthUser,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let is_admin = user.is_admin();
    let user_id = user.id;

    let initial = {
        let snapshot = state.monitor.latest().unwrap_or_else(
            || json!({"ts": db::now_ms(), "unavailable": true, "reason": "no sample yet"}),
        );
        let ev = Event::default()
            .event("monitor")
            .id(db::now_ms().to_string())
            .data(snapshot.to_string());
        futures::stream::once(async move { Ok(ev) })
    };

    let live = futures::stream::unfold(
        (rx, is_admin, user_id),
        |(mut rx, is_admin, user_id)| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Some(owner) = event.data.get("userId").and_then(|v| v.as_i64()) {
                            if !is_admin && owner != user_id {
                                continue;
                            }
                        }
                        let sse = Event::default()
                            .event(event.kind.clone())
                            .id(event.ts.to_string())
                            .data(event.data.to_string());
                        return Some((Ok(sse), (rx, is_admin, user_id)));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    Sse::new(initial.chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
