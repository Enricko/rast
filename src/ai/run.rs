//! Replies run on the server, one per conversation at a time.
//!
//! A reply goes in steps. Each step streams the model's answer: the run
//! task batches what arrives and publishes it to the account's pages as
//! `ai.delta` events (`{conversationId, messageId, seq, text, reasoning}`:
//! text added to the current step since the previous delta, see
//! [`super::apply_delta`]; `seq` counts up from 1 within a run). When the
//! model asks for tools, their blocks are added and the message is published
//! whole (`ai.message` with `running` and `seq`); calls the owner let run
//! without asking run at once, then the next step starts. A call that needs
//! an answer ends the run with the message `waiting`; answering it
//! (`POST …/tools/{callId}`) starts a new run that goes on from there.
//!
//! The run keeps the reply's blocks for pages that load the conversation
//! meanwhile (`seq` tells which deltas they still need) and stores them at
//! every tool change and when it ends. Stopping marks the reply stopped at
//! once; the provider's thread ends at its next line.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, Notify};

use super::{
    provider::{self, ChatError, ChatRequest, Delta, Turn},
    tools, Effective,
};
use crate::{
    audit::{self, AuditLog},
    auth::AuthUser,
    db::now_ms,
    state::AppState,
};

/// How often streamed text is published.
const PUBLISH_EVERY: Duration = Duration::from_millis(120);
/// Model calls in one run before it stops and leaves the rest to the owner.
pub const MAX_STEPS: usize = 25;
/// Tool calls in one reply.
pub const MAX_TOOL_CALLS: usize = 200;

/// What a running reply has so far.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub blocks: Vec<Value>,
    /// Deltas published so far in this run.
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

    fn with<T>(&self, f: impl FnOnce(&mut Snapshot) -> T) -> T {
        let mut snapshot = match self.snapshot.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut snapshot)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct Runs {
    active: Mutex<HashMap<String, Arc<Active>>>,
    /// Held while answering tool requests, sending, or stopping a waiting
    /// reply: one change to a waiting reply at a time.
    pub decide: tokio::sync::Mutex<()>,
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

    /// Claim the conversation for a new run (`None`: one is running).
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
    pub owner: AuthUser,
    pub conversation_id: String,
    pub message_id: i64,
    pub provider_id: String,
    pub model: String,
    /// Its messages: the conversation before this reply.
    pub request: ChatRequest,
    pub tools: tools::Available,
    /// Tools the owner let run without asking in this chat.
    pub allowed_tools: Vec<String>,
    /// The reply so far (empty for a new one).
    pub blocks: Vec<Value>,
    pub limits: Effective,
    /// Tokens the reply used before this run.
    pub tokens: (i64, i64),
}

impl Reservation {
    /// Write the reply (stored as message `job.message_id`) in the background.
    pub fn start(mut self, state: AppState, job: ReplyJob) {
        self.armed = false;
        self.run.message_id.store(job.message_id, Ordering::Relaxed);
        self.run.with(|s| s.blocks = job.blocks.clone());
        let runs = self.runs.clone();
        let run = self.run.clone();
        tokio::spawn(async move {
            let conversation_id = job.conversation_id.clone();
            reply(&state, &runs, &run, job).await;
            runs.finish(&conversation_id, &run);
        });
    }
}

enum End {
    Done,
    Waiting,
    Stopped,
    Failed(String),
}

enum Step {
    Answered {
        calls: Vec<provider::ToolCall>,
        usage: provider::Usage,
    },
    Stopped(provider::Usage),
    Failed {
        error: ChatError,
        wrote: bool,
    },
}

async fn reply(state: &AppState, runs: &Runs, run: &Arc<Active>, mut job: ReplyJob) {
    let history = std::mem::take(&mut job.request.messages);
    let mut tokens = job.tokens;
    let mut tools_on = !job.request.tools.is_empty();
    let mut steps = 0usize;
    let end = loop {
        if !run_allowed_tools(state, run, &job, tokens).await || run.cancelled() {
            break End::Stopped;
        }
        if steps >= MAX_STEPS {
            break End::Failed(format!(
                "The agent stopped after {MAX_STEPS} steps in a row. Send a message to let it go on."
            ));
        }
        if !job.owner.is_admin() {
            if let Err(error) = super::check_allowance(&state.db, job.owner.id, &job.limits).await {
                break End::Failed(error.public_message());
            }
        }
        steps += 1;
        let mut request = job.request.clone();
        if !tools_on {
            request.tools.clear();
        }
        request.messages = history.clone();
        request
            .messages
            .extend(super::turns_of(&Value::Array(run.snapshot().blocks)));
        match stream_step(state, run, &job, request).await {
            Step::Stopped(usage) => {
                count(state, &job, &mut tokens, usage).await;
                break End::Stopped;
            }
            Step::Failed { error, wrote } if error.tools_refused && tools_on && !wrote => {
                tracing::info!(model = %job.model, error = %error.message, "AI model refused tools; answering without them");
                tools_on = false;
                steps -= 1;
            }
            Step::Failed { error, .. } => break End::Failed(error.message),
            Step::Answered { calls, usage } => {
                count(state, &job, &mut tokens, usage).await;
                if calls.is_empty() {
                    break End::Done;
                }
                let (too_many, waiting) = run.with(|s| {
                    for call in &calls {
                        let status = if !job.tools.has(&call.name) {
                            "error"
                        } else if tools::may_skip_asking(&call.name)
                            && job.allowed_tools.iter().any(|t| t == &call.name)
                        {
                            "allowed"
                        } else {
                            "pending"
                        };
                        let mut block = super::tool_block(call, status);
                        if status == "error" {
                            block["error"] = json!(if tools::is_tool(&call.name) {
                                format!("the tool {} is not available to this account", call.name)
                            } else {
                                format!("there is no tool called {:?}", call.name)
                            });
                        }
                        s.blocks.push(block);
                    }
                    let used = s.blocks.iter().filter(|b| super::is_tool_block(b)).count();
                    let waiting = s.blocks.iter().any(|b| b["status"] == "pending");
                    (used > MAX_TOOL_CALLS, waiting)
                });
                if too_many {
                    break End::Failed(format!(
                        "The agent asked for more than {MAX_TOOL_CALLS} tool calls in one reply and was stopped."
                    ));
                }
                if waiting {
                    break End::Waiting;
                }
                progress(state, run, &job, tokens).await;
            }
        }
    };

    let (status, error) = match end {
        End::Done => ("done", String::new()),
        End::Waiting => ("waiting", String::new()),
        End::Stopped => ("stopped", String::new()),
        End::Failed(error) => ("error", error),
    };
    if status != "waiting" {
        // Calls that will not run now.
        run.with(|s| {
            for block in s.blocks.iter_mut().filter(|b| super::is_tool_block(b)) {
                if matches!(
                    block["status"].as_str(),
                    Some("pending" | "allowed" | "running")
                ) {
                    block["status"] = json!("skipped");
                }
            }
        });
    }
    let content = Value::Array(run.snapshot().blocks);
    let finished = (status != "waiting").then(now_ms);
    if let Err(e) = store(&state.db, &job, &content, status, &error, tokens, finished).await {
        tracing::warn!(error = %e, "could not store an AI reply");
    }
    // Stored and counted: the conversation takes the next message now
    // (before pages hear the reply ended and send one).
    runs.finish(&job.conversation_id, run);
    publish_message(state, &job, false, 0).await;
    if let Ok(Some(conversation)) = super::conversation(&state.db, &job.conversation_id).await {
        state.events.publish_user_and_admin(
            "ai.conversation",
            job.owner.id,
            json!({ "conversation": conversation.json(Some(&job.owner.username), false) }),
        );
    }
}

/// Run the calls the owner allowed, in order. False when stopped.
async fn run_allowed_tools(
    state: &AppState,
    run: &Arc<Active>,
    job: &ReplyJob,
    tokens: (i64, i64),
) -> bool {
    loop {
        if run.cancelled() {
            return false;
        }
        let next = run.with(|s| {
            let at = s
                .blocks
                .iter()
                .position(|b| super::is_tool_block(b) && b["status"] == "allowed")?;
            s.blocks[at]["status"] = json!("running");
            Some((
                at,
                s.blocks[at]["name"].as_str().unwrap_or("").to_string(),
                s.blocks[at]["input"].clone(),
            ))
        });
        let Some((at, name, input)) = next else {
            return true;
        };
        progress(state, run, job, tokens).await;
        let outcome = tokio::select! {
            biased;
            _ = run.stopped.notified() => None,
            result = tools::execute(state, &job.owner, &job.tools, &name, &input) => Some(result),
        };
        let stopped = outcome.is_none();
        let failed = !matches!(outcome, Some(Ok(_)));
        run.with(|s| {
            let block = &mut s.blocks[at];
            match outcome {
                Some(Ok(output)) => {
                    block["status"] = json!("done");
                    block["output"] = json!(tools::cap(
                        &output,
                        tools::MAX_RESULT,
                        name == tools::RUN_COMMAND
                    ));
                }
                Some(Err(error)) => {
                    block["status"] = json!("error");
                    block["error"] = json!(tools::cap(&error, 4000, false));
                }
                None => {
                    block["status"] = json!("error");
                    block["error"] = json!("Stopped while it ran.");
                }
            }
        });
        let mut log = AuditLog::new(format!("ai.tool.{name}"))
            .actor(&job.owner)
            .object("ai_conversation", &job.conversation_id)
            .path(tools::summary(&name, &input));
        log = if failed { log.failure() } else { log.success() };
        audit::record_with_events(&state.db, &state.events, log).await;
        progress(state, run, job, tokens).await;
        if stopped {
            return false;
        }
    }
}

/// One model call: stream its answer into the current step.
async fn stream_step(
    state: &AppState,
    run: &Arc<Active>,
    job: &ReplyJob,
    request: ChatRequest,
) -> Step {
    let prompt_chars =
        request.system.len() + request.messages.iter().map(Turn::chars).sum::<usize>();
    let (tx, mut rx) = mpsc::unbounded_channel::<Delta>();
    let cancel = run.cancel.clone();
    let mut reading = tokio::task::spawn_blocking(move || {
        provider::stream_chat(&request, &cancel, &mut |delta| {
            let _ = tx.send(delta);
        })
    });
    let mut wrote = 0usize;
    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    let mut ticker = tokio::time::interval(PUBLISH_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let publish = |pending_text: &mut String, pending_reasoning: &mut String| {
        if pending_text.is_empty() && pending_reasoning.is_empty() {
            return;
        }
        let seq = run.with(|s| {
            super::apply_delta(&mut s.blocks, pending_text, pending_reasoning);
            s.seq += 1;
            s.seq
        });
        state.events.publish_user(
            "ai.delta",
            job.owner.id,
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
    let result = loop {
        tokio::select! {
            biased;
            _ = run.stopped.notified() => break None,
            delta = rx.recv() => match delta {
                Some(Delta::Text(t)) => { wrote += t.len(); pending_text.push_str(&t); }
                Some(Delta::Reasoning(r)) => { wrote += r.len(); pending_reasoning.push_str(&r); }
                None => {
                    // The reader is done (its sender dropped): take its result.
                    break Some((&mut reading).await.unwrap_or_else(|e| {
                        Err(format!("the reply stopped unexpectedly: {e}").into())
                    }));
                }
            },
            _ = ticker.tick() => publish(&mut pending_text, &mut pending_reasoning),
        }
    };
    publish(&mut pending_text, &mut pending_reasoning);
    // Tokens the provider reported; otherwise about four characters each,
    // when anything was written (a refused request costs nothing).
    let tokens = |usage: provider::Usage, extra: usize| {
        if !usage.is_empty() {
            usage
        } else if wrote + extra > 0 {
            provider::Usage {
                input: (prompt_chars / 4) as i64,
                output: ((wrote + extra) / 4).max(1) as i64,
                ..Default::default()
            }
        } else {
            provider::Usage::default()
        }
    };
    match result {
        None => Step::Stopped(tokens(provider::Usage::default(), 0)),
        Some(Ok(outcome)) if run.cancelled() => Step::Stopped(tokens(outcome.usage, 0)),
        Some(Ok(outcome)) => {
            let call_chars: usize = outcome
                .calls
                .iter()
                .map(|c| c.name.len() + c.input.to_string().len())
                .sum();
            Step::Answered {
                usage: tokens(outcome.usage, call_chars),
                calls: outcome.calls,
            }
        }
        Some(Err(error)) => Step::Failed {
            error,
            wrote: wrote > 0,
        },
    }
}

async fn count(state: &AppState, job: &ReplyJob, tokens: &mut (i64, i64), usage: provider::Usage) {
    tokens.0 += usage.input;
    tokens.1 += usage.output;
    if usage.is_empty() {
        return;
    }
    if let Err(e) = super::record_usage(
        &state.db,
        job.owner.id,
        &job.provider_id,
        &job.model,
        &usage,
    )
    .await
    {
        tracing::warn!(error = %e, "could not record AI usage");
    }
}

/// Store the reply as it is (still being written) and show it.
async fn progress(state: &AppState, run: &Arc<Active>, job: &ReplyJob, tokens: (i64, i64)) {
    let (content, seq) = run.with(|s| (Value::Array(s.blocks.clone()), s.seq));
    if let Err(e) = store(&state.db, job, &content, "streaming", "", tokens, None).await {
        tracing::warn!(error = %e, "could not store an AI reply");
    }
    publish_message(state, job, true, seq).await;
}

async fn publish_message(state: &AppState, job: &ReplyJob, running: bool, seq: u64) {
    if let Ok(Some(message)) = super::message(&state.db, job.message_id).await {
        state.events.publish_user_and_admin(
            "ai.message",
            job.owner.id,
            json!({"conversationId": job.conversation_id, "message": message.json(), "running": running, "seq": seq}),
        );
    }
}

async fn store(
    pool: &SqlitePool,
    job: &ReplyJob,
    content: &Value,
    status: &str,
    error: &str,
    tokens: (i64, i64),
    finished_at: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE ai_messages SET content = ?, status = ?, error = ?, input_tokens = ?, output_tokens = ?,
         finished_at = ? WHERE id = ?",
    )
    .bind(content.to_string())
    .bind(status)
    .bind(error)
    .bind(tokens.0)
    .bind(tokens.1)
    .bind(finished_at)
    .bind(job.message_id)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE ai_conversations SET updated_at = ? WHERE id = ?")
        .bind(now_ms())
        .bind(&job.conversation_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replies that were being written when RAST stopped (replies waiting for
/// an answer stay waiting).
pub async fn recover(pool: &SqlitePool) {
    let rows = match sqlx::query("SELECT id, content FROM ai_messages WHERE status = 'streaming'")
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "could not read interrupted AI replies");
            return;
        }
    };
    for row in rows {
        let id: i64 = row.get("id");
        let mut content: Value =
            serde_json::from_str(&row.get::<String, _>("content")).unwrap_or_else(|_| json!([]));
        if let Some(blocks) = content.as_array_mut() {
            for block in blocks.iter_mut().filter(|b| super::is_tool_block(b)) {
                match block["status"].as_str() {
                    Some("running") => {
                        block["status"] = json!("error");
                        block["error"] = json!("Interrupted: RAST restarted while it ran.");
                    }
                    Some("pending" | "allowed") => block["status"] = json!("skipped"),
                    _ => {}
                }
            }
        }
        let result = sqlx::query(
            "UPDATE ai_messages SET status = 'error', content = ?,
             error = 'Interrupted: RAST restarted while this reply was written.', finished_at = ?
             WHERE id = ?",
        )
        .bind(content.to_string())
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "could not mark an interrupted AI reply");
        }
    }
}
