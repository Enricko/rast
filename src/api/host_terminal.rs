//! The Terminal app. Administrators get root shells on the server (see
//! [`crate::terminal`]); accounts granted the app by a group or their own
//! rule get shells in their own sandbox container (see [`crate::sandbox`]).
//! The app policy middleware refuses everyone else.
//!
//! * `GET /api/terminal` — is it available, and the caller's open shells.
//! * `POST /api/terminal/setup` — sandboxes: build the image the first time
//!   (in the background; `GET /api/terminal` reports its progress).
//! * `DELETE /api/terminal/sessions/{id}` — end one of the caller's shells.
//! * `GET /api/terminal/ws` — WebSocket: the first text message
//!   `{"type":"start","csrf":"…","cols":N,"rows":N,"session":"<id>"?}` opens
//!   a new shell or (with `session`) shows an open one again. Then binary
//!   messages carry terminal bytes both ways, text `{"type":"resize",…}`
//!   resizes. The server sends `{"type":"ready","session":{…},"created":bool}`,
//!   `{"type":"exit","code":N|null}`, `{"type":"detached"}` (opened in another
//!   window) or `{"type":"error","message":"…"}`. Closing the WebSocket only
//!   detaches: the shell keeps running.
//!
//! The same safety rules as container terminals apply (same-origin
//! WebSocket, CSRF token in the first message, the session re-checked every
//! minute).

use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, State,
    },
    http::HeaderMap,
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::terminal::{refuse, same_origin, still_admin};
use crate::{
    audit::{self, AuditLog},
    auth::{csrf, AuthUser},
    db,
    docker::exec::{clamp_size, read_frame, write_frame, Frame},
    error::{AppError, AppResult},
    sandbox,
    state::AppState,
    terminal::{valid_session_id, HostSession, TerminalClient, TerminalError, MAX_SESSIONS},
};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_CHECK: Duration = Duration::from_secs(60);
const MAX_MESSAGE: usize = 1024 * 1024;
/// What the server owner runs to turn the Terminal app on.
const ENABLE_COMMAND: &str = "sudo rast terminal enable";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/terminal", get(overview))
        .route("/api/terminal/setup", post(setup))
        .route("/api/terminal/sessions/{id}", delete(close_session))
        .route("/api/terminal/ws", get(socket))
}

/// Which shells an account gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Administrators: root shells from `rast-terminal.service`.
    Root,
    /// Everyone else granted the app: their own container.
    Sandbox,
}

impl Mode {
    fn of(user: &AuthUser) -> Self {
        if user.is_admin() {
            Self::Root
        } else {
            Self::Sandbox
        }
    }

    fn client(self, state: &AppState) -> TerminalClient {
        match self {
            Self::Root => TerminalClient::default(),
            Self::Sandbox => sandbox::client(&state.config),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Sandbox => "sandbox",
        }
    }
}

/// Non-administrators reach these routes only with the app granted (the app
/// policy middleware); this repeats the check for the record.
async fn require_access(state: &AppState, user: &AuthUser) -> AppResult<Mode> {
    let mode = Mode::of(user);
    if mode == Mode::Sandbox && !sandbox::allowed(&state.db, user.id).await {
        return Err(AppError::Forbidden(
            "Terminal is not available to your account",
        ));
    }
    Ok(mode)
}

/// Sandboxes are containers: refused while Docker is turned off.
fn require_docker(state: &AppState, mode: Mode) -> AppResult<()> {
    if mode == Mode::Sandbox && !state.config.docker_enabled {
        return Err(AppError::Unavailable(
            "Terminal needs Docker, which is turned off in this server's settings.".into(),
        ));
    }
    Ok(())
}

fn helper_error(mode: Mode, error: TerminalError) -> AppError {
    match error {
        TerminalError::Off if mode == Mode::Sandbox => AppError::Unavailable(
            "Terminal needs Docker on this server, and it is not running.".into(),
        ),
        TerminalError::Off => AppError::Unavailable(
            "The Terminal is off on this server. Turn it on with `sudo rast terminal enable`."
                .into(),
        ),
        TerminalError::Refused { code, message } if code == "preparing" => {
            AppError::Unavailable(message)
        }
        TerminalError::Refused { code, .. } if code == "not_found" => {
            AppError::NotFound("this terminal is no longer open")
        }
        TerminalError::Refused { message, .. } => AppError::conflict(message),
        TerminalError::Io(message) => {
            AppError::Unavailable(format!("The terminal helper is not answering: {message}"))
        }
    }
}

/// GET /api/terminal
async fn overview(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    match require_access(&state, &user).await? {
        Mode::Root => root_overview(&user).await,
        Mode::Sandbox => sandbox_overview(&state, &user).await,
    }
}

async fn root_overview(user: &AuthUser) -> AppResult<Json<Value>> {
    let client = TerminalClient::default();
    let status = match client.status().await {
        Ok(status) => status,
        Err(TerminalError::Off) => {
            return Ok(Json(json!({
                "available": false,
                "reason": "off",
                "enableCommand": ENABLE_COMMAND,
            })))
        }
        Err(error) => {
            return Ok(Json(json!({
                "available": false,
                "reason": "error",
                "message": error.to_string(),
                "enableCommand": ENABLE_COMMAND,
            })))
        }
    };
    let sessions = client
        .list(user.id)
        .await
        .map_err(|e| helper_error(Mode::Root, e))?;
    Ok(Json(json!({
        "available": true,
        "mode": "root",
        "shellUser": status["user"],
        "open": status["sessions"],
        "max": MAX_SESSIONS,
        "sessions": sessions,
    })))
}

async fn sandbox_overview(state: &AppState, user: &AuthUser) -> AppResult<Json<Value>> {
    if !state.config.docker_enabled {
        return Ok(Json(json!({
            "available": false,
            "mode": "sandbox",
            "reason": "docker",
            "message": "Terminal needs Docker, which is turned off in this server's settings.",
        })));
    }
    let client = sandbox::client(&state.config);
    let status = match client.status().await {
        Ok(status) => status,
        Err(TerminalError::Off) => {
            return Ok(Json(json!({
                "available": false,
                "mode": "sandbox",
                "reason": "docker",
                "message": "Terminal needs Docker on this server, and it is not running.",
            })))
        }
        Err(error) => {
            return Ok(Json(json!({
                "available": false,
                "mode": "sandbox",
                "reason": "error",
                "message": error.to_string(),
            })))
        }
    };
    let sessions = client
        .list(user.id)
        .await
        .map_err(|e| helper_error(Mode::Sandbox, e))?;
    Ok(Json(json!({
        "available": true,
        "mode": "sandbox",
        "shellUser": user.username.to_ascii_lowercase(),
        "image": status["image"],
        "imageError": status["imageError"],
        "limits": {
            "memoryMb": status["memoryMb"],
            "cpus": status["cpus"],
            "network": status["network"],
        },
        "max": status["perUser"],
        "sessions": sessions,
    })))
}

/// POST /api/terminal/setup — sandboxes: start building the image.
async fn setup(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let mode = require_access(&state, &user).await?;
    if mode == Mode::Root {
        return Ok(Json(json!({"image": "ready"})));
    }
    require_docker(&state, mode)?;
    let status = sandbox::client(&state.config)
        .prepare(user.id, &user.username.to_ascii_lowercase())
        .await
        .map_err(|e| helper_error(mode, e))?;
    Ok(Json(json!({
        "image": status["image"],
        "imageError": status["imageError"],
    })))
}

/// DELETE /api/terminal/sessions/{id}
async fn close_session(
    State(state): State<AppState>,
    user: AuthUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    if !valid_session_id(&id) {
        return Err(AppError::bad_request("invalid terminal id"));
    }
    let mode = require_access(&state, &user).await?;
    mode.client(&state)
        .kill(user.id, &id)
        .await
        .map_err(|e| helper_error(mode, e))?;
    let entry = AuditLog::new("terminal.close")
        .actor(&user)
        .object("terminal", &id)
        .detail(mode.as_str());
    audit::record_with_events(&state.db, &state.events, entry.success()).await;
    Ok(Json(json!({"ok": true})))
}

/// GET /api/terminal/ws — WebSocket upgrade.
async fn socket(
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
    let mode = require_access(&state, &user).await?;
    require_docker(&state, mode)?;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, state, user, mode)))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ClientMessage {
    Start {
        csrf: String,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        session: Option<String>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
}

async fn run(mut socket: WebSocket, state: AppState, user: AuthUser, mode: Mode) {
    let start = match tokio::time::timeout(START_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            serde_json::from_str::<ClientMessage>(text.as_str()).ok()
        }
        _ => None,
    };
    let Some(ClientMessage::Start {
        csrf,
        cols,
        rows,
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
    if !csrf::verify_token(&csrf, &state.secrets, &user.session_id) {
        refuse(
            &mut socket,
            "Your session could not be verified. Reload the page.",
        )
        .await;
        return;
    }
    let (cols, rows) = clamp_size(cols, rows);
    if mode == Mode::Sandbox && session.is_none() {
        // A new sandbox shell: the home folder must exist and be within quota.
        if let Err(error) = sandbox::ready_home(&state.db, &state.storage, &user).await {
            refuse(&mut socket, &error.public_message()).await;
            return;
        }
    }
    let client = mode.client(&state);
    let opened = match session.as_deref() {
        Some(id) if !valid_session_id(id) => {
            refuse(&mut socket, "This terminal is no longer open.").await;
            return;
        }
        Some(id) => client
            .attach(user.id, id, cols, rows)
            .await
            .map(|s| (s, false)),
        None => client
            .open(user.id, &user.username.to_ascii_lowercase(), cols, rows)
            .await
            .map(|s| (s, true)),
    };
    let (shell, created) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            if session.is_none() {
                let entry = AuditLog::new("terminal.open").actor(&user);
                let detail = format!("{}: {error}", mode.as_str());
                audit::record_with_events(&state.db, &state.events, entry.failure().detail(detail))
                    .await;
            }
            let message = match helper_error(mode, error) {
                AppError::NotFound(_) => "This terminal is no longer open.".to_string(),
                other => other.public_message(),
            };
            refuse(&mut socket, &message).await;
            return;
        }
    };
    if created {
        let entry = AuditLog::new("terminal.open")
            .actor(&user)
            .object("terminal", &shell.info.id)
            .detail(mode.as_str());
        audit::record_with_events(&state.db, &state.events, entry.success()).await;
    }
    let ready = json!({"type": "ready", "session": shell.info, "created": created}).to_string();
    if socket.send(Message::Text(ready.into())).await.is_err() {
        return;
    }
    bridge(socket, shell, &state, &user, mode).await;
}

enum Output {
    Data(Vec<u8>),
    Exit(Option<i32>),
    Detached,
}

async fn bridge(
    mut socket: WebSocket,
    shell: HostSession,
    state: &AppState,
    user: &AuthUser,
    mode: Mode,
) {
    let HostSession {
        mut reader,
        mut writer,
        ..
    } = shell;
    let (output_tx, mut output) = mpsc::channel::<Output>(16);
    let pump = tokio::spawn(async move {
        loop {
            let next = match read_frame(&mut reader).await {
                Ok(Some(Frame::Data(bytes))) => Output::Data(bytes),
                Ok(Some(Frame::Exit(code))) => Output::Exit(Some(code)),
                Ok(Some(Frame::Detached)) => Output::Detached,
                _ => Output::Exit(None),
            };
            let last = !matches!(next, Output::Data(_));
            if output_tx.send(next).await.is_err() || last {
                break;
            }
        }
    });
    let mut session_check = tokio::time::interval(SESSION_CHECK);
    session_check.tick().await;

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if write_frame(&mut writer, &Frame::Data(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMessage::Resize { cols, rows }) =
                        serde_json::from_str::<ClientMessage>(text.as_str())
                    {
                        let (cols, rows) = clamp_size(Some(cols), Some(rows));
                        if write_frame(&mut writer, &Frame::Resize { cols, rows }).await.is_err() {
                            break;
                        }
                    }
                }
                // The page closed: detach (the shell keeps running).
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            next = output.recv() => match next {
                Some(Output::Data(bytes)) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Some(Output::Detached) => {
                    let _ = socket
                        .send(Message::Text(json!({"type": "detached"}).to_string().into()))
                        .await;
                    break;
                }
                next @ (Some(Output::Exit(_)) | None) => {
                    let code = match next {
                        Some(Output::Exit(code)) => code,
                        _ => None,
                    };
                    let exit = json!({"type": "exit", "code": code}).to_string();
                    let _ = socket.send(Message::Text(exit.into())).await;
                    break;
                }
            },
            _ = session_check.tick() => {
                let allowed = match mode {
                    Mode::Root => still_admin(state, user).await,
                    Mode::Sandbox => {
                        session_valid(state, user).await
                            && sandbox::allowed(&state.db, user.id).await
                    }
                };
                if !allowed {
                    refuse(&mut socket, "Your session ended, so the terminal was closed.").await;
                    if mode == Mode::Sandbox && !sandbox::allowed(&state.db, user.id).await {
                        sandbox::end_shells_of(&state.config, user.id).await;
                    }
                    break;
                }
            }
        }
    }
    // Ending the connection without a close frame detaches.
    drop(writer);
    pump.abort();
    let _ = socket.send(Message::Close(None)).await;
}

/// The browser session behind a shell is still signed in.
async fn session_valid(state: &AppState, user: &AuthUser) -> bool {
    let found: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(&user.session_id)
    .bind(user.id)
    .bind(db::now_ms())
    .fetch_optional(&state.db)
    .await;
    matches!(found, Ok(Some(_)))
}
