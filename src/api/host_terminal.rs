//! The Terminal app: administrators' shells on the server (see
//! [`crate::terminal`]). Administrators only.
//!
//! * `GET /api/terminal` — is it on, and the caller's open shells.
//! * `DELETE /api/terminal/sessions/{id}` — end one of them.
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
    routing::{delete, get},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::terminal::{refuse, same_origin, still_admin};
use crate::{
    audit::{self, AuditLog},
    auth::{csrf, AdminUser, AuthUser},
    docker::exec::{clamp_size, read_frame, write_frame, Frame},
    error::{AppError, AppResult},
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
        .route("/api/terminal/sessions/{id}", delete(close_session))
        .route("/api/terminal/ws", get(socket))
}

fn helper_error(error: TerminalError) -> AppError {
    match error {
        TerminalError::Off => AppError::Unavailable(
            "The Terminal is off on this server. Turn it on with `sudo rast terminal enable`."
                .into(),
        ),
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
async fn overview(State(_state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
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
    let sessions = client.list(admin.0.id).await.map_err(helper_error)?;
    Ok(Json(json!({
        "available": true,
        "shellUser": status["user"],
        "open": status["sessions"],
        "max": MAX_SESSIONS,
        "sessions": sessions,
    })))
}

/// DELETE /api/terminal/sessions/{id}
async fn close_session(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    if !valid_session_id(&id) {
        return Err(AppError::bad_request("invalid terminal id"));
    }
    let user = admin.0;
    TerminalClient::default()
        .kill(user.id, &id)
        .await
        .map_err(helper_error)?;
    let entry = AuditLog::new("terminal.close")
        .actor(&user)
        .object("terminal", &id);
    audit::record_with_events(&state.db, &state.events, entry.success()).await;
    Ok(Json(json!({"ok": true})))
}

/// GET /api/terminal/ws — WebSocket upgrade.
async fn socket(
    State(state): State<AppState>,
    admin: AdminUser,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> AppResult<Response> {
    if !same_origin(&headers, &state.config) {
        return Err(AppError::Forbidden(
            "terminals open only from the RAST page itself",
        ));
    }
    let user = admin.0;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, state, user)))
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

async fn run(mut socket: WebSocket, state: AppState, user: AuthUser) {
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
    let client = TerminalClient::default();
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
            .open(user.id, &user.username, cols, rows)
            .await
            .map(|s| (s, true)),
    };
    let (shell, created) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            if session.is_none() {
                let entry = AuditLog::new("terminal.open").actor(&user);
                let detail = error.to_string();
                audit::record_with_events(&state.db, &state.events, entry.failure().detail(detail))
                    .await;
            }
            let message = match helper_error(error) {
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
            .object("terminal", &shell.info.id);
        audit::record_with_events(&state.db, &state.events, entry.success()).await;
    }
    let ready = json!({"type": "ready", "session": shell.info, "created": created}).to_string();
    if socket.send(Message::Text(ready.into())).await.is_err() {
        return;
    }
    bridge(socket, shell, &state, &user).await;
}

enum Output {
    Data(Vec<u8>),
    Exit(Option<i32>),
    Detached,
}

async fn bridge(mut socket: WebSocket, shell: HostSession, state: &AppState, user: &AuthUser) {
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
                if !still_admin(state, user).await {
                    refuse(&mut socket, "Your session ended, so the terminal was closed.").await;
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
