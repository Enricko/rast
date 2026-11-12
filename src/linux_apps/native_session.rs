//! Native Linux graphical sessions: **one** Xpra seamless session per RAST
//! user, shared by all of that person's native applications.
//!
//! Docker-streamed apps started one container per app and copy. Native apps
//! instead run as unprivileged host identities (`rast-u<user-id>`) and draw
//! into one Xpra seamless server per person: everything a person opens shares
//! the same display, clipboard and window manager. This module owns that
//! session's lifecycle:
//!
//! * lazy start on the first launch, reused by every later app (and by every
//!   browser window: the proxy attaches another client to the same session);
//! * display/port allocation under a process-wide lock;
//! * the privileged helper calls that create the identity, the runtime
//!   directory and the systemd units ([`Helper`] is the seam: production uses
//!   [`crate::native::client::NativeClient`], tests use a fake daemon);
//! * run bookkeeping in SQLite (`linux_sessions`, `linux_runs`, migration 12)
//!   written **before** a unit starts, so a restarted server can adopt what is
//!   still running;
//! * a sweeper that reconciles unit state, stops sessions that vanished, and
//!   ends idle sessions after [`EMPTY_GRACE`] plus the configured idle time;
//! * restart recovery (`start`), which adopts matching live units, stops
//!   orphans and drops database rows whose units are gone.
//!
//! Everything the browser needs is in [`NativeSession::json`]; the proxy in
//! [`super::native_proxy`] only needs [`NativeSession::upstream_port`] and the
//! activity counters.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    auth::AuthUser,
    config::Config,
    db,
    error::{AppError, AppResult},
    events::EventBus,
    linux_apps::SESSION_EVENT,
};

/// Every proxied Xpra page and asset lives under this prefix; the secret
/// session key follows it: `/linux-apps/session/<key>/…`.
pub const SESSION_PATH_PREFIX: &str = "/linux-apps/session/";

/// How often units and runs are reconciled with the helper.
const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// A session with no running app and no viewer waits this long before the idle
/// timer starts (a reload or a quick re-open must not restart the session).
const EMPTY_GRACE: Duration = Duration::from_secs(60);
/// How long a freshly started session may take to answer its web page.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// An ended session stays in the registry this long so the browser can show
/// why it ended; then it is forgotten.
const ENDED_LINGER: Duration = Duration::from_secs(5);
/// Attempts at finding a display/port that Xpra accepts.
const START_ATTEMPTS: u32 = 2;
/// Xpra displays are picked from this range (the server's X socket).
const DISPLAY_MIN: u32 = 100;
const DISPLAY_MAX: u32 = 999;
/// Length of the secret session key: 32 lowercase hex characters.
const KEY_HEX_CHARS: usize = 32;
/// Length of a run id: 16 lowercase hex characters.
const RUN_HEX_CHARS: usize = 16;
/// Longest app slug used in a unit name.
const UNIT_SLUG_MAX: usize = 20;
/// A unit-name collision (another copy still holds the sequence) is retried
/// with the next sequence this many times.
const MAX_UNIT_ATTEMPTS: u32 = 50;
/// Root of the per-user runtime directories (NOT `/run/rast`, which app
/// identities cannot traverse).
const RUNTIME_ROOT: &str = "/run/rast-linux";

// ---------------------------------------------------------------------------
// Helper seam
// ---------------------------------------------------------------------------

/// An error returned by the privileged helper. Codes mirror
/// [`crate::native::protocol::NativeError`].
#[derive(Debug, Clone)]
pub struct HelperError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HelperError {}

impl From<HelperError> for AppError {
    fn from(error: HelperError) -> Self {
        match error.code.as_str() {
            "invalid_request" | "unsupported" => AppError::bad_request(error.message),
            "unit_conflict" | "identity_in_use" => AppError::conflict(error.message),
            // Everything else is an unavailable/setup problem the person can
            // act on ("the session did not start", …).
            _ => AppError::unavailable(error.message),
        }
    }
}

/// The privileged operations this module needs.
///
/// The production implementation forwards [`crate::native::client::NativeClient::call_json`];
/// tests implement it in memory (including a fake Xpra upstream on the port the
/// manager allocated, so the real readiness probe runs).
#[async_trait]
pub trait Helper: Send + Sync + 'static {
    /// Run one helper operation. `params` is always a JSON object; the returned
    /// value is the response's `data` object.
    async fn call(&self, op: &str, params: Value) -> Result<Value, HelperError>;
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Everything needed to start one native application inside a person's
/// session. The caller (the app center) fills this from the app record, the
/// launcher and the per-app environment.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub app_id: String,
    pub app_name: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub tasks: u32,
    /// Chromium/Electron app: stale singleton files in its profile are removed
    /// before the launch so the browser can open a window.
    pub chromium: bool,
}

/// A person's host identity, as provisioned by the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    username: String,
    uid: u32,
    gid: u32,
    home: PathBuf,
}

/// Is the identity's home present, owned by the identity and 0700?
///
/// The server usually cannot stat inside the identity-owned tree, but the home
/// directory itself is a direct child of a traversable root, so its metadata
/// is readable. A home that was chowned by accident (for example by an
/// operator script that recursively chowned the data directory) breaks the
/// X11 auth file location and the profile environment; this check makes the
/// session manager fall back to the helper, which repairs it.
fn identity_home_is_healthy(home: &std::path::Path, uid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(home) {
        Ok(metadata) => {
            metadata.is_dir()
                && metadata.uid() == uid
                && metadata.permissions().mode() & 0o777 == 0o700
        }
        Err(_) => false,
    }
}

impl Identity {
    fn from_json(data: &Value) -> Option<Self> {
        Some(Self {
            username: data.get("username")?.as_str()?.to_string(),
            uid: u32::try_from(data.get("uid")?.as_u64()?).ok()?,
            gid: u32::try_from(data.get("gid")?.as_u64()?).ok()?,
            home: PathBuf::from(data.get("home")?.as_str()?),
        })
    }
}

impl NativeSession {
    /// The secret part of the session URL. Never log it.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The RAST account that owns this session.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// Loopback port of the Xpra web server, while the session runs.
    pub fn upstream_port(&self) -> Option<u16> {
        let state = self.lock();
        if state.status == SessionStatus::Ended {
            None
        } else {
            state.port
        }
    }

    /// A browser attached to the Xpra session.
    pub fn viewer_joined(&self) {
        let mut state = self.lock();
        state.viewers = state.viewers.saturating_add(1);
        state.empty_since = None;
        state.last_input = Instant::now();
    }

    /// A browser detached from the Xpra session.
    pub fn viewer_left(&self) {
        let mut state = self.lock();
        state.viewers = state.viewers.saturating_sub(1);
        if state.viewers == 0 && !state.running_apps() {
            state.empty_since = Some(Instant::now());
        }
    }

    /// Keyboard, mouse, clipboard or any other activity reached the session.
    pub fn input(&self) {
        self.lock().last_input = Instant::now();
    }

    /// The JSON the API and the owner event carry.
    pub fn json(&self) -> Value {
        let state = self.lock();
        json!({
            "id": "native",
            "status": state.status.as_str(),
            "url": format!("{SESSION_PATH_PREFIX}{}/", self.key),
            "appId": Value::Null,
            "appName": "Desktop session",
            "reason": state.reason,
            "error": state.error,
            "startedAt": state.started_at,
            "viewers": state.viewers,
            // Native apps draw their own top-level X11 windows; the browser
            // adopts each one in its own RAST window.
            "separateWindows": true,
            "display": state.display,
            "apps": state.apps.iter().map(NativeRun::json).collect::<Vec<Value>>(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn identity(&self) -> Identity {
        self.identity.clone()
    }

    fn status(&self) -> SessionStatus {
        self.lock().status
    }
}

/// One person's shared graphical session.
pub struct NativeSession {
    key: String,
    user_id: i64,
    identity: Identity,
    state: Mutex<SessionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Starting,
    Ready,
    Ended,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug)]
struct SessionState {
    status: SessionStatus,
    /// The X11 display number (`<display>` in `DISPLAY=:<display>`).
    display: u32,
    /// Loopback TCP port of the Xpra web server (also its WebSocket).
    port: Option<u16>,
    /// systemd transient unit of the Xpra session.
    unit: String,
    viewers: usize,
    /// Last input/activity from any viewer (idle bookkeeping).
    last_input: Instant,
    /// When the session became app-less and viewer-less (idle timer start).
    empty_since: Option<Instant>,
    started_at: i64,
    reason: Option<&'static str>,
    error: Option<String>,
    apps: Vec<NativeRun>,
    /// Set when the session ends; the registry keeps it briefly.
    ended_at: Option<Instant>,
}

impl SessionState {
    fn running_apps(&self) -> bool {
        self.apps.iter().any(|run| run.status.is_active())
    }
}

/// One application process inside a session.
#[derive(Debug, Clone)]
struct NativeRun {
    id: String,
    app_id: String,
    name: String,
    unit: String,
    status: RunStatus,
    error: Option<String>,
    started_at: i64,
    ended_at: Option<i64>,
}

impl NativeRun {
    fn json(&self) -> Value {
        json!({
            "runId": self.id,
            "appId": self.app_id,
            "name": self.name,
            "status": self.status.as_str(),
            "error": self.error,
            "startedAt": self.started_at,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Launching,
    Running,
    Exited,
    Failed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Launching | Self::Running)
    }
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

/// Owns every native graphical session of this server.
pub struct NativeSessions {
    config: Arc<Config>,
    db: SqlitePool,
    helper: Arc<dyn Helper>,
    events: EventBus,
    /// Sessions by key, plus the current session per user.
    inner: Mutex<Inner>,
    /// Per-user async locks: two launches for the same person share one
    /// session instead of racing to start two.
    user_locks: Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>,
    /// Serialises display and TCP port allocation across users.
    allocation: AsyncMutex<()>,
    started: AtomicBool,
    /// Enforce that an identity home is a 0700 directory owned by the identity.
    ///
    /// True in production. Tests run unprivileged and cannot create a directory
    /// owned by `linux_uid_base + userId`, so the mock constructor disables it;
    /// the decision logic itself is unit-tested via [`identity_home_is_healthy`].
    strict_identity_health: bool,
}

#[derive(Default)]
struct Inner {
    by_key: HashMap<String, Arc<NativeSession>>,
    by_user: HashMap<i64, Arc<NativeSession>>,
}

impl NativeSessions {
    /// Build the manager around the production helper client.
    pub fn new(
        config: Arc<Config>,
        db: SqlitePool,
        helper: crate::native::client::NativeClient,
        events: EventBus,
    ) -> Arc<Self> {
        Self::with_helper_options(config, db, Arc::new(ClientHelper(helper)), events, true)
    }

    /// Internal constructor used by tests (any [`Helper`] implementation).
    #[cfg(test)]
    fn with_helper(
        config: Arc<Config>,
        db: SqlitePool,
        helper: Arc<dyn Helper>,
        events: EventBus,
    ) -> Arc<Self> {
        // Mock environments cannot create identity-owned paths, so the strict
        // home-ownership check is disabled here (see `strict_identity_health`).
        Self::with_helper_options(config, db, helper, events, false)
    }

    fn with_helper_options(
        config: Arc<Config>,
        db: SqlitePool,
        helper: Arc<dyn Helper>,
        events: EventBus,
        strict_identity_health: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            helper,
            events,
            inner: Mutex::new(Inner::default()),
            user_locks: Mutex::new(HashMap::new()),
            allocation: AsyncMutex::new(()),
            started: AtomicBool::new(false),
            strict_identity_health,
        })
    }

    /// Adopt live units left by a previous process, then sweep in the
    /// background. Safe to call once; later calls do nothing.
    pub async fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.recover().await;
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                this.sweep().await;
            }
        });
    }

    /// Open `request` for `user`: start the shared session when needed, start
    /// the app inside it, and return `(run, session)`.
    ///
    /// Opening an app that already runs in the person's session returns the
    /// existing run instead of starting a second process.
    pub async fn launch(
        self: &Arc<Self>,
        user: &AuthUser,
        request: LaunchRequest,
    ) -> AppResult<(Value, Value)> {
        if request.command.is_empty() {
            return Err(AppError::bad_request("this app has no launch command"));
        }
        validate_app_id(&request.app_id)?;
        // Fast path: the app already runs for this person.
        if let Some((run, session)) = self.existing_run(user.id, &request.app_id) {
            return Ok((run, session));
        }
        // Memory admission happens before anything is created or started: the
        // new app must fit next to the reserve.
        let available = available_memory_bytes();
        if let Some(message) = admission_error(
            request.memory_mb,
            available,
            self.config.linux_admission_reserve_mb,
        ) {
            return Err(AppError::unavailable(message));
        }

        // Make sure the shared graphical session exists (and is ready).
        self.ensure_session(user).await?;
        // Serialise launches for one person so a double click cannot start two
        // processes for the same app.
        let user_lock = self.user_lock(user.id);
        let _guard = user_lock.lock().await;
        if let Some((run, session)) = self.existing_run(user.id, &request.app_id) {
            return Ok((run, session));
        }
        // The session may have ended while we waited for the lock.
        let session = self
            .session_of_user(user.id)
            .filter(|session| session.status() != SessionStatus::Ended)
            .ok_or_else(|| {
                AppError::unavailable(
                    "The graphical session ended. Open the app again to restart it.",
                )
            })?;

        let (run, session_json) = self.start_run(&session, user.id, &request).await?;
        Ok((run, session_json))
    }

    /// The person's session as JSON, when one exists (starting, ready or just
    /// ended).
    pub async fn session_json(&self, user: &AuthUser) -> AppResult<Option<Value>> {
        Ok(self.session_of_user(user.id).map(|session| session.json()))
    }

    /// Close one of the person's applications (its own close button normally
    /// does the same from inside).
    pub async fn close_app(&self, user: &AuthUser, run_id: &str) -> AppResult<Value> {
        let Some(session) = self.session_of_user(user.id) else {
            return Err(AppError::NotFound("this app is not running"));
        };
        let run = {
            let state = session.lock();
            state
                .apps
                .iter()
                .find(|run| run.id == run_id)
                .cloned()
                .ok_or(AppError::NotFound("this app is not running"))?
        };
        if run.status.is_active() {
            let _ = self
                .helper
                .call("app_stop", json!({"unit": run.unit}))
                .await;
        }
        let run = {
            let mut state = session.lock();
            let Some(entry) = state.apps.iter_mut().find(|entry| entry.id == run_id) else {
                return Err(AppError::NotFound("this app is not running"));
            };
            if entry.status.is_active() {
                entry.status = RunStatus::Exited;
                entry.ended_at = Some(db::now_ms());
            }
            entry.clone()
        };
        self.store_run(&run).await;
        self.publish_session(&session);
        Ok(run.json())
    }

    /// End the person's whole graphical session now.
    pub async fn close_session(self: &Arc<Self>, user: &AuthUser) -> AppResult<Value> {
        let Some(session) = self.session_of_user(user.id) else {
            return Ok(json!({"ok": true}));
        };
        self.end_session(&session, "closed", None).await;
        Ok(session.json())
    }

    /// Stop the person's session and every run in it by user id. Used when an
    /// administrator deactivates the account; the proxy already rejects the
    /// account's cookies, this releases the resources. Never fails when there
    /// is nothing to stop (idempotent, safe against concurrent closes).
    pub async fn close_for_user(self: &Arc<Self>, user_id: i64) -> AppResult<()> {
        if let Some(session) = self.session_of_user(user_id) {
            self.end_session(&session, "removed", None).await;
        }
        Ok(())
    }

    /// Number of running applications per app id (the app center's `running`
    /// badge).
    pub fn running_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        let inner = self.inner();
        for session in inner.by_user.values() {
            let state = session.lock();
            if state.status == SessionStatus::Ended {
                continue;
            }
            for run in &state.apps {
                if run.status.is_active() {
                    *counts.entry(run.app_id.clone()).or_insert(0) += 1;
                }
            }
        }
        counts
    }

    /// Stop every run of `app_id` for every person (called before an update or
    /// uninstall replaces the app).
    pub async fn stop_app_runs(self: &Arc<Self>, app_id: &str) {
        let sessions = self.all_sessions();
        for session in sessions {
            let runs: Vec<NativeRun> = {
                let state = session.lock();
                state
                    .apps
                    .iter()
                    .filter(|run| run.app_id == app_id && run.status.is_active())
                    .cloned()
                    .collect()
            };
            if runs.is_empty() {
                continue;
            }
            for run in &runs {
                let _ = self
                    .helper
                    .call("app_stop", json!({"unit": run.unit}))
                    .await;
            }
            {
                let mut state = session.lock();
                let now = db::now_ms();
                for entry in state.apps.iter_mut() {
                    if entry.app_id == app_id && entry.status.is_active() {
                        entry.status = RunStatus::Exited;
                        entry.ended_at = Some(now);
                    }
                }
            }
            for run in &runs {
                let stopped = NativeRun {
                    status: RunStatus::Exited,
                    ended_at: Some(db::now_ms()),
                    ..run.clone()
                };
                self.store_run(&stopped).await;
            }
            self.publish_session(&session);
        }
    }

    /// Stop every application and every session (server shutdown). Units are
    /// stopped best effort, then the database is cleaned so a restart starts
    /// from nothing.
    pub async fn stop_all(self: &Arc<Self>) {
        let sessions = self.all_sessions();
        for session in sessions {
            let (runs, user_id) = {
                let state = session.lock();
                (
                    state
                        .apps
                        .iter()
                        .filter(|run| run.status.is_active())
                        .cloned()
                        .collect::<Vec<NativeRun>>(),
                    session.user_id,
                )
            };
            for run in &runs {
                let _ = self
                    .helper
                    .call("app_stop", json!({"unit": run.unit}))
                    .await;
            }
            let _ = self
                .helper
                .call("session_stop", json!({"userId": user_id}))
                .await;
            {
                let mut state = session.lock();
                state.status = SessionStatus::Ended;
                state.reason = Some("closed");
                for run in state.apps.iter_mut() {
                    if run.status.is_active() {
                        run.status = RunStatus::Exited;
                        run.ended_at = Some(db::now_ms());
                    }
                }
            }
        }
        let now = db::now_ms();
        let _ = sqlx::query("DELETE FROM linux_sessions")
            .execute(&self.db)
            .await;
        let _ = sqlx::query(
            "UPDATE linux_runs SET status = 'exited', ended_at = COALESCE(ended_at, ?)
             WHERE status IN ('launching', 'running')",
        )
        .bind(now)
        .execute(&self.db)
        .await;
        let mut inner = self.inner();
        inner.by_key.clear();
        inner.by_user.clear();
    }

    /// A session by its secret key (used by the proxy).
    pub fn find_session(&self, key: &str) -> Option<Arc<NativeSession>> {
        self.inner().by_key.get(key).cloned()
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn user_lock(&self, user_id: i64) -> Arc<AsyncMutex<()>> {
        let mut locks = self.user_locks.lock().unwrap_or_else(|p| p.into_inner());
        locks
            .entry(user_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn session_of_user(&self, user_id: i64) -> Option<Arc<NativeSession>> {
        self.inner().by_user.get(&user_id).cloned()
    }

    fn all_sessions(&self) -> Vec<Arc<NativeSession>> {
        self.inner().by_key.values().cloned().collect()
    }

    fn insert_session(&self, session: Arc<NativeSession>) {
        let mut inner = self.inner();
        inner
            .by_key
            .insert(session.key().to_string(), session.clone());
        inner.by_user.insert(session.user_id(), session);
    }

    fn forget_session(&self, session: &Arc<NativeSession>) {
        let mut inner = self.inner();
        inner.by_key.remove(session.key());
        if inner
            .by_user
            .get(&session.user_id())
            .is_some_and(|current| current.key() == session.key())
        {
            inner.by_user.remove(&session.user_id());
        }
    }

    /// The existing run of `app_id` for `user_id`, if it is still active.
    fn existing_run(&self, user_id: i64, app_id: &str) -> Option<(Value, Value)> {
        let session = self.session_of_user(user_id)?;
        // Never call `session.json()` while holding the state lock (it locks
        // again); take the run out first.
        let run = {
            let state = session.lock();
            if state.status == SessionStatus::Ended {
                return None;
            }
            state
                .apps
                .iter()
                .find(|run| run.app_id == app_id && run.status.is_active())?
                .clone()
        };
        Some((run.json(), session.json()))
    }

    fn publish_session(&self, session: &NativeSession) {
        self.events.publish_user(
            SESSION_EVENT,
            session.user_id(),
            json!({"session": session.json()}),
        );
    }

    // -----------------------------------------------------------------------
    // Session start
    // -----------------------------------------------------------------------

    /// The person's running session, starting it when needed. Race-free: the
    /// per-user lock plus the map re-check make concurrent callers share one
    /// session and one helper `session_start`.
    async fn ensure_session(self: &Arc<Self>, user: &AuthUser) -> AppResult<Arc<NativeSession>> {
        if let Some(session) = self.session_of_user(user.id) {
            if session.status() != SessionStatus::Ended {
                if let Some(session) = self.reusable_session(user, &session).await? {
                    return Ok(session);
                }
            }
        }
        let lock = self.user_lock(user.id);
        let _guard = lock.lock().await;
        if let Some(session) = self.session_of_user(user.id) {
            if session.status() != SessionStatus::Ended {
                if let Some(session) = self.reusable_session(user, &session).await? {
                    return Ok(session);
                }
            }
        }
        let (identity, _repaired) = self.ensure_identity(user.id).await?;
        let runtime = runtime_dir(identity.uid);
        self.prepare_runtime(&identity, &runtime).await?;

        let mut last_error = String::new();
        for _attempt in 0..START_ATTEMPTS {
            let (display_number, port) = self.allocate_display().await?;
            let unit = session_unit(user.id);
            let key = random_hex(KEY_HEX_CHARS / 2);
            let started_at = db::now_ms();
            let session = Arc::new(NativeSession {
                key: key.clone(),
                user_id: user.id,
                identity: identity.clone(),
                state: Mutex::new(SessionState {
                    status: SessionStatus::Starting,
                    display: display_number,
                    port: Some(port),
                    unit: unit.clone(),
                    viewers: 0,
                    last_input: Instant::now(),
                    empty_since: None,
                    started_at,
                    reason: None,
                    error: None,
                    apps: Vec::new(),
                    ended_at: None,
                }),
            });
            // Persist before starting, so a restart can recover the unit.
            self.store_session_start(&session).await?;
            self.insert_session(session.clone());
            self.publish_session(&session);
            tracing::info!(
                user_id = user.id,
                display = display_number,
                port,
                "starting a native graphical session"
            );

            let args = xpra_args(display_number, port, &runtime);
            let params = json!({
                "userId": user.id,
                "display": display_number,
                "port": port,
                "xpraBin": self.config.linux_xpra_bin.display().to_string(),
                "runtimeDir": runtime.display().to_string(),
                "args": args,
                "env": session_env(&identity, &runtime),
                "memoryMaxMb": self.config.linux_session_memory_mb,
                "cpuQuotaPercent": self.config.linux_session_cpus.saturating_mul(100),
                "tasksMax": self.config.linux_session_tasks,
            });
            match self.helper.call("session_start", params).await {
                Ok(_) => {}
                Err(error) => {
                    let message = format!("Could not start the graphical session: {error}");
                    self.end_session(&session, "error", Some(message.clone()))
                        .await;
                    return Err(AppError::unavailable(message));
                }
            }
            match wait_ready(port, READY_TIMEOUT).await {
                Ok(()) => {
                    {
                        let mut state = session.lock();
                        if state.status == SessionStatus::Ended {
                            return Err(AppError::unavailable(
                                "The graphical session was closed while starting.",
                            ));
                        }
                        state.status = SessionStatus::Ready;
                        state.last_input = Instant::now();
                        state.empty_since = Some(Instant::now());
                    }
                    let _ = sqlx::query(
                        "UPDATE linux_sessions SET status = 'ready', updated_at = ? WHERE user_id = ?",
                    )
                    .bind(db::now_ms())
                    .bind(user.id)
                    .execute(&self.db)
                    .await;
                    self.publish_session(&session);
                    return Ok(session);
                }
                Err(message) => {
                    last_error = message.clone();
                    tracing::warn!(
                        user_id = user.id,
                        display = display_number,
                        port,
                        "the native graphical session did not become ready: {message}"
                    );
                    self.end_session(&session, "error", Some(message)).await;
                    // Another attempt with a different display may succeed
                    // (e.g. a display was taken between scan and start).
                }
            }
        }
        Err(AppError::unavailable(format!(
            "The graphical session did not start: {last_error}"
        )))
    }

    /// Can a live session be reused for this launch?
    ///
    /// The identity is validated first: a repaired or changed identity means the
    /// session's Xvfb auth file (which lives in the identity home) may be stale,
    /// so the session is ended and the caller starts a fresh one. That is the
    /// self-healing path for sessions started before a home-permission repair.
    async fn reusable_session(
        self: &Arc<Self>,
        user: &AuthUser,
        session: &Arc<NativeSession>,
    ) -> AppResult<Option<Arc<NativeSession>>> {
        let (identity, repaired) = self.ensure_identity(user.id).await?;
        if !repaired && session.identity() == identity {
            return Ok(Some(session.clone()));
        }
        tracing::warn!(
            user_id = user.id,
            repaired,
            "the native identity was repaired or changed; restarting the graphical session so its X auth is regenerated"
        );
        self.end_session(
            session,
            "error",
            Some(
                "The native Linux identity was repaired; the graphical session is restarting."
                    .into(),
            ),
        )
        .await;
        Ok(None)
    }

    /// Validate/provision the person's Linux identity and cache it in SQLite.
    ///
    /// `useradd`/`groupadd` take several seconds on some hosts, so a cached
    /// identity whose home is still healthy is used directly: warm session
    /// starts must not wait for the helper. Returns `(identity, repaired)`;
    /// `repaired` is true when the cached identity was stale (the helper had to
    /// re-provision its home), which means any live graphical session must be
    /// restarted so its Xvfb auth file is regenerated.
    async fn ensure_identity(&self, user_id: i64) -> AppResult<(Identity, bool)> {
        let mut repaired = false;
        if let Some(identity) = self.stored_identity(user_id).await {
            if !self.strict_identity_health
                || identity_home_is_healthy(&identity.home, identity.uid)
            {
                self.validate_identity_mapping(user_id, &identity)?;
                return Ok((identity, false));
            }
            tracing::info!(
                user_id,
                "the cached native identity home is missing or not owned by the identity; repairing it through the helper"
            );
            repaired = true;
        }
        let data = self
            .helper
            .call("identity_ensure", json!({"userId": user_id}))
            .await?;
        let identity = Identity::from_json(&data)
            .ok_or_else(|| AppError::internal("the native helper returned an invalid identity"))?;
        self.validate_identity_mapping(user_id, &identity)?;
        if self.strict_identity_health && !identity_home_is_healthy(&identity.home, identity.uid) {
            return Err(AppError::unavailable(format!(
                "The Linux identity for this account is not usable: {} must be a 0700 \
                 directory owned by uid {} (user id {user_id}). The native helper could not \
                 repair it; restart rast-native.service with the current RAST build or fix \
                 that directory's ownership.",
                identity.home.display(),
                identity.uid
            )));
        }
        let _ = sqlx::query(
            "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
                 username = excluded.username, uid = excluded.uid, gid = excluded.gid,
                 home = excluded.home",
        )
        .bind(user_id)
        .bind(&identity.username)
        .bind(identity.uid as i64)
        .bind(identity.gid as i64)
        .bind(identity.home.display().to_string())
        .bind(db::now_ms())
        .execute(&self.db)
        .await;
        Ok((identity, repaired))
    }

    /// The helper must map a RAST user to `linux_uid_base + userId` with the
    /// `rast-u<userId>` username; anything else would start units and create
    /// profiles for the wrong account.
    fn validate_identity_mapping(&self, user_id: i64, identity: &Identity) -> AppResult<()> {
        let expected_uid = u32::try_from(user_id)
            .ok()
            .and_then(|id| self.config.linux_uid_base.checked_add(id));
        let Some(expected_uid) = expected_uid else {
            return Err(AppError::unavailable(format!(
                "User id {user_id} cannot be mapped to a Linux uid with linux_uid_base {}.",
                self.config.linux_uid_base
            )));
        };
        if identity.username != format!("rast-u{user_id}") || identity.uid != expected_uid {
            return Err(AppError::unavailable(format!(
                "The native helper returned an inconsistent Linux identity (username {:?}, \
                 uid {}); expected rast-u{user_id} with uid {} (linux_uid_base {} + user id \
                 {user_id}). Restart rast-native.service so it runs the same version as RAST.",
                identity.username, identity.uid, expected_uid, self.config.linux_uid_base
            )));
        }
        if identity.gid != identity.uid {
            return Err(AppError::unavailable(format!(
                "The native Linux identity rast-u{user_id} has gid {} but uid {}; expected \
                 gid = uid.",
                identity.gid, identity.uid
            )));
        }
        Ok(())
    }

    /// The identity last provisioned for this account, if it is still known.
    async fn stored_identity(&self, user_id: i64) -> Option<Identity> {
        let row: IdentityRow = sqlx::query_as(
            "SELECT username, uid, gid, home FROM linux_identities WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()?;
        Some(Identity {
            username: row.username,
            uid: u32::try_from(row.uid).ok()?,
            gid: u32::try_from(row.gid).ok()?,
            home: PathBuf::from(row.home),
        })
    }

    /// Create `/run/rast-linux/<uid>` and its writable subdirectories (0700,
    /// owned by the identity) before Xpra starts.
    async fn prepare_runtime(&self, identity: &Identity, runtime: &Path) -> AppResult<()> {
        let mut dirs = vec![
            runtime.to_path_buf(),
            runtime.join("sockets"),
            runtime.join("tmp"),
            runtime.join("cache"),
        ];
        // Deduplicate while keeping order (runtime first: the helper creates
        // parents as root and only chowns the final component).
        let mut seen = HashSet::new();
        dirs.retain(|dir| seen.insert(dir.clone()));
        for dir in dirs {
            let params = json!({
                "path": dir.display().to_string(),
                "uid": identity.uid,
                "gid": identity.gid,
                "mode": 0o700,
            });
            self.helper
                .call("path_mkdir", params)
                .await
                .map_err(|error| {
                    AppError::unavailable(format!(
                        "Could not prepare the session directory {}: {error}",
                        dir.display()
                    ))
                })?;
        }
        Ok(())
    }

    /// Is `path` a real directory the helper accepts as an application cwd
    /// (i.e. inside the managed app trees, not a RAST home or a system path)?
    fn managed_cwd(&self, path: &Path) -> bool {
        if !path.is_absolute() || !path.is_dir() {
            return false;
        }
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return false;
        };
        let apps = self.config.linux_apps_dir();
        let users = self.config.data_dir.join("linux-users");
        let apps = std::fs::canonicalize(&apps).unwrap_or(apps);
        let users = std::fs::canonicalize(&users).unwrap_or(users);
        canonical.starts_with(&apps) || canonical.starts_with(&users)
    }

    /// Pick the next free X11 display and loopback TCP port. A process-wide
    /// lock keeps concurrent first launches apart; the display is re-scanned
    /// for X sockets and lock files, and sessions in memory are skipped too.
    async fn allocate_display(&self) -> AppResult<(u32, u16)> {
        let _guard = self.allocation.lock().await;
        let in_use: HashSet<u32> = {
            let inner = self.inner();
            inner
                .by_key
                .values()
                .map(|session| session.lock().display)
                .collect()
        };
        for display in DISPLAY_MIN..=DISPLAY_MAX {
            if in_use.contains(&display) || display_in_use(display) {
                continue;
            }
            if let Some(port) = free_loopback_port().await {
                return Ok((display, port));
            }
        }
        Err(AppError::unavailable(
            "No free display is available for another graphical session.",
        ))
    }

    async fn store_session_start(&self, session: &NativeSession) -> AppResult<()> {
        let (status, display, port, unit, started_at) = {
            let state = session.lock();
            (
                state.status.as_str(),
                state.display,
                state.port.unwrap_or_default(),
                state.unit.clone(),
                state.started_at,
            )
        };
        sqlx::query(
            "INSERT INTO linux_sessions
                 (user_id, key, display, port, unit, status, started_at, last_input, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
                 key = excluded.key, display = excluded.display, port = excluded.port,
                 unit = excluded.unit, status = excluded.status,
                 started_at = excluded.started_at, last_input = excluded.last_input,
                 updated_at = excluded.updated_at",
        )
        .bind(session.user_id())
        .bind(session.key())
        .bind(display as i64)
        .bind(port as i64)
        .bind(unit)
        .bind(status)
        .bind(started_at)
        .bind(started_at)
        .bind(db::now_ms())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Application start
    // -----------------------------------------------------------------------

    async fn start_run(
        self: &Arc<Self>,
        session: &Arc<NativeSession>,
        user_id: i64,
        request: &LaunchRequest,
    ) -> AppResult<(Value, Value)> {
        let identity = session.identity();
        let display = session.lock().display;
        let profile_root = self
            .config
            .linux_apps_dir()
            .join("profiles")
            .join(&request.app_id)
            .join(user_id.to_string());
        // `profile_prepare` takes the *identity* home (the helper validates it
        // against its own <data_dir>/linux-users/<userId>) and creates the
        // per-app profile; it returns the profile HOME the app must use.
        let identity_home = identity.home.display().to_string();
        let profile_home = profile_root.join("home");
        let rast_home = self.config.homes_dir().join(&identity.username);
        // The helper only accepts an existing RAST home; create the person's
        // home folder when it has none yet (the server owns the homes root).
        if !rast_home.is_dir() {
            if let Err(error) = std::fs::create_dir_all(&rast_home) {
                tracing::warn!(
                    user_id,
                    "cannot create the RAST home {}: {error}",
                    rast_home.display()
                );
            }
        }

        let prepared = self
            .helper
            .call(
                "profile_prepare",
                json!({
                    "appId": request.app_id,
                    "userId": user_id,
                    "uid": identity.uid,
                    "gid": identity.gid,
                    "home": identity_home,
                    "rastHome": rast_home.display().to_string(),
                }),
            )
            .await?;
        let home = prepared
            .get("home")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or(profile_home);
        // Do not let a broken profile reach systemd (it would fail with an
        // opaque "CHDIR: Permission denied"). The profile tree belongs to the
        // app identity with mode 0700, so the unprivileged server cannot always
        // stat inside it: a successful `profile_prepare` is authoritative and
        // permission-denied means "exists but not ours to inspect". Only a
        // definite "missing" is turned into an actionable error.
        match std::fs::metadata(&home) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(AppError::unavailable(format!(
                    "The application profile directory {} is not a directory. \
                     Ask an administrator to check the ownership of {}.",
                    home.display(),
                    profile_root.display()
                )))
            }
            // The profile tree is 0700 for the app identity: the server is
            // expected to be denied. Any other error (notably "missing") is a
            // preparation failure worth reporting before systemd runs.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) => {
                return Err(AppError::unavailable(format!(
                    "The application profile directory {} could not be checked: {error}. \
                     Ask an administrator to check the ownership of {}.",
                    home.display(),
                    profile_root.display()
                )))
            }
        }

        // The person's RAST home is linked into the app's profile and made
        // readable by the app identity. Both are best effort: an app that does
        // not need file access must still start.
        let link = profile_root.join("RAST");
        if let Err(error) = self
            .helper
            .call(
                "path_symlink",
                json!({
                    "link": link.display().to_string(),
                    "target": rast_home.display().to_string(),
                }),
            )
            .await
        {
            tracing::debug!(
                app = %request.app_id,
                user_id,
                "could not link the RAST home into the app profile: {error}"
            );
        }
        if rast_home.is_dir() {
            if let Err(error) = self
                .helper
                .call(
                    "path_acl",
                    json!({
                        "path": rast_home.display().to_string(),
                        "uid": identity.uid,
                        "perms": "rx",
                        "recursive": true,
                    }),
                )
                .await
            {
                tracing::warn!(
                    user_id,
                    "could not grant the app identity access to the RAST home: {error}"
                );
            }
        }

        // The helper's cwd allow-list covers the managed app trees, not the
        // RAST homes root, so a launcher working directory is only used when
        // it is an existing directory inside those trees; otherwise the app
        // starts in its profile HOME (the unit would fail with an opaque
        // CHDIR error otherwise).
        let cwd = match request.cwd.as_deref() {
            Some(cwd) if self.managed_cwd(cwd) => cwd.to_path_buf(),
            Some(cwd) => {
                tracing::info!(
                    app = %request.app_id,
                    user_id,
                    working_dir = %cwd.display(),
                    "ignoring the launcher working directory (outside the managed app tree)"
                );
                home.clone()
            }
            None => home.clone(),
        };

        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("DISPLAY".into(), format!(":{display}"));
        // The session's X server uses the auth file the helper pins as the
        // session `XAUTHORITY`: the identity home's `.Xauthority` (Xpra appends
        // the cookie for the allocated display there). Applications must use
        // the same file; pointing at Xpra's runtime files is wrong because
        // stale `Xauthority-<old display>` files from earlier sessions remain
        // in the runtime directory.
        env.insert(
            "XAUTHORITY".into(),
            identity.home.join(".Xauthority").display().to_string(),
        );
        env.insert("HOME".into(), home.display().to_string());
        env.insert(
            "XDG_CONFIG_HOME".into(),
            profile_root.join("config").display().to_string(),
        );
        env.insert(
            "XDG_DATA_HOME".into(),
            profile_root.join("data").display().to_string(),
        );
        env.insert(
            "XDG_CACHE_HOME".into(),
            profile_root.join("cache").display().to_string(),
        );
        env.insert(
            "XDG_STATE_HOME".into(),
            profile_root.join("state").display().to_string(),
        );
        env.insert(
            "TMPDIR".into(),
            profile_root.join("tmp").display().to_string(),
        );
        for (name, value) in &request.env {
            env.insert(name.clone(), value.clone());
        }

        let run_id = random_hex(RUN_HEX_CHARS / 2);
        let started_at = db::now_ms();
        let slug = app_slug(&request.app_id);
        let mut sequence = self.next_run_sequence(user_id, &request.app_id).await?;
        let mut unit = app_unit(identity.uid, &slug, sequence);
        let run = NativeRun {
            id: run_id.clone(),
            app_id: request.app_id.clone(),
            name: request.app_name.clone(),
            unit: unit.clone(),
            status: RunStatus::Launching,
            error: None,
            started_at,
            ended_at: None,
        };
        // Persist the run before starting it so a crash during startup is
        // reconciled instead of leaving a forgotten process behind.
        sqlx::query(
            "INSERT INTO linux_runs (id, app_id, user_id, unit, status, error, started_at)
             VALUES (?, ?, ?, ?, 'launching', NULL, ?)",
        )
        .bind(&run.id)
        .bind(&run.app_id)
        .bind(user_id)
        .bind(&run.unit)
        .bind(started_at)
        .execute(&self.db)
        .await?;
        session.lock().apps.push(run.clone());
        self.publish_session(session);

        // Browsers that were killed or stopped leave Chromium/Electron
        // singleton files in their profile; the next launch would then hang or
        // delegate to a dead instance instead of opening a window (the legacy
        // Docker image cleaned these in its init script). The app is not
        // running at this point (launch is idempotent), so this is safe.
        if request.chromium {
            if let Err(error) = self
                .helper
                .call(
                    "profile_clean_locks",
                    json!({"appId": request.app_id, "userId": user_id}),
                )
                .await
            {
                tracing::debug!(
                    app = %request.app_id,
                    user_id,
                    "could not remove stale browser singleton files: {error}"
                );
            }
        }

        // A unit name left active by a crash is retried with the next
        // sequence instead of failing the launch.
        let mut attempt = 0;
        loop {
            let params = json!({
                "userId": user_id,
                "appId": request.app_id,
                "runId": run.id,
                "unit": unit,
                "argv": request.command,
                "cwd": cwd.display().to_string(),
                "env": env,
                "memoryMaxMb": request.memory_mb,
                "cpuQuotaPercent": request.cpus.saturating_mul(100),
                "tasksMax": request.tasks,
            });
            match self.helper.call("app_start", params).await {
                Ok(_) => break,
                Err(error) if error.code == "unit_conflict" && attempt < MAX_UNIT_ATTEMPTS => {
                    attempt += 1;
                    sequence += 1;
                    unit = app_unit(identity.uid, &slug, sequence);
                    {
                        let mut state = session.lock();
                        if let Some(entry) = state.apps.iter_mut().find(|entry| entry.id == run.id)
                        {
                            entry.unit = unit.clone();
                        }
                    }
                    let _ = sqlx::query("UPDATE linux_runs SET unit = ? WHERE id = ?")
                        .bind(&unit)
                        .bind(&run.id)
                        .execute(&self.db)
                        .await;
                }
                Err(error) => {
                    let stale_xauthority = error.code == "xauthority_unavailable";
                    let message = if error.code == "command_failed"
                        && (error.message.contains("CHDIR")
                            || error
                                .message
                                .to_ascii_lowercase()
                                .contains("working directory"))
                    {
                        format!(
                            "the app could not start because its profile directory {} was not \
                             accessible ({})",
                            home.display(),
                            error.message
                        )
                    } else {
                        error.message.clone()
                    };
                    let failed = {
                        let mut state = session.lock();
                        let Some(entry) = state.apps.iter_mut().find(|entry| entry.id == run.id)
                        else {
                            return Err(AppError::unavailable(message));
                        };
                        entry.status = RunStatus::Failed;
                        entry.error = Some(message.clone());
                        entry.ended_at = Some(db::now_ms());
                        entry.clone()
                    };
                    self.store_run(&failed).await;
                    self.publish_session(session);
                    if stale_xauthority {
                        // The helper's own advice: stop the session so Xpra
                        // regenerates the auth file. Do it now, so reopening
                        // the app starts a fresh session instead of failing
                        // again.
                        self.end_session(
                            session,
                            "error",
                            Some(
                                "The graphical session's X authentication file was stale; the \
                                 session was restarted."
                                    .into(),
                            ),
                        )
                        .await;
                        return Err(AppError::unavailable(format!(
                            "Could not start {}: {message} The graphical session has been \
                             restarted; open the app again.",
                            request.app_name
                        )));
                    }
                    return Err(AppError::unavailable(format!(
                        "Could not start {}: {message}",
                        request.app_name
                    )));
                }
            }
        }

        let running = {
            let mut state = session.lock();
            let Some(entry) = state.apps.iter_mut().find(|entry| entry.id == run.id) else {
                return Err(AppError::unavailable(
                    "The graphical session ended while starting the app.",
                ));
            };
            entry.status = RunStatus::Running;
            entry.unit = unit;
            entry.clone()
        };
        self.store_run(&running).await;
        self.publish_session(session);
        tracing::info!(
            app = %request.app_id,
            user_id,
            unit = %running.unit,
            "started a native application in the share graphical session"
        );
        Ok((running.json(), session.json()))
    }

    /// Persist one run row (`status`, `unit`, `error`, `ended_at`).
    async fn store_run(&self, run: &NativeRun) {
        let _ = sqlx::query(
            "UPDATE linux_runs SET unit = ?, status = ?, error = ?, ended_at = ? WHERE id = ?",
        )
        .bind(&run.unit)
        .bind(run.status.as_str())
        .bind(&run.error)
        .bind(run.ended_at)
        .bind(&run.id)
        .execute(&self.db)
        .await;
    }

    async fn next_run_sequence(&self, user_id: i64, app_id: &str) -> AppResult<u32> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM linux_runs WHERE user_id = ? AND app_id = ?")
                .bind(user_id)
                .bind(app_id)
                .fetch_one(&self.db)
                .await
                .unwrap_or(0);
        Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
    }

    // -----------------------------------------------------------------------
    // Ending sessions
    // -----------------------------------------------------------------------

    /// End one session: stop its units, clean the database and tell the owner.
    async fn end_session(
        self: &Arc<Self>,
        session: &Arc<NativeSession>,
        reason: &'static str,
        error: Option<String>,
    ) {
        let (user_id, runs) = {
            let mut state = session.lock();
            if state.status == SessionStatus::Ended {
                return;
            }
            state.status = SessionStatus::Ended;
            state.reason = Some(reason);
            state.error = error;
            state.ended_at = Some(Instant::now());
            let now = db::now_ms();
            for run in state.apps.iter_mut() {
                if run.status.is_active() {
                    run.status = RunStatus::Exited;
                    run.ended_at = Some(now);
                }
            }
            (session.user_id(), state.apps.clone())
        };
        let _ = sqlx::query("DELETE FROM linux_sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.db)
            .await;
        for run in &runs {
            let _ = sqlx::query(
                "UPDATE linux_runs SET status = ?, error = ?, ended_at = COALESCE(ended_at, ?)
                 WHERE id = ?",
            )
            .bind(run.status.as_str())
            .bind(&run.error)
            .bind(run.ended_at.unwrap_or_else(db::now_ms))
            .bind(&run.id)
            .execute(&self.db)
            .await;
        }
        for run in runs.iter().filter(|run| run.status == RunStatus::Exited) {
            // The app may already be gone; the helper ignores unknown units.
            let _ = self
                .helper
                .call("app_stop", json!({"unit": run.unit}))
                .await;
        }
        let _ = self
            .helper
            .call("session_stop", json!({"userId": user_id}))
            .await;
        tracing::info!(user_id, reason, "native graphical session ended");
        self.publish_session(session);
    }

    // -----------------------------------------------------------------------
    // Recovery and sweeping
    // -----------------------------------------------------------------------

    /// Adopt live units from a previous RAST process and stop everything that
    /// has no matching database row, then forget rows whose units are gone.
    async fn recover(self: &Arc<Self>) {
        let units = match self.units().await {
            Ok(units) => units,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "cannot list native units during recovery; keeping the database as it is"
                );
                return;
            }
        };
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT user_id, key, display, port, unit, started_at FROM linux_sessions",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        let mut adopted: HashMap<i64, Arc<NativeSession>> = HashMap::new();
        for row in rows {
            let display_number = u32::try_from(row.display).unwrap_or(0);
            let port = u16::try_from(row.port).unwrap_or(0);
            let unit = if row.unit.is_empty() {
                session_unit(row.user_id)
            } else {
                row.unit.clone()
            };
            let active = units.get(&unit).is_some_and(|unit| unit.active);
            if !active || port == 0 || display_number == 0 {
                tracing::info!(
                    user_id = row.user_id,
                    "dropping a native session row whose unit is gone"
                );
                let _ = self
                    .helper
                    .call("session_stop", json!({"userId": row.user_id}))
                    .await;
                let _ = sqlx::query("DELETE FROM linux_sessions WHERE user_id = ?")
                    .bind(row.user_id)
                    .execute(&self.db)
                    .await;
                continue;
            }
            let identity = match self.ensure_identity(row.user_id).await {
                Ok((identity, repaired)) => {
                    if repaired {
                        tracing::info!(
                            user_id = row.user_id,
                            "the native identity was repaired; not adopting the stale session \
                             (a fresh session regenerates its X auth)"
                        );
                        let _ = self
                            .helper
                            .call("session_stop", json!({"userId": row.user_id}))
                            .await;
                        let _ = sqlx::query("DELETE FROM linux_sessions WHERE user_id = ?")
                            .bind(row.user_id)
                            .execute(&self.db)
                            .await;
                        continue;
                    }
                    identity
                }
                Err(error) => {
                    tracing::warn!(
                        user_id = row.user_id,
                        error = %error,
                        "cannot recreate the session identity; stopping the session"
                    );
                    let _ = self
                        .helper
                        .call("session_stop", json!({"userId": row.user_id}))
                        .await;
                    let _ = sqlx::query("DELETE FROM linux_sessions WHERE user_id = ?")
                        .bind(row.user_id)
                        .execute(&self.db)
                        .await;
                    continue;
                }
            };
            if wait_ready(port, READY_TIMEOUT).await.is_err() {
                tracing::warn!(
                    user_id = row.user_id,
                    port,
                    "an adopted native session is not answering; stopping it"
                );
                let _ = self
                    .helper
                    .call("session_stop", json!({"userId": row.user_id}))
                    .await;
                let _ = sqlx::query("DELETE FROM linux_sessions WHERE user_id = ?")
                    .bind(row.user_id)
                    .execute(&self.db)
                    .await;
                continue;
            }
            let session = Arc::new(NativeSession {
                key: row.key.clone(),
                user_id: row.user_id,
                identity,
                state: Mutex::new(SessionState {
                    status: SessionStatus::Ready,
                    display: display_number,
                    port: Some(port),
                    unit,
                    viewers: 0,
                    last_input: Instant::now(),
                    // Adopted sessions start their idle timer immediately:
                    // a restart must not leave permanent empty sessions.
                    empty_since: Some(Instant::now()),
                    started_at: row.started_at,
                    reason: None,
                    error: None,
                    apps: Vec::new(),
                    ended_at: None,
                }),
            });
            let _ = sqlx::query(
                "UPDATE linux_sessions SET status = 'ready', updated_at = ? WHERE user_id = ?",
            )
            .bind(db::now_ms())
            .bind(row.user_id)
            .execute(&self.db)
            .await;
            tracing::info!(
                user_id = row.user_id,
                display = display_number,
                port,
                "adopted a running native graphical session"
            );
            self.insert_session(session.clone());
            adopted.insert(row.user_id, session);
        }

        // Adopt app runs, or mark them ended when their unit is gone.
        let runs: Vec<RunRow> = sqlx::query_as(
            "SELECT id, app_id, user_id, unit, started_at FROM linux_runs
             WHERE status IN ('launching', 'running')",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        let mut live_units: HashSet<String> = HashSet::new();
        for row in runs {
            let active = units.get(&row.unit).is_some_and(|unit| unit.active);
            let Some(session) = adopted.get(&row.user_id) else {
                if active {
                    let _ = self
                        .helper
                        .call("app_stop", json!({"unit": row.unit}))
                        .await;
                }
                let _ = sqlx::query(
                    "UPDATE linux_runs SET status = 'exited', ended_at = ? WHERE id = ?",
                )
                .bind(db::now_ms())
                .bind(&row.id)
                .execute(&self.db)
                .await;
                continue;
            };
            if active {
                live_units.insert(row.unit.clone());
                let name = app_name(&self.db, &row.app_id).await;
                let run = NativeRun {
                    id: row.id,
                    app_id: row.app_id,
                    name,
                    unit: row.unit,
                    status: RunStatus::Running,
                    error: None,
                    started_at: row.started_at,
                    ended_at: None,
                };
                session.lock().apps.push(run.clone());
                self.store_run(&run).await;
            } else {
                let _ = sqlx::query(
                    "UPDATE linux_runs SET status = 'exited', ended_at = ? WHERE id = ?",
                )
                .bind(db::now_ms())
                .bind(&row.id)
                .execute(&self.db)
                .await;
            }
        }

        // Stop orphans: units with no matching database row.
        for (unit, info) in &units {
            if !info.active {
                continue;
            }
            if unit.starts_with("rast-app-") && !live_units.contains(unit) {
                tracing::info!(unit, "stopping an orphaned native app unit");
                let _ = self.helper.call("app_stop", json!({"unit": unit})).await;
            } else if let Some(user_id) = session_unit_user(unit) {
                if !adopted.contains_key(&user_id) {
                    tracing::info!(unit, user_id, "stopping an orphaned session unit");
                    let _ = self
                        .helper
                        .call("session_stop", json!({"userId": user_id}))
                        .await;
                }
            }
        }
        for session in adopted.values() {
            self.publish_session(session);
        }
    }

    /// One reconciliation pass: refresh run states from the helper, apply the
    /// idle rules, notice vanished sessions and forget old ended ones.
    async fn sweep(self: &Arc<Self>) {
        let units = match self.units().await {
            Ok(units) => units,
            Err(error) => {
                tracing::debug!(error = %error, "cannot list native units; skipping this sweep");
                return;
            }
        };
        let sessions = self.all_sessions();
        let mut to_end: Vec<(Arc<NativeSession>, &'static str, Option<String>)> = Vec::new();
        for session in &sessions {
            let (status, unit) = {
                let state = session.lock();
                (state.status, state.unit.clone())
            };
            match status {
                SessionStatus::Ended => continue,
                SessionStatus::Starting => {
                    let started_at = session.lock().started_at;
                    if db::now_ms() - started_at > READY_TIMEOUT.as_millis() as i64 + 30_000 {
                        to_end.push((
                            session.clone(),
                            "error",
                            Some("The graphical session did not start in time.".into()),
                        ));
                    }
                    continue;
                }
                SessionStatus::Ready => {}
            }
            if !units.get(&unit).is_some_and(|unit| unit.active) {
                to_end.push((
                    session.clone(),
                    "error",
                    Some("The graphical session stopped unexpectedly.".into()),
                ));
                continue;
            }
            let mut changed = false;
            let mut idle = false;
            {
                let mut state = session.lock();
                let now = db::now_ms();
                for run in state.apps.iter_mut() {
                    if !run.status.is_active() {
                        continue;
                    }
                    let active = units.get(&run.unit).is_some_and(|unit| unit.active);
                    if active {
                        if run.status == RunStatus::Launching {
                            run.status = RunStatus::Running;
                            changed = true;
                        }
                    } else {
                        run.status = if run.status == RunStatus::Launching {
                            RunStatus::Failed
                        } else {
                            RunStatus::Exited
                        };
                        if run.status == RunStatus::Failed && run.error.is_none() {
                            run.error = Some("The application did not start.".into());
                        }
                        run.ended_at = Some(now);
                        changed = true;
                    }
                }
                let app_running = state.running_apps();
                if app_running || state.viewers > 0 {
                    state.empty_since = None;
                } else if state.empty_since.is_none() {
                    state.empty_since = Some(Instant::now());
                }
                if let Some(since) = state.empty_since {
                    // An empty session is always reclaimed after the reload
                    // grace; `linux_idle_minutes` adds a configurable delay
                    // (0 = reclaim right after the grace). A session with
                    // running apps or an attached viewer is never reclaimed.
                    let deadline = EMPTY_GRACE
                        + Duration::from_secs(self.config.linux_idle_minutes.saturating_mul(60));
                    if since.elapsed() >= deadline {
                        idle = true;
                    }
                }
            }
            if changed {
                let runs = session.lock().apps.clone();
                for run in &runs {
                    self.store_run(run).await;
                }
                self.publish_session(session);
            }
            let user_id = session.user_id();
            let _ = sqlx::query(
                "UPDATE linux_sessions SET last_input = ?, updated_at = ? WHERE user_id = ?",
            )
            .bind(db::now_ms())
            .bind(db::now_ms())
            .bind(user_id)
            .execute(&self.db)
            .await;
            if idle {
                to_end.push((session.clone(), "idle", None));
            }
        }
        for (session, reason, error) in to_end {
            self.end_session(&session, reason, error).await;
        }
        // Forget ended sessions after a short grace (the browser already got
        // the final event).
        let ended: Vec<Arc<NativeSession>> = self
            .all_sessions()
            .into_iter()
            .filter(|session| {
                let state = session.lock();
                state.status == SessionStatus::Ended
                    && state
                        .ended_at
                        .is_some_and(|at| at.elapsed() >= ENDED_LINGER)
            })
            .collect();
        for session in ended {
            self.forget_session(&session);
        }
    }

    /// Ask the helper for every managed unit (name -> state).
    async fn units(&self) -> Result<HashMap<String, UnitInfo>, HelperError> {
        let data = self.helper.call("units_list", json!({})).await?;
        let mut units = HashMap::new();
        for unit in data
            .get("units")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let Some(name) = unit.get("unit").and_then(Value::as_str) else {
                continue;
            };
            units.insert(
                name.to_string(),
                UnitInfo {
                    active: unit.get("active").and_then(Value::as_bool).unwrap_or(false),
                    main_pid: unit
                        .get("mainPid")
                        .and_then(Value::as_u64)
                        .and_then(|pid| u32::try_from(pid).ok()),
                },
            );
        }
        Ok(units)
    }
}

/// One unit as reported by the helper's `units_list`.
#[derive(Debug, Clone, Copy)]
struct UnitInfo {
    active: bool,
    #[allow(dead_code)]
    main_pid: Option<u32>,
}

/// `linux_identities` row (cached identity).
#[derive(Debug, sqlx::FromRow)]
struct IdentityRow {
    username: String,
    uid: i64,
    gid: i64,
    home: String,
}

/// `linux_sessions` row used by recovery.
#[derive(Debug, sqlx::FromRow)]
struct SessionRow {
    user_id: i64,
    key: String,
    display: i64,
    port: i64,
    unit: String,
    started_at: i64,
}

/// `linux_runs` row used by recovery.
#[derive(Debug, sqlx::FromRow)]
struct RunRow {
    id: String,
    app_id: String,
    user_id: i64,
    unit: String,
    started_at: i64,
}

// ---------------------------------------------------------------------------
// Production helper client
// ---------------------------------------------------------------------------

/// Adapter from [`crate::native::client::NativeClient`] to the [`Helper`] seam.
struct ClientHelper(crate::native::client::NativeClient);

#[async_trait]
impl Helper for ClientHelper {
    async fn call(&self, op: &str, params: Value) -> Result<Value, HelperError> {
        self.0
            .call_json(op, params)
            .await
            .map_err(|error| HelperError {
                code: error.code,
                message: error.message,
            })
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested directly)
// ---------------------------------------------------------------------------

/// The systemd transient unit of one person's session (the helper derives the
/// same name; the server must predict it to persist the row before starting).
fn session_unit(user_id: i64) -> String {
    format!("rast-xpra-u{user_id}")
}

/// Parse `rast-xpra-u<userId>` back into the RAST user id.
fn session_unit_user(unit: &str) -> Option<i64> {
    unit.strip_prefix("rast-xpra-u")?.parse().ok()
}

/// Application unit name: `rast-app-u<uid>-<slug>-<sequence>`.
fn app_unit(uid: u32, slug: &str, sequence: u32) -> String {
    format!("rast-app-u{uid}-{slug}-{sequence}")
}

/// Lowercase `[a-z0-9-]` slug of an app id, at most [`UNIT_SLUG_MAX`] chars.
///
/// Characters outside the alphabet become a single `-`, so an app id with
/// underscores or spaces still yields a valid (and unique-enough) unit name.
fn app_slug(app_id: &str) -> String {
    let mut slug = String::new();
    for c in app_id.chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && slug.ends_with('-') {
            continue;
        }
        if slug.len() >= UNIT_SLUG_MAX {
            break;
        }
        slug.push(c);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "app".to_string()
    } else {
        slug
    }
}

/// App ids are file-derived slugs; anything else is refused before it becomes
/// a path or unit name.
fn validate_app_id(app_id: &str) -> AppResult<()> {
    let valid = !app_id.is_empty()
        && app_id.len() <= 40
        && app_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(AppError::bad_request("invalid app id"))
    }
}

fn runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(RUNTIME_ROOT).join(uid.to_string())
}

/// The Xpra server command line. Args are passed verbatim after the configured
/// `/usr/bin/xpra` binary; the helper starts them as the person's identity and
/// injects `HOME`/`XAUTHORITY`/`XDG_RUNTIME_DIR`.
///
/// * `--auth=none`: only the authenticated RAST proxy can reach the loopback
///   port, so Xpra itself must not ask the HTML5 client for a password.
/// * `--html=on`: the same TCP port serves the HTML5 client (HTTP) and its
///   WebSocket.
/// * everything not needed is disabled (audio, printing, webcam, mDNS, ...).
fn xpra_args(display: u32, port: u16, runtime: &Path) -> Vec<String> {
    vec![
        "start".to_string(),
        format!(":{display}"),
        "--daemon=no".to_string(),
        "--systemd-run=no".to_string(),
        format!("--bind-tcp=127.0.0.1:{port}"),
        "--html=on".to_string(),
        "--auth=none".to_string(),
        "--no-mdns".to_string(),
        "--notifications=no".to_string(),
        "--clipboard=yes".to_string(),
        format!("--socket-dir={}", runtime.join("sockets").display()),
        format!("--log-file={}", runtime.join("xpra.log").display()),
        "--xvfb=Xvfb".to_string(),
        "--dpi=96".to_string(),
        "--audio=no".to_string(),
        "--speaker=disabled".to_string(),
        "--microphone=disabled".to_string(),
        "--webcam=no".to_string(),
        "--printing=no".to_string(),
        "--pulseaudio=no".to_string(),
        "--dbus=no".to_string(),
    ]
}

/// Environment of the Xpra server process. The helper forces the identity
/// `HOME`/`XAUTHORITY`; explicitness here documents the contract and makes the
/// session reproducible when the helper fills only missing keys.
fn session_env(identity: &Identity, runtime: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HOME".into(), identity.home.display().to_string());
    env.insert(
        "XAUTHORITY".into(),
        identity.home.join(".Xauthority").display().to_string(),
    );
    env.insert("XDG_RUNTIME_DIR".into(), runtime.display().to_string());
    env.insert(
        "XDG_CACHE_HOME".into(),
        runtime.join("cache").display().to_string(),
    );
    env.insert("TMPDIR".into(), runtime.join("tmp").display().to_string());
    env
}

/// Is this display number already taken? Xpra's Xvfb uses an abstract socket
/// on this host, so the classic `/tmp/.X11-unix/X<n>` socket may not exist;
/// the `/tmp/.X<n>-lock` file still does.
fn display_in_use(display: u32) -> bool {
    PathBuf::from(format!("/tmp/.X11-unix/X{display}")).exists()
        || PathBuf::from(format!("/tmp/.X{display}-lock")).exists()
}

/// Ask the kernel for a free loopback port (bind with port 0, read it, drop).
async fn free_loopback_port() -> Option<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

/// Is the Xpra web server answering its HTML5 page on this loopback port?
async fn probe_http(port: u16) -> bool {
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut buffer = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() >= 8192 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
        let ok = head.starts_with("http/1.1 200") || head.starts_with("http/1.0 200");
        Some(ok && head.contains("content-type: text/html"))
    };
    tokio::time::timeout(Duration::from_secs(3), attempt)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Wait until the Xpra HTML5 page is served, up to `timeout`.
async fn wait_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if probe_http(port).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "The graphical session did not answer in time. Check the native helper log."
                    .to_string(),
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Memory admission for one more app. `available` is `/proc/meminfo`
/// `MemAvailable`; the app needs its own limit plus the operator's reserve.
/// The returned message shows both numbers.
fn admission_error(memory_mb: u32, available: u64, reserve_mb: u32) -> Option<String> {
    let required = (memory_mb as u64 + reserve_mb as u64) * 1024 * 1024;
    if available >= required {
        return None;
    }
    Some(format!(
        "The server has {} free and needs {} free to open another app. Close an app and try again.",
        gib(available),
        gib(required)
    ))
}

/// `/proc/meminfo` `MemAvailable` in bytes; unknown means "do not block".
fn available_memory_bytes() -> u64 {
    let text = std::fs::read_to_string("/proc/meminfo").ok();
    parse_mem_available(text.as_deref()).unwrap_or(u64::MAX)
}

fn parse_mem_available(meminfo: Option<&str>) -> Option<u64> {
    let line = meminfo?
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    hex::encode(buffer)
}

async fn app_name(db: &SqlitePool, app_id: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT name FROM linux_apps WHERE id = ?")
        .bind(app_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| app_id.to_string())
}

/// Constructors and observers used by the proxy's tests.
#[cfg(test)]
impl NativeSession {
    /// A ready session with no Xpra upstream (HTTP/WS tests supply the port
    /// themselves).
    pub(crate) fn for_test(key: String, user_id: i64) -> Arc<Self> {
        Arc::new(Self {
            key,
            user_id,
            identity: Identity {
                username: format!("rast-u{user_id}"),
                uid: 200_000u32.saturating_add(user_id as u32),
                gid: 200_000u32.saturating_add(user_id as u32),
                home: PathBuf::from(format!("/var/lib/rast/linux-users/{user_id}")),
            },
            state: Mutex::new(SessionState {
                status: SessionStatus::Ready,
                display: 100,
                port: None,
                unit: session_unit(user_id),
                viewers: 0,
                last_input: Instant::now(),
                empty_since: None,
                started_at: 1,
                reason: None,
                error: None,
                apps: Vec::new(),
                ended_at: None,
            }),
        })
    }

    pub(crate) fn viewers_for_test(&self) -> usize {
        self.lock().viewers
    }

    pub(crate) fn last_input_for_test(&self) -> Instant {
        self.lock().last_input
    }
}

// ---------------------------------------------------------------------------
// Tests: a fake helper daemon + fake Xpra upstream
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Fake privileged helper: records calls, keeps a unit table and — on
    /// `session_start` — binds the allocated loopback port and serves a
    /// minimal Xpra-like HTML page so the real readiness probe succeeds.
    #[derive(Default)]
    struct FakeHelper {
        calls: StdMutex<Vec<(String, Value)>>,
        units: StdMutex<HashMap<String, bool>>,
        session_starts: StdMutex<usize>,
        session_stops: StdMutex<usize>,
        app_starts: StdMutex<usize>,
        orphan_stops: StdMutex<Vec<String>>,
        fail_session_start: StdMutex<Option<String>>,
        broken_profile: StdMutex<bool>,
        stale_xauthority: StdMutex<bool>,
        upstreams: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
        port: StdMutex<Option<u16>>,
    }

    impl FakeHelper {
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }

        fn call_count(&self, op: &str) -> usize {
            self.calls().iter().filter(|(name, _)| name == op).count()
        }

        fn unit_active(&self, unit: &str) -> bool {
            self.units
                .lock()
                .unwrap()
                .get(unit)
                .copied()
                .unwrap_or(false)
        }

        fn set_unit(&self, unit: &str, active: bool) {
            self.units.lock().unwrap().insert(unit.to_string(), active);
        }

        /// Bind the fake Xpra HTTP server on `port`, retrying while the port
        /// is still coming free (the manager allocates it by binding port 0 and
        /// dropping the listener, so a brief race is normal on a busy host; a
        /// hard-coded port must never be used).
        async fn bind_upstream(&self, port: u16) -> Option<TcpListener> {
            for _ in 0..40 {
                match TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(listener) => return Some(listener),
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(_) => return None,
                }
            }
            None
        }

        /// Serve the minimal Xpra page on an already-bound listener.
        async fn serve_upstream(&self, listener: TcpListener) {
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        let mut chunk = [0u8; 1024];
                        let _ = socket.read(&mut chunk).await;
                        let body =
                            "<!doctype html><html><body><div id=\"screen\"></div></body></html>";
                        let response = format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            self.upstreams.lock().unwrap().push(handle);
        }

        /// Bind and serve the fake Xpra page on `port` (session_start path).
        async fn upstream(&self, port: u16) -> Result<(), HelperError> {
            let Some(listener) = self.bind_upstream(port).await else {
                return Err(HelperError {
                    code: "unavailable".into(),
                    message: format!("fake xpra port {port} stayed busy"),
                });
            };
            self.serve_upstream(listener).await;
            Ok(())
        }
    }

    #[async_trait]
    impl Helper for FakeHelper {
        async fn call(&self, op: &str, params: Value) -> Result<Value, HelperError> {
            self.calls
                .lock()
                .unwrap()
                .push((op.to_string(), params.clone()));
            match op {
                "identity_ensure" => {
                    let user_id = params["userId"].as_i64().unwrap_or(0);
                    let uid = 200_000 + user_id as u32;
                    let home = fake_identity_home(user_id);
                    let _ = std::fs::create_dir_all(&home);
                    Ok(json!({
                        "username": format!("rast-u{user_id}"),
                        "uid": uid,
                        "gid": uid,
                        "home": home.display().to_string(),
                        "created": false,
                    }))
                }
                "path_mkdir" | "path_acl" | "path_symlink" => Ok(json!({"ok": true})),
                "profile_prepare" => {
                    let app_id = params["appId"].as_str().unwrap_or_default();
                    let user_id = params["userId"].as_i64().unwrap_or(0);
                    // The real daemon creates the profile tree; the fake one
                    // has to create it too (the session manager verifies it).
                    if *self.broken_profile.lock().unwrap() {
                        // Simulate a helper whose profile setup silently did
                        // not create the directory.
                        return Ok(json!({
                            "home": std::env::temp_dir()
                                .join(format!("rast-native-missing-profile/{app_id}/{user_id}/home"))
                                .display()
                                .to_string(),
                        }));
                    }
                    let profile = std::env::temp_dir()
                        .join(format!("rast-native-test-profiles/{app_id}/{user_id}"));
                    let _ = std::fs::create_dir_all(profile.join("home"));
                    Ok(json!({
                        "home": profile.join("home").display().to_string(),
                        "profile": profile.display().to_string(),
                    }))
                }
                "session_start" => {
                    if let Some(message) = self.fail_session_start.lock().unwrap().clone() {
                        return Err(HelperError {
                            code: "command_failed".into(),
                            message,
                        });
                    }
                    *self.session_starts.lock().unwrap() += 1;
                    let port = u16::try_from(params["port"].as_u64().unwrap_or(0)).unwrap_or(0);
                    *self.port.lock().unwrap() = Some(port);
                    let unit = session_unit(params["userId"].as_i64().unwrap_or(0));
                    self.upstream(port).await?;
                    self.set_unit(&unit, true);
                    Ok(json!({"unit": unit, "active": true}))
                }
                "session_stop" => {
                    *self.session_stops.lock().unwrap() += 1;
                    let unit = session_unit(params["userId"].as_i64().unwrap_or(0));
                    self.set_unit(&unit, false);
                    Ok(json!({"stopped": true}))
                }
                "app_start" => {
                    if *self.stale_xauthority.lock().unwrap() {
                        return Err(HelperError {
                            code: "xauthority_unavailable".into(),
                            message: "the session's X auth file is not readable; home was \
                                      probably repaired after the session started"
                                .into(),
                        });
                    }
                    let unit = params["unit"].as_str().unwrap_or_default().to_string();
                    if self.unit_active(&unit) {
                        return Err(HelperError {
                            code: "unit_conflict".into(),
                            message: format!("unit {unit} is already active"),
                        });
                    }
                    *self.app_starts.lock().unwrap() += 1;
                    self.set_unit(&unit, true);
                    Ok(json!({"unit": unit, "active": true}))
                }
                "app_stop" => {
                    let unit = params["unit"].as_str().unwrap_or_default().to_string();
                    if self.unit_active(&unit) {
                        self.orphan_stops.lock().unwrap().push(unit.clone());
                    }
                    self.set_unit(&unit, false);
                    Ok(json!({"stopped": true}))
                }
                "unit_status" => {
                    let unit = params["unit"].as_str().unwrap_or_default().to_string();
                    let active = self.unit_active(&unit);
                    Ok(json!({
                        "unit": unit,
                        "active": active,
                        "state": if active { "active" } else { "inactive" },
                        "mainPid": null,
                    }))
                }
                "units_list" => {
                    let units = self
                        .units
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(unit, active)| {
                            json!({"unit": unit, "active": *active, "mainPid": null})
                        })
                        .collect::<Vec<Value>>();
                    Ok(json!({"units": units}))
                }
                "ping" => Ok(json!({"version": 1})),
                other => Err(HelperError {
                    code: "unsupported".into(),
                    message: format!("fake helper does not know {other}"),
                }),
            }
        }
    }

    /// A stable, per-user fake identity home (the warm-cache check stats it).
    fn fake_identity_home(user_id: i64) -> PathBuf {
        std::env::temp_dir().join(format!("rast-native-test-homes/{user_id}"))
    }

    async fn test_db() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::connect(&dir.path().join("native.db"), 2)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (1, 'alice', 'x', 'admin', 1, 1), (2, 'bob', 'x', 'user', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // `linux_runs.app_id` references `linux_apps(id)`.
        for app_id in ["one", "two", "code", "app"] {
            sqlx::query(
                "INSERT INTO linux_apps (id, name, format, file_name, build, status,
                     memory_mb, cpus, created_at, updated_at)
                 VALUES (?, ?, 'deb', 'app.deb', 1, 'ready', 1024, 2, 1, 1)",
            )
            .bind(app_id)
            .bind(format!("App {app_id}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        (dir, pool)
    }

    fn test_config() -> Arc<Config> {
        Arc::new(Config::from_parts(None, crate::config::FileConfig::default(), |_| None).unwrap())
    }

    fn test_user(id: i64) -> AuthUser {
        AuthUser {
            id,
            username: format!("user{id}"),
            display_name: "Test".into(),
            role: crate::auth::Role::User,
            session_id: "s".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    async fn manager() -> (
        tempfile::TempDir,
        Arc<FakeHelper>,
        Arc<NativeSessions>,
        Arc<Config>,
    ) {
        let (dir, db) = test_db().await;
        let config = test_config();
        let helper = Arc::new(FakeHelper::default());
        let sessions =
            NativeSessions::with_helper(config.clone(), db, helper.clone(), EventBus::new());
        (dir, helper, sessions, config)
    }

    fn launch_request(app_id: &str) -> LaunchRequest {
        LaunchRequest {
            app_id: app_id.into(),
            app_name: "Test App".into(),
            command: vec!["/usr/bin/xterm".into()],
            env: Vec::new(),
            cwd: None,
            memory_mb: 512,
            cpus: 2,
            tasks: 256,
            chromium: false,
        }
    }

    #[test]
    fn unit_and_slug_names_are_helper_compatible() {
        assert_eq!(session_unit(901), "rast-xpra-u901");
        assert_eq!(session_unit_user("rast-xpra-u901"), Some(901));
        assert_eq!(session_unit_user("rast-app-u1-x"), None);
        assert_eq!(
            app_slug("google-chrome-stable_amd64"),
            "google-chrome-stable"
        );
        assert_eq!(app_slug("OBS Studio"), "obs-studio");
        assert_eq!(app_slug("---"), "app");
        assert!(app_unit(200_901, &app_slug("code"), 7).len() < 64);
        assert_eq!(app_unit(200_901, "code", 0), "rast-app-u200901-code-0");
    }

    #[test]
    fn session_json_has_the_frontend_shape() {
        let session = NativeSession::for_test("a".repeat(32), 1);
        let json = session.json();
        assert_eq!(json["id"], "native");
        assert_eq!(json["status"], "ready");
        assert_eq!(
            json["url"],
            "/linux-apps/session/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"
        );
        assert_eq!(json["separateWindows"], true);
        assert!(json.get("key").is_none());
        assert_eq!(json["apps"], json!([]));
        assert_eq!(json["display"], 100);
    }

    #[test]
    fn memory_admission_shows_both_numbers() {
        let free = 2u64 * 1024 * 1024 * 1024;
        let message = admission_error(2048, free, 1024).expect("must refuse");
        assert!(message.contains("2.0 GB"), "{message}");
        assert!(message.contains("3.0 GB"), "{message}");
        assert!(admission_error(512, free, 1024).is_none());
        // Exactly at the boundary is enough.
        let exact = 1024u64 * 1024 * 1024;
        assert!(admission_error(512, exact, 512).is_none());
        assert!(admission_error(512, exact - 1, 512).is_some());
    }

    #[test]
    fn meminfo_parsing() {
        let text = "MemTotal:       32000000 kB\nMemAvailable:    8000000 kB\n";
        assert_eq!(parse_mem_available(Some(text)), Some(8_000_000u64 * 1024));
        assert_eq!(parse_mem_available(Some("nothing")), None);
        assert_eq!(parse_mem_available(None), None);
    }

    #[test]
    fn xpra_command_is_the_verified_one() {
        let args = xpra_args(150, 14611, &PathBuf::from("/run/rast-linux/200901"));
        assert_eq!(args[0], "start");
        assert_eq!(args[1], ":150");
        assert!(args.contains(&"--bind-tcp=127.0.0.1:14611".to_string()));
        assert!(args.contains(&"--html=on".to_string()));
        assert!(args.contains(&"--auth=none".to_string()));
        assert!(args.contains(&"--systemd-run=no".to_string()));
        assert!(args.contains(&"--socket-dir=/run/rast-linux/200901/sockets".to_string()));
        assert!(args.contains(&"--log-file=/run/rast-linux/200901/xpra.log".to_string()));
        assert!(args.contains(&"--xvfb=Xvfb".to_string()));
        assert!(!args.iter().any(|arg| arg == "--daemon=yes"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_missing_profile_is_reported_before_starting_the_app() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        *helper.broken_profile.lock().unwrap() = true;
        let error = sessions
            .launch(&test_user(1), launch_request("code"))
            .await
            .expect_err("must fail before app_start");
        let message = error.to_string();
        assert!(
            message.contains("profile directory") && message.contains("not"),
            "{message}"
        );
        assert_eq!(helper.call_count("app_start"), 0, "no unit was started");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn launcher_working_directories_are_constrained_to_managed_trees() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("linux-apps/profiles/app/1/home")).unwrap();
        let config = Arc::new(
            Config::from_parts(None, crate::config::FileConfig::default(), |name| {
                if name == "RAST_DATA_DIR" {
                    Some(data.display().to_string())
                } else {
                    None
                }
            })
            .unwrap(),
        );
        let (store, db) = test_db().await;
        let helper = Arc::new(FakeHelper::default());
        let sessions = NativeSessions::with_helper(config, db, helper, EventBus::new());
        let inside = data.join("linux-apps/profiles/app/1/home");
        assert!(sessions.managed_cwd(&inside));
        assert!(!sessions.managed_cwd(Path::new("/etc")));
        assert!(!sessions.managed_cwd(&data.join("linux-apps/does-not-exist")));
        assert!(!sessions.managed_cwd(Path::new("relative")));
        drop(store);
    }

    #[test]
    fn identity_home_health_is_checked() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        assert!(identity_home_is_healthy(&home, uid));
        assert!(!identity_home_is_healthy(&home, uid.saturating_add(1)));
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!identity_home_is_healthy(&home, uid));
        assert!(!identity_home_is_healthy(&dir.path().join("missing"), uid));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_inconsistent_helper_identity_is_refused() {
        let (_dir, _helper, sessions, _cfg) = manager().await;
        let good = Identity {
            username: "rast-u1".into(),
            uid: 200_001,
            gid: 200_001,
            home: PathBuf::from("/tmp/anything"),
        };
        sessions
            .validate_identity_mapping(1, &good)
            .expect("consistent mapping");
        for broken in [
            Identity {
                uid: 2,
                gid: 2,
                ..good.clone()
            },
            Identity {
                username: "root".into(),
                ..good.clone()
            },
            Identity {
                gid: 5,
                ..good.clone()
            },
        ] {
            let error = sessions
                .validate_identity_mapping(1, &broken)
                .expect_err("inconsistent identity");
            assert!(
                error.to_string().contains("identity"),
                "actionable message: {error}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_session_with_a_stale_identity_is_restarted() {
        let (_dir, _helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        // A "live" session created under a different (stale) identity, e.g.
        // before a home-permission repair changed the identity.
        let stale = Arc::new(NativeSession {
            key: "f".repeat(32),
            user_id: 1,
            identity: Identity {
                username: "rast-u1".into(),
                uid: 999_999,
                gid: 999_999,
                home: PathBuf::from("/nonexistent"),
            },
            state: Mutex::new(SessionState {
                status: SessionStatus::Ready,
                display: 900,
                port: Some(1),
                unit: session_unit(1),
                viewers: 0,
                last_input: Instant::now(),
                empty_since: None,
                started_at: 1,
                reason: None,
                error: None,
                apps: Vec::new(),
                ended_at: None,
            }),
        });
        sessions.insert_session(stale.clone());
        let fresh = sessions.ensure_session(&user).await.expect("fresh session");
        assert_ne!(fresh.key(), stale.key());
        assert_eq!(fresh.json()["status"], "ready");
        assert_eq!(stale.json()["status"], "ended");
        assert_eq!(stale.json()["reason"], "error");
        assert!(stale.json()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("identity was repaired"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_xauthority_restarts_the_session_for_the_next_open() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        let (_, first_session) = sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        let first_url = first_session["url"].as_str().unwrap().to_string();

        // The helper's new defense reports a stale X auth file for the next app.
        *helper.stale_xauthority.lock().unwrap() = true;
        let error = sessions
            .launch(&user, launch_request("app"))
            .await
            .expect_err("stale xauthority fails the launch");
        let text = error.to_string();
        assert!(
            text.contains("open the app again") && text.contains("auth"),
            "{text}"
        );
        // The stale session is stopped automatically so its X auth regenerates.
        assert!(
            helper.call_count("session_stop") >= 1,
            "the stale session is stopped"
        );
        let known = sessions.session_json(&user).await.unwrap();
        assert!(
            known.is_none() || known.unwrap()["status"] == "ended",
            "the stale session must not be reused"
        );

        // Reopening the app starts a brand new session and works.
        *helper.stale_xauthority.lock().unwrap() = false;
        let (run, session) = sessions
            .launch(&user, launch_request("app"))
            .await
            .expect("fresh launch");
        assert_eq!(run["status"], "running");
        assert_ne!(session["url"].as_str().unwrap(), first_url);
        assert_eq!(session["status"], "ready");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_known_identity_skips_the_helper_on_warm_starts() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let (first, repaired) = sessions.ensure_identity(1).await.expect("provision");
        assert!(!repaired);
        assert_eq!(helper.call_count("identity_ensure"), 1);
        // The row is persisted for the next process, and the home exists now:
        // a second lookup must not call the slow helper again.
        let (second, repaired) = sessions.ensure_identity(1).await.expect("cached");
        assert!(!repaired, "a healthy cached identity is not a repair");
        assert_eq!(second, first);
        assert_eq!(helper.call_count("identity_ensure"), 1);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM linux_identities")
            .fetch_one(&sessions.db)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        // A different account still needs its own identity.
        let (_, repaired) = sessions.ensure_identity(2).await.expect("second account");
        assert!(!repaired);
        assert_eq!(helper.call_count("identity_ensure"), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_launches_share_one_session() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let a = sessions.clone();
        let b = sessions.clone();
        let first =
            tokio::spawn(async move { a.launch(&test_user(1), launch_request("one")).await });
        let second =
            tokio::spawn(async move { b.launch(&test_user(1), launch_request("two")).await });
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        let (run_a, session_a) = first.expect("first launch");
        let (run_b, session_b) = second.expect("second launch");
        assert_eq!(
            *helper.session_starts.lock().unwrap(),
            1,
            "one Xpra session"
        );
        assert_eq!(helper.call_count("session_start"), 1);
        assert_eq!(helper.call_count("identity_ensure"), 1);
        assert_eq!(session_a["status"], "ready");
        assert_eq!(session_b["status"], "ready");
        assert_eq!(session_a["url"], session_b["url"]);
        assert_eq!(run_a["appId"], "one");
        assert_eq!(run_b["appId"], "two");
        assert_eq!(run_a["status"], "running");
        // Launches serialise per person: one session unit, two app units.
        assert_eq!(*helper.app_starts.lock().unwrap(), 2);
        // The session is registered under its key for the proxy.
        let key = session_a["url"]
            .as_str()
            .unwrap()
            .trim_start_matches(SESSION_PATH_PREFIX)
            .trim_end_matches('/');
        assert!(sessions.find_session(key).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn launching_the_same_app_returns_the_running_run() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        let (first, _) = sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        let (second, _) = sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        assert_eq!(first["runId"], second["runId"]);
        assert_eq!(*helper.app_starts.lock().unwrap(), 1);
        assert_eq!(sessions.running_counts().get("code").copied(), Some(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_and_idle_ending_stop_units() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        let (run, _) = sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        let run_id = run["runId"].as_str().unwrap().to_string();
        let stopped = sessions.close_app(&user, &run_id).await.unwrap();
        assert_eq!(stopped["status"], "exited");
        assert!(!helper.unit_active(run["unit"].as_str().unwrap_or("")));
        // Closing the session ends it and deletes its database row.
        let ended = sessions.close_session(&user).await.unwrap();
        assert_eq!(ended["status"], "ended");
        assert_eq!(ended["reason"], "closed");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM linux_sessions")
            .fetch_one(&sessions.db)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_session_start_reports_and_cleans_up() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        *helper.fail_session_start.lock().unwrap() = Some("xpra missing".into());
        let error = sessions
            .launch(&test_user(1), launch_request("code"))
            .await
            .expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("xpra missing"), "{message}");
        assert_eq!(
            *helper.session_stops.lock().unwrap(),
            1,
            "the failed unit is stopped"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM linux_sessions")
            .fetch_one(&sessions.db)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recovery_adopts_live_units_and_stops_orphans() {
        let (_dir, db) = test_db().await;
        let config = test_config();
        // A session row that is still live, one that is gone.
        sqlx::query(
            "INSERT INTO linux_sessions
                (user_id, key, display, port, unit, status, started_at, last_input, updated_at)
             VALUES (1, ?, 700, ?, 'rast-xpra-u1', 'ready', 1, 1, 1),
                    (2, ?, 701, 9999, 'rast-xpra-u2', 'ready', 1, 1, 1)",
        )
        .bind("a".repeat(32))
        .bind(0u16) // filled below with the fake upstream port
        .bind("b".repeat(32))
        .execute(&db)
        .await
        .unwrap();
        // A live app run for user 1 and an orphan run for user 2.
        sqlx::query(
            "INSERT INTO linux_runs (id, app_id, user_id, unit, status, error, started_at)
             VALUES ('run1', 'code', 1, 'rast-app-u200001-code-0', 'running', NULL, 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        let helper = Arc::new(FakeHelper::default());
        // Fake live units: user 1's session + app, user 2's dead session and
        // an orphan app unit that has no database row.
        helper.set_unit("rast-xpra-u1", true);
        helper.set_unit("rast-xpra-u2", false);
        helper.set_unit("rast-app-u200001-code-0", true);
        helper.set_unit("rast-app-u999999-orphan-0", true);
        // Serve the adopted session's HTML page on a port we own from the
        // start (never a hard-coded or re-bound port: tests run on busy hosts).
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("port");
        let port = listener.local_addr().expect("address").port();
        helper.serve_upstream(listener).await;
        sqlx::query("UPDATE linux_sessions SET port = ? WHERE user_id = 1")
            .bind(port as i64)
            .execute(&db)
            .await
            .unwrap();
        let sessions = NativeSessions::with_helper(config, db, helper.clone(), EventBus::new());
        sessions.recover().await;
        // User 1's session and app were adopted.
        let session = sessions.session_of_user(1).expect("adopted session");
        assert_eq!(session.json()["status"], "ready");
        assert_eq!(session.json()["display"], 700);
        let apps = session.json()["apps"].as_array().unwrap().clone();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0]["runId"], "run1");
        assert_eq!(apps[0]["status"], "running");
        // The dead session row was cleaned and its unit stopped.
        assert!(sessions.session_of_user(2).is_none());
        assert!(helper
            .calls()
            .iter()
            .any(|(op, params)| op == "session_stop" && params["userId"].as_i64() == Some(2)));
        // The orphan app unit was stopped; the adopted one was not.
        let stops = helper.orphan_stops.lock().unwrap().clone();
        assert!(stops.contains(&"rast-app-u999999-orphan-0".to_string()));
        assert!(!stops.contains(&"rast-app-u200001-code-0".to_string()));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM linux_sessions")
            .fetch_one(&sessions.db)
            .await
            .unwrap();
        assert_eq!(rows, 1, "only the adopted session row remains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_empty_session_is_reclaimed_after_the_grace() {
        // `linux_idle_minutes = 0`: reclaim an empty, viewer-less session as
        // soon as the 60 s reload grace has passed (never while a viewer is
        // attached).
        let (_dir, db) = test_db().await;
        let config = Arc::new(
            Config::from_parts(
                None,
                crate::config::FileConfig {
                    linux_idle_minutes: Some(0),
                    ..Default::default()
                },
                |_| None,
            )
            .unwrap(),
        );
        let helper = Arc::new(FakeHelper::default());
        let sessions = NativeSessions::with_helper(config, db, helper.clone(), EventBus::new());
        let user = test_user(1);
        sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        let session = sessions.session_of_user(1).expect("session");
        // The app exits by itself (its own close button).
        helper.set_unit("rast-app-u200001-code-0", false);
        sessions.sweep().await;
        // Still inside the reload grace: the session stays.
        sessions.sweep().await;
        assert_eq!(session.json()["status"], "ready");
        // Pretend the grace passed: the next sweep ends the session.
        session.lock().empty_since = Some(Instant::now() - EMPTY_GRACE - Duration::from_secs(1));
        sessions.sweep().await;
        assert_eq!(session.json()["status"], "ended");
        assert_eq!(session.json()["reason"], "idle");
        assert!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM linux_sessions")
                .fetch_one(&sessions.db)
                .await
                .unwrap()
                == 0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn close_for_user_stops_the_whole_session() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        sessions
            .close_for_user(1)
            .await
            .expect("closing a live session");
        let session = sessions.session_of_user(1).expect("session kept briefly");
        assert_eq!(session.json()["status"], "ended");
        assert_eq!(session.json()["reason"], "removed");
        assert!(!helper.unit_active(&session_unit(1)));
        // Closing again is not an error (idempotent).
        sessions.close_for_user(1).await.expect("idempotent close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_vanished_session_is_ended_by_the_sweeper() {
        let (_dir, helper, sessions, _cfg) = manager().await;
        let user = test_user(1);
        sessions
            .launch(&user, launch_request("code"))
            .await
            .unwrap();
        // Xpra dies behind RAST's back.
        helper.set_unit(&session_unit(1), false);
        sessions.sweep().await;
        let json = sessions
            .session_json(&user)
            .await
            .unwrap()
            .expect("still known briefly");
        assert_eq!(json["status"], "ended");
        assert_eq!(json["reason"], "error");
        assert!(json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stopped unexpectedly"));
    }
}
