//! Container terminals over WebSocket (admin only).
//!
//! `GET /api/docker/containers/{id}/terminal` upgrades to a WebSocket that is
//! bridged to a `container-exec` session of the privileged worker.
//!
//! Browser → server:
//! * first, within 10 seconds, a text message
//!   `{"type":"start","csrf":"…","cols":N,"rows":N}`;
//! * binary messages: keyboard bytes;
//! * text `{"type":"resize","cols":N,"rows":N}`.
//!
//! Server → browser:
//! * text `{"type":"ready","container":"name"}` once the shell runs;
//! * binary messages: terminal output;
//! * text `{"type":"exit","code":N|null}` when the shell ends;
//! * text `{"type":"error","message":"…"}` when the terminal cannot open or
//!   is closed by the server.
//!
//! Safety: a browser sends cookies with WebSocket requests from any site and
//! cannot add headers, so the request must come from the RAST page itself
//! (same `Origin` as the host or the public URL) and the first message must
//! carry the session's CSRF token. The terminal policy is checked here and
//! again by the worker. The session is re-checked every minute: signing out,
//! a revoked session or a lost admin role closes the terminal.

use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, State,
    },
    http::{header, HeaderMap},
    response::Response,
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::docker::{all_containers, change_error, find_container, inspect_container, str_field};
use crate::{
    audit::{self, AuditLog},
    auth::{csrf, middleware::origin_of, AdminUser, AuthUser},
    config::Config,
    db,
    docker::{
        exec::{clamp_size, read_frame, terminal_policy, write_frame, Frame},
        protocol::validate_container_id,
        TerminalSession,
    },
    error::{AppError, AppResult},
    state::AppState,
};

/// The browser must send its start message within this time.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the signed-in session is re-checked.
const SESSION_CHECK: Duration = Duration::from_secs(60);
/// Largest WebSocket message accepted from the browser (a big paste).
const MAX_MESSAGE: usize = 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/docker/containers/{id}/terminal",
        get(container_terminal),
    )
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
    },
    Resize {
        cols: u16,
        rows: u16,
    },
}

/// GET /api/docker/containers/{id}/terminal — WebSocket upgrade.
pub async fn container_terminal(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> AppResult<Response> {
    if !same_origin(&headers, &state.config) {
        return Err(AppError::Forbidden(
            "terminals open only from the RAST page itself",
        ));
    }
    validate_container_id(&id).map_err(AppError::bad_request)?;
    state.docker.ensure_available().await?;
    let user = admin.0;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run_terminal(socket, state, user, id)))
}

/// Whether the WebSocket request comes from the RAST page: browsers always
/// send `Origin` on WebSocket requests, and it must name this server (the
/// configured public URL or the host the request was sent to).
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

async fn run_terminal(mut socket: WebSocket, state: AppState, user: AuthUser, id: String) {
    let start = match tokio::time::timeout(START_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            serde_json::from_str::<ClientMessage>(text.as_str()).ok()
        }
        _ => None,
    };
    let Some(ClientMessage::Start { csrf, cols, rows }) = start else {
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

    let (container_id, name, opened) = match open(&state, &id, cols, rows).await {
        Ok(parts) => parts,
        Err((container_id, name, error)) => {
            let message = error.public_message();
            if let Some(container_id) = container_id {
                let entry = AuditLog::new("docker.container.exec")
                    .actor(&user)
                    .object("container", &container_id)
                    .path(&name);
                audit::record_with_events(
                    &state.db,
                    &state.events,
                    entry.failure().detail(message.clone()),
                )
                .await;
            }
            refuse(&mut socket, &message).await;
            return;
        }
    };
    let entry = AuditLog::new("docker.container.exec")
        .actor(&user)
        .object("container", &container_id)
        .path(&name);
    audit::record_with_events(&state.db, &state.events, entry.success()).await;

    let ready = json!({"type": "ready", "container": name}).to_string();
    if socket.send(Message::Text(ready.into())).await.is_err() {
        return;
    }
    bridge(socket, opened, &state, &user).await;
}

type OpenError = (Option<String>, String, AppError);

/// Resolve the container, apply the terminal policy and start the shell.
async fn open(
    state: &AppState,
    id: &str,
    cols: u16,
    rows: u16,
) -> Result<(String, String, TerminalSession), OpenError> {
    let containers = all_containers(state)
        .await
        .map_err(|error| (None, String::new(), error))?;
    let container = find_container(&containers, id)
        .map_err(|error| (None, String::new(), error))?
        .clone();
    let full_id = str_field(&container, "id").to_string();
    let name = str_field(&container, "name").to_string();
    let fail = |error: AppError| (Some(full_id.clone()), name.clone(), error);
    let inspect = inspect_container(state, &full_id).await.map_err(fail)?;
    if str_field(&inspect, "state") != "running" {
        return Err(fail(AppError::conflict(
            "The container is not running. Start it to open a terminal.",
        )));
    }
    let (allowed, reason) = terminal_policy(&inspect);
    if !allowed {
        return Err(fail(AppError::conflict(reason)));
    }
    let session = state
        .docker
        .open_terminal(&full_id, cols, rows)
        .await
        .map_err(|error| fail(change_error(error)))?;
    Ok((full_id, name, session))
}

/// Shell output on its way to the browser.
enum Output {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

async fn bridge(
    mut socket: WebSocket,
    session: TerminalSession,
    state: &AppState,
    user: &AuthUser,
) {
    let TerminalSession {
        mut reader,
        mut writer,
        ..
    } = session;
    // Frames are read in their own task (reading a frame is not cancel safe).
    let (output_tx, mut output) = mpsc::channel::<Output>(16);
    let pump = tokio::spawn(async move {
        loop {
            let next = match read_frame(&mut reader).await {
                Ok(Some(Frame::Data(bytes))) => Output::Data(bytes),
                Ok(Some(Frame::Exit(code))) => Output::Exit(Some(code)),
                _ => Output::Exit(None),
            };
            let last = matches!(next, Output::Exit(_));
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
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            next = output.recv() => match next {
                Some(Output::Data(bytes)) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
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
    let _ = write_frame(&mut writer, &Frame::Close).await;
    drop(writer);
    pump.abort();
    let _ = socket.send(Message::Close(None)).await;
}

/// The session that opened the terminal is still valid and still belongs to
/// an active administrator. Read only: it does not extend the session.
async fn still_admin(state: &AppState, user: &AuthUser) -> bool {
    let found: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT 1 FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.id = ? AND s.user_id = ? AND s.revoked_at IS NULL AND s.expires_at > ?
           AND u.active = 1 AND u.role = 'admin'",
    )
    .bind(&user.session_id)
    .bind(user.id)
    .bind(db::now_ms())
    .fetch_optional(&state.db)
    .await;
    matches!(found, Ok(Some(_)))
}

/// Tell the browser why the terminal is not available, then close.
async fn refuse(socket: &mut WebSocket, message: &str) {
    let error = json!({"type": "error", "message": message}).to_string();
    let _ = socket.send(Message::Text(error.into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

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
        assert!(same_origin(
            &headers(Some("http://NAS.local:8720"), "nas.local:8720"),
            &direct
        ));
        // Another site, another port, no origin at all, or an opaque origin.
        assert!(!same_origin(
            &headers(Some("https://evil.example"), "192.168.1.20:8720"),
            &direct
        ));
        assert!(!same_origin(
            &headers(Some("http://192.168.1.20:9999"), "192.168.1.20:8720"),
            &direct
        ));
        assert!(!same_origin(&headers(None, "192.168.1.20:8720"), &direct));
        assert!(!same_origin(
            &headers(Some("null"), "192.168.1.20:8720"),
            &direct
        ));

        // Behind a tunnel the host may be rewritten; the public URL counts.
        let tunnel = config(Some("https://rast.example.com"));
        assert!(same_origin(
            &headers(Some("https://rast.example.com"), "localhost:8720"),
            &tunnel
        ));
        assert!(!same_origin(
            &headers(Some("https://evil.example.com"), "localhost:8720"),
            &tunnel
        ));

        let mut cross = headers(Some("http://192.168.1.20:8720"), "192.168.1.20:8720");
        cross.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&cross, &direct));
    }

    #[test]
    fn client_messages_are_strict() {
        let start: ClientMessage =
            serde_json::from_str(r#"{"type":"start","csrf":"abc","cols":120,"rows":40}"#).unwrap();
        assert!(matches!(
            start,
            ClientMessage::Start {
                cols: Some(120),
                rows: Some(40),
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
