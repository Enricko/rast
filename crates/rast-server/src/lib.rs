//! The RAST server: accounts with roles, password sign-in and sessions.
//!
//! * Passwords: Argon2id PHC strings; the password itself is never stored.
//! * Sessions: a random 256-bit token in an `HttpOnly; Secure; SameSite=Strict`
//!   cookie. The database keeps only its SHA-256, so a leaked database cannot be
//!   replayed as a session. Each session records the client address and browser
//!   it signed in from; people end their own, administrators end others'.
//! * Lockout: wrong passwords are throttled per account and address; ten in
//!   half an hour lock the account for 15 minutes, until an administrator
//!   unlocks it. A password an administrator set must be replaced at the next
//!   sign-in before anything else is allowed.
//! * CSRF: every request that changes state must carry `X-RAST: 1`. Browsers
//!   only send custom headers cross-origin after a CORS preflight, which this
//!   server never approves; together with `SameSite=Strict` that blocks
//!   cross-site requests.
//! * First run: with no accounts, the server writes a one-time setup token to
//!   `<data>/setup-token`. Creating the first account (a Super Admin) needs it,
//!   so whoever reaches a fresh public site first cannot claim it.

pub mod access;
mod api;
mod auth;
mod db;
mod groups;
mod limit;
mod roles;
mod sessions;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use sqlx::SqlitePool;

pub use roles::Role;

/// How long a sign-in lasts.
pub const SESSION_SECS: i64 = 7 * 24 * 3600;

#[derive(Clone)]
pub struct AppState {
    pub(crate) pool: SqlitePool,
    pub(crate) limiter: Arc<limit::Limiter>,
    pub(crate) setup: Arc<Setup>,
}

/// The one-time token that authorizes creating the first account.
pub(crate) struct Setup {
    token: Mutex<Option<String>>,
    path: Option<PathBuf>,
}

impl Setup {
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        use sha2::{Digest, Sha256};
        use subtle::ConstantTimeEq;
        let guard = self
            .token
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.as_deref().is_some_and(|token| {
            let a = Sha256::digest(token.as_bytes());
            let b = Sha256::digest(candidate.trim().as_bytes());
            a.ct_eq(&b).into()
        })
    }

    /// The token is spent: forget it and remove its file.
    pub(crate) fn finish(&self) {
        *self
            .token
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl AppState {
    /// A state over `pool`. `setup_token` is the first-run token, if any.
    pub fn new(pool: SqlitePool, setup_token: Option<String>, token_path: Option<PathBuf>) -> Self {
        Self {
            pool,
            limiter: Arc::new(limit::Limiter::default()),
            setup: Arc::new(Setup {
                token: Mutex::new(setup_token),
                path: token_path,
            }),
        }
    }

    /// A state on a fresh in-memory database, for tests.
    pub async fn in_memory(setup_token: &str) -> Result<Self, String> {
        let pool = db::open_memory().await.map_err(|error| error.to_string())?;
        Ok(Self::new(pool, Some(setup_token.to_owned()), None))
    }
}

pub fn router(state: AppState) -> Router {
    api::router(state)
}

/// Open the database under `data`, prepare first-run setup and serve until
/// SIGINT or SIGTERM.
pub async fn serve(listen: SocketAddr, data: PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(&data)
        .map_err(|error| format!("cannot create {}: {error}", data.display()))?;
    // Password hashes and session digests live here: owner only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot protect {}: {error}", data.display()))?;
    }
    let pool = db::open(&data.join("rast.db"))
        .await
        .map_err(|error| format!("cannot open the database: {error}"))?;
    let token_path = data.join("setup-token");
    let token = if db::user_count(&pool).await.map_err(|e| e.to_string())? == 0 {
        Some(write_setup_token(&token_path)?)
    } else {
        let _ = std::fs::remove_file(&token_path);
        None
    };
    let state = AppState::new(pool.clone(), token, Some(token_path));

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(error) = db::prune_sessions(&pool).await {
                eprintln!("rast-server: pruning sessions failed: {error}");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot listen on {listen}: {error}"))?;
    eprintln!("rast-server: serving the API on http://{listen}/api");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|error| error.to_string())
}

/// Keep an existing token across restarts so a token already handed out stays valid.
fn write_setup_token(path: &Path) -> Result<String, String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim();
        if existing.len() >= 20 {
            eprintln!(
                "rast-server: no accounts yet. The setup token is in {}",
                path.display()
            );
            return Ok(existing.to_owned());
        }
    }
    let token = auth::random_token(18);
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, format!("{token}\n"))
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot protect {}: {error}", temp.display()))?;
    }
    std::fs::rename(&temp, path)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    eprintln!(
        "rast-server: no accounts yet. Create the Super Admin with the setup token in {}",
        path.display()
    );
    Ok(token)
}

async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => {
                    let _ = interrupt.await;
                    return;
                }
            };
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}
