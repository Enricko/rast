//! Native terminal over WebSocket, plus the Terminal runtime environment API.
//!
//! The terminal runs on the server as the mapped `rast-u<userId>` identity
//! (one private HOME per person), behind an authenticated, same-origin
//! WebSocket. Every start/attach/close re-checks the RAST session and the
//! "terminal" application permission; the identity is always derived from the
//! authenticated session and never supplied by the browser.
//!
//! Browser → server: a `start` text message with the CSRF token, then binary
//! keyboard bytes, `resize` and `close` messages.
//! Server → browser: `ready`, binary output chunks, `exit`/`error`.
//!
//! REST:
//! * `GET  /api/terminal/env`      runtime environment + panel data
//! * `PATCH /api/terminal/env`     save per-user default runtime versions
//! * `GET  /api/terminal/sessions` this account's live sessions
//! * `POST /api/terminal/sessions/{id}/close` close one session

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State,
    },
    http::{header, HeaderMap},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    access,
    auth::{csrf, middleware::origin_of, AuthUser},
    config::Config,
    db,
    error::{AppError, AppResult},
    state::AppState,
    storage::{safe_path::LogicalPath, Access},
    terminal::{env, manager::TerminalSpec},
};

/// The browser must send its start message within this time.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the signed-in session and permission are re-checked.
const SESSION_CHECK: Duration = Duration::from_secs(30);
/// Largest WebSocket message accepted from the browser (a big paste).
const MAX_MESSAGE: usize = 1024 * 1024;
/// Upper bound for per-terminal runtime override entries.
const MAX_OVERRIDES: usize = 16;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/terminal/env", get(env).patch(set_env))
        .route("/api/terminal/sessions", get(list_sessions))
        .route("/api/terminal/sessions/{id}/close", post(close_session))
        .route("/api/terminal/ws", get(terminal_ws))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderRef {
    /// RAST logical path (`/`, `/sub/dir`, `/shared/<share>/...`); never a
    /// POSIX path.
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMessage {
    Start {
        csrf: String,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        folder: Option<FolderRef>,
        #[serde(default)]
        runtimes: Option<BTreeMap<String, String>>,
        /// Reconnect to an existing session instead of starting a new shell.
        #[serde(default)]
        session: Option<String>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Close,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvUpdate {
    defaults: BTreeMap<String, String>,
}

async fn env(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    require_app(&state, &user).await?;
    Ok(Json(state.terminal.env_dto(&user).await?))
}

async fn set_env(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<EnvUpdate>,
) -> AppResult<Json<Value>> {
    require_app(&state, &user).await?;
    for (runtime, version) in &body.defaults {
        if runtime.len() > 64 || version.len() > 64 {
            return Err(AppError::bad_request("invalid runtime default"));
        }
        if !env::version_installed(&state.config, runtime, version) {
            return Err(AppError::bad_request(format!(
                "{runtime} {version} is not installed"
            )));
        }
    }
    Ok(Json(
        state.terminal.set_defaults(&user, &body.defaults).await?,
    ))
}

async fn list_sessions(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    require_app(&state, &user).await?;
    Ok(Json(state.terminal.list(&user).await))
}

async fn close_session(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    require_app(&state, &user).await?;
    state.terminal.close(&user, &id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn require_app(state: &AppState, user: &AuthUser) -> AppResult<()> {
    if !state.config.terminal.enabled {
        return Err(AppError::unavailable(
            "The terminal is disabled on this server (terminal_enabled = false).",
        ));
    }
    if !crate::terminal::manager::supported() {
        return Err(AppError::unavailable(
            "The terminal is only available on Linux servers.",
        ));
    }
    let access = access::app_access(&state.db, user, "terminal").await?;
    if !access.allowed {
        return Err(AppError::Forbidden(
            "Your account may not use the Terminal.",
        ));
    }
    Ok(())
}

/// GET /api/terminal/ws — authenticated WebSocket upgrade.
async fn terminal_ws(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> AppResult<Response> {
    if !same_origin(&headers, &state.config) {
        return Err(AppError::Forbidden(
            "terminals open only from the RAST page itself",
        ));
    }
    require_app(&state, &user).await?;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run_terminal(socket, state, user)))
}

/// Whether the WebSocket request comes from the RAST page itself.
fn same_origin(headers: &HeaderMap, cfg: &Config) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(origin_of)
    else {
        return false;
    };
    if let Some(public) = cfg.public_url.as_deref().and_then(origin_of) {
        if public == origin {
            return true;
        }
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host.trim()))
}

async fn run_terminal(mut socket: WebSocket, state: AppState, user: AuthUser) {
    let start = match tokio::time::timeout(START_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            serde_json::from_str::<ClientMessage>(text.as_str()).ok()
        }
        _ => None,
    };
    let Some(ClientMessage::Start {
        csrf: token,
        cols,
        rows,
        folder,
        runtimes,
        session,
    }) = start
    else {
        refuse(
            &mut socket,
            "The terminal did not start correctly. Try again.",
        )
        .await;
        return;
    };
    if !csrf::verify_token(&token, &state.secrets, &user.session_id) {
        refuse(
            &mut socket,
            "Your session could not be verified. Reload the page.",
        )
        .await;
        return;
    }
    let (cols, rows) = crate::terminal::protocol::clamp_size(cols, rows);

    // Reconnect first: never start a duplicate shell for a tab that only
    // lost its WebSocket.
    let viewer = if let Some(session_id) = session.filter(|id| !id.is_empty()) {
        match state.terminal.attach(&user, &session_id).await {
            Ok(viewer) => Some((viewer, true)),
            Err(error) => {
                // An expired session is not fatal: tell the client so it can
                // start a fresh shell (the UI opens a new tab).
                let message = format!("{}\u{0}", error.public_message());
                let _ = socket
                    .send(Message::Text(
                        json!({"type": "error", "message": error.public_message(), "reconnect": false})
                            .to_string()
                            .into(),
                    ))
                    .await;
                let _ = message;
                return;
            }
        }
    } else {
        None
    };

    let viewer = match viewer {
        Some((viewer, reconnected)) => (viewer, reconnected),
        None => {
            let overrides = runtimes.unwrap_or_default();
            if overrides.len() > MAX_OVERRIDES {
                refuse(&mut socket, "Too many runtime overrides.").await;
                return;
            }
            for (runtime, version) in &overrides {
                if !env::version_installed(&state.config, runtime, version) {
                    refuse(
                        &mut socket,
                        &format!("{runtime} {version} is not installed on this server."),
                    )
                    .await;
                    return;
                }
            }
            let (cwd, write_access) = match resolve_folder(&state, &user, folder.as_ref()).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    refuse(&mut socket, &error.public_message()).await;
                    return;
                }
            };
            let spec = TerminalSpec {
                cwd,
                write_access,
                overrides,
                cols,
                rows,
            };
            match state.terminal.start(&user, spec).await {
                Ok(viewer) => (viewer, false),
                Err(error) => {
                    refuse(&mut socket, &error.public_message()).await;
                    return;
                }
            }
        }
    };
    let (mut viewer, reconnected) = viewer;
    let session_id = viewer.session_id.clone();

    let ready = json!({
        "type": "ready",
        "sessionId": session_id,
        "shell": viewer.shell,
        "cwd": viewer.cwd,
        "root": viewer.root,
        "reconnected": reconnected,
        "runtimes": viewer.runtimes,
    });
    if socket
        .send(Message::Text(ready.to_string().into()))
        .await
        .is_err()
    {
        state.terminal.detach(&session_id).await;
        return;
    }
    if !viewer.replay.is_empty()
        && socket
            .send(Message::Binary(viewer.replay.into()))
            .await
            .is_err()
    {
        state.terminal.detach(&session_id).await;
        return;
    }

    let mut session_check = tokio::time::interval(SESSION_CHECK);
    session_check.tick().await;
    loop {
        tokio::select! {
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if !state.terminal.input(&user, &session_id, bytes.to_vec()).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(ClientMessage::Resize { cols, rows }) => {
                                let (cols, rows) = crate::terminal::protocol::clamp_size(Some(cols), Some(rows));
                                state.terminal.resize(&user, &session_id, cols, rows).await;
                            }
                            Ok(ClientMessage::Close) => {
                                let _ = state.terminal.close(&user, &session_id).await;
                                break;
                            }
                            Ok(ClientMessage::Start { .. }) | Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                }
            }
            event = viewer.events.recv() => {
                match event {
                    Some(crate::terminal::manager::ViewerEvent::Output(bytes)) => {
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(crate::terminal::manager::ViewerEvent::Exit(code)) => {
                        let _ = socket.send(Message::Text(
                            json!({"type": "exit", "code": code}).to_string().into(),
                        )).await;
                        break;
                    }
                    Some(crate::terminal::manager::ViewerEvent::Error(message)) => {
                        let _ = socket.send(Message::Text(
                            json!({"type": "error", "message": message}).to_string().into(),
                        )).await;
                        break;
                    }
                    Some(crate::terminal::manager::ViewerEvent::Closed) | None => break,
                }
            }
            _ = session_check.tick() => {
                if !session_still_valid(&state, &user).await {
                    let _ = socket.send(Message::Text(
                        json!({"type": "error", "message": "Your session ended, so the terminal was closed."})
                            .to_string()
                            .into(),
                    )).await;
                    let _ = state.terminal.close(&user, &session_id).await;
                    break;
                }
            }
        }
    }
    state.terminal.detach(&session_id).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Resolve a RAST folder reference to a physical directory.
///
/// Uses the storage layer, so the share ACL/group rules decide whether the
/// folder is visible; the returned path is canonical (no symlinks) and inside
/// a storage root. The browser never supplies a POSIX path.
async fn resolve_folder(
    state: &AppState,
    user: &AuthUser,
    folder: Option<&FolderRef>,
) -> AppResult<(Option<PathBuf>, bool)> {
    let Some(folder) = folder else {
        return Ok((None, false));
    };
    let logical = LogicalPath::parse(&folder.path)?;
    let resolved = state
        .storage
        .resolve(&state.db, user, &logical.as_string(), Access::Read)
        .await?;
    let metadata =
        std::fs::metadata(&resolved.physical).map_err(|_| AppError::NotFound("path not found"))?;
    if !metadata.is_dir() {
        return Err(AppError::bad_request(
            "Open in Terminal works on folders, not files.",
        ));
    }
    Ok((Some(resolved.physical), resolved.perm.allows_write()))
}

/// The session that opened the terminal is still valid and the account may
/// still use the Terminal (app permission can be revoked live).
async fn session_still_valid(state: &AppState, user: &AuthUser) -> bool {
    let found: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT 1 FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.id = ? AND s.user_id = ? AND s.revoked_at IS NULL AND s.expires_at > ? AND u.active = 1",
    )
    .bind(&user.session_id)
    .bind(user.id)
    .bind(db::now_ms())
    .fetch_optional(&state.db)
    .await;
    if !matches!(found, Ok(Some(_))) {
        return false;
    }
    match access::app_access(&state.db, user, "terminal").await {
        Ok(access) => access.allowed,
        Err(_) => false,
    }
}

async fn refuse(socket: &mut WebSocket, message: &str) {
    let error = json!({"type": "error", "message": message}).to_string();
    let _ = socket.send(Message::Text(error.into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Unused query struct kept for future list filters (serde-friendly route).
#[derive(Debug, Deserialize, Default)]
struct SessionsQuery {
    #[serde(default)]
    _all: Option<bool>,
}

#[allow(dead_code)]
fn _keep_query(_query: Option<Query<SessionsQuery>>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn config(public_url: Option<&str>) -> Config {
        Config::from_parts(
            None,
            crate::config::FileConfig {
                public_url: public_url.map(str::to_string),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap()
    }

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        headers
    }

    #[test]
    fn terminals_open_only_from_the_rast_page() {
        let direct = config(None);
        assert!(same_origin(
            &headers(Some("http://192.168.1.20:8720"), "192.168.1.20:8720"),
            &direct
        ));
        assert!(!same_origin(
            &headers(Some("https://evil.example"), "192.168.1.20:8720"),
            &direct
        ));
        assert!(!same_origin(&headers(None, "192.168.1.20:8720"), &direct));
        let tunnel = config(Some("https://rast.example.com"));
        assert!(same_origin(
            &headers(Some("https://rast.example.com"), "localhost:8720"),
            &tunnel
        ));
        assert!(!same_origin(
            &headers(Some("https://evil.example.com"), "localhost:8720"),
            &tunnel
        ));
    }

    #[test]
    fn client_messages_are_strict() {
        let start: ClientMessage = serde_json::from_str(
            r#"{"type":"start","csrf":"abc","cols":120,"rows":40,"folder":{"path":"/shared/team"},"runtimes":{"rust":"1.100.0"},"session":"abc"}"#,
        )
        .unwrap();
        assert!(matches!(
            start,
            ClientMessage::Start {
                cols: Some(120),
                rows: Some(40),
                session: Some(_),
                ..
            }
        ));
        let resize: ClientMessage =
            serde_json::from_str(r#"{"type":"resize","cols":80,"rows":24}"#).unwrap();
        assert!(matches!(
            resize,
            ClientMessage::Resize { cols: 80, rows: 24 }
        ));
        for bad in [
            r#"{"type":"exec","cmd":"rm -rf /"}"#,
            r#"{"type":"resize","cols":-1,"rows":24}"#,
            r#"{"type":"resize","cols":70000,"rows":24}"#,
            r#"{"csrf":"abc"}"#,
        ] {
            assert!(serde_json::from_str::<ClientMessage>(bad).is_err(), "{bad}");
        }
    }
}
