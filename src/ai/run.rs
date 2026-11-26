//! Replies run on the server, one per conversation at a time.
//!
//! The provider's stream is read on a blocking thread; the run task batches
//! what arrives and publishes it to the account's pages as `ai.delta` events
//! (`{conversationId, messageId, seq, text, reasoning}`: text appended since
//! the previous delta, `seq` counting up from 1), keeps a snapshot for pages
//! that load the conversation meanwhile (`seq` tells which deltas they still
//! need), and stores the reply when it ends (`ai.message`). Stopping marks
//! the reply stopped at once; the thread ends at its next line.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, Notify};

use super::provider::{self, ChatRequest, Delta};
use crate::{db::now_ms, state::AppState};

/// How often streamed text is published.
const PUBLISH_EVERY: Duration = Duration::from_millis(120);

/// What a running reply has so far.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub text: String,
    pub reasoning: String,
    /// Deltas published so far.
    pub seq: u64,
}

#[derive(Debug, Default)]
pub struct Active {
    message_id: AtomicI64,
    cancel: Arc<AtomicBool>,
    stopped: Notify,
    snapshot: Mutex<Snapshot>,
}

impl Active {
    pub fn message_id(&self) -> i64 {
        self.message_id.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct Runs {
    active: Mutex<HashMap<String, Arc<Active>>>,
}

impl Runs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn get(&self, conversation_id: &str) -> Option<Arc<Active>> {
        self.active.lock().ok()?.get(conversation_id).cloned()
    }

    pub fn running(&self, conversation_id: &str) -> bool {
        self.get(conversation_id).is_some()
    }

    /// Claim the conversation for a new reply (`None`: one is running).
    pub fn reserve(self: &Arc<Self>, conversation_id: &str) -> Option<Reservation> {
        let mut active = self.active.lock().ok()?;
        if active.contains_key(conversation_id) {
            return None;
        }
        let run = Arc::new(Active::default());
        active.insert(conversation_id.to_string(), run.clone());
        Some(Reservation {
            runs: self.clone(),
            conversation_id: conversation_id.to_string(),
            run,
            armed: true,
        })
    }

    /// Stop the conversation's reply. Returns whether one was running.
    pub fn stop(&self, conversation_id: &str) -> bool {
        match self.get(conversation_id) {
            Some(run) => {
                run.cancel.store(true, Ordering::Relaxed);
                run.stopped.notify_one();
                true
            }
            None => false,
        }
    }

    fn finish(&self, conversation_id: &str, run: &Arc<Active>) {
        if let Ok(mut active) = self.active.lock() {
            if active
                .get(conversation_id)
                .is_some_and(|current| Arc::ptr_eq(current, run))
            {
                active.remove(conversation_id);
            }
        }
    }
}

/// A claimed conversation; released when dropped before [`Reservation::start`].
pub struct Reservation {
    runs: Arc<Runs>,
    conversation_id: String,
    run: Arc<Active>,
    armed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            self.runs.finish(&self.conversation_id, &self.run);
        }
    }
}

/// Who and what a reply is for.
pub struct ReplyJob {
    pub owner_id: i64,
    pub conversation_id: String,
    pub message_id: i64,
    pub provider_id: String,
    pub model: String,
    pub request: ChatRequest,
}

impl Reservation {
    /// Write the reply (stored as message `job.message_id`) in the background.
    pub fn start(mut self, state: AppState, job: ReplyJob) {
        self.armed = false;
        self.run.message_id.store(job.message_id, Ordering::Relaxed);
        let runs = self.runs.clone();
        let run = self.run.clone();
        tokio::spawn(async move {
            let conversation_id = job.conversation_id.clone();
            reply(&state, &run, job).await;
            runs.finish(&conversation_id, &run);
        });
    }
}

enum End {
    Finished(Result<provider::Usage, String>),
    Stopped,
}

async fn reply(state: &AppState, run: &Arc<Active>, job: ReplyJob) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Delta>();
    let cancel = run.cancel.clone();
    let request = job.request.clone();
    let prompt_chars: usize =
        request.system.len() + request.messages.iter().map(|m| m.text.len()).sum::<usize>();
    let mut reading = tokio::task::spawn_blocking(move || {
        provider::stream_chat(&request, &cancel, &mut |delta| {
            let _ = tx.send(delta);
        })
    });
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    let mut ticker = tokio::time::interval(PUBLISH_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let publish = |pending_text: &mut String, pending_reasoning: &mut String| {
        if pending_text.is_empty() && pending_reasoning.is_empty() {
            return;
        }
        let seq = {
            let mut snapshot = match run.snapshot.lock() {
                Ok(s) => s,
                Err(poisoned) => poisoned.into_inner(),
            };
            snapshot.text.push_str(pending_text);
            snapshot.reasoning.push_str(pending_reasoning);
            snapshot.seq += 1;
            snapshot.seq
        };
        state.events.publish_user(
            "ai.delta",
            job.owner_id,
            json!({
                "conversationId": job.conversation_id,
                "messageId": job.message_id,
                "seq": seq,
                "text": pending_text,
                "reasoning": pending_reasoning,
            }),
        );
        pending_text.clear();
        pending_reasoning.clear();
    };
    let end = loop {
        tokio::select! {
            biased;
            _ = run.stopped.notified() => break End::Stopped,
            delta = rx.recv() => match delta {
                Some(Delta::Text(t)) => { text.push_str(&t); pending_text.push_str(&t); }
                Some(Delta::Reasoning(r)) => { reasoning.push_str(&r); pending_reasoning.push_str(&r); }
                None => {
                    // The reader is done (its sender dropped): take its result.
                    let result = (&mut reading).await.unwrap_or_else(|e| Err(format!("the reply stopped unexpectedly: {e}")));
                    break End::Finished(result);
                }
            },
            _ = ticker.tick() => publish(&mut pending_text, &mut pending_reasoning),
        }
    };
    publish(&mut pending_text, &mut pending_reasoning);

    let (status, error, usage) = match end {
        End::Stopped => ("stopped", String::new(), None),
        End::Finished(Ok(usage)) => ("done", String::new(), Some(usage)),
        End::Finished(Err(error)) => ("error", error, None),
    };
    // Tokens the provider reported; otherwise about four characters each,
    // when anything was written (a refused request costs nothing).
    let (input, output) = match usage {
        Some(u) if u.input > 0 || u.output > 0 => (u.input, u.output),
        _ if !text.is_empty() || !reasoning.is_empty() => (
            (prompt_chars / 4) as i64,
            ((text.len() + reasoning.len()) / 4).max(1) as i64,
        ),
        _ => (0, 0),
    };
    let content = super::reply_content(&reasoning, &text);
    let now = now_ms();
    if let Err(e) = store(
        &state.db, &job, &content, status, &error, input, output, now,
    )
    .await
    {
        tracing::warn!(error = %e, "could not store an AI reply");
    }
    if let Err(e) = super::record_usage(
        &state.db,
        job.owner_id,
        &job.provider_id,
        &job.model,
        input,
        output,
    )
    .await
    {
        tracing::warn!(error = %e, "could not record AI usage");
    }
    if let Ok(Some(message)) = super::message(&state.db, job.message_id).await {
        state.events.publish_user_and_admin(
            "ai.message",
            job.owner_id,
            json!({"conversationId": job.conversation_id, "message": message.json(), "running": false}),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn store(
    pool: &SqlitePool,
    job: &ReplyJob,
    content: &serde_json::Value,
    status: &str,
    error: &str,
    input: i64,
    output: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE ai_messages SET content = ?, status = ?, error = ?, input_tokens = ?, output_tokens = ?,
         finished_at = ? WHERE id = ?",
    )
    .bind(content.to_string())
    .bind(status)
    .bind(error)
    .bind(input)
    .bind(output)
    .bind(now)
    .bind(job.message_id)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE ai_conversations SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&job.conversation_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replies that were being written when RAST stopped.
pub async fn recover(pool: &SqlitePool) {
    let result = sqlx::query(
        "UPDATE ai_messages SET status = 'error', error = 'Interrupted: RAST restarted while this reply was written.',
         finished_at = ? WHERE status = 'streaming'",
    )
    .bind(now_ms())
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "could not mark interrupted AI replies");
    }
}
