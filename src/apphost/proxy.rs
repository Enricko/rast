//! Requests for an app: its API (forwarded to its server over
//! `app.sock`), its window files, and the list of app windows the desktop
//! adds to its own apps.
//!
//! A forwarded request has already passed RAST's sign-in, CSRF and app
//! access checks (the protected router's layers). It reaches the app without
//! cookies or `Authorization`, with `X-Rast-*` headers RAST sets itself:
//! `X-Rast-Token` (the app's secret), `X-Rast-User` (JSON: `id`, `username`,
//! `displayName`, `role`, `admin`) and `X-Rast-Client-Ip`.

use std::{path::Component, time::Duration};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::UnixStream;

use super::App;
use crate::{
    access,
    auth::{AuthUser, ClientIp},
    error::{AppError, AppResult},
    state::AppState,
};

/// Headers never passed between the browser and an app.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// Any `/api/<segment>/…` RAST itself does not answer: an app's API, or 404.
pub async fn forward(
    State(state): State<AppState>,
    user: AuthUser,
    ClientIp(ip): ClientIp,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    let Some(app) = state.apps.for_path(&path) else {
        return AppError::NotFound("not found").into_response();
    };
    to_app(&state, &app, &user, ip, request).await
}

/// Routes mounted beside RAST's own (AI Agent's `/api/ai` in its tests, see
/// [`crate::api::router_with`]) hand their requests to the app package
/// that serves the same paths while it runs; otherwise they answer them.
pub async fn hand_to_app(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let app = state.apps.for_path(request.uri().path());
    let user = request.extensions().get::<AuthUser>().cloned();
    let ip = request
        .extensions()
        .get::<ClientIp>()
        .map(|ip| ip.0)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    match (app, user) {
        (Some(app), Some(user)) => to_app(&state, &app, &user, ip, request).await,
        _ => next.run(request).await,
    }
}

async fn to_app(
    state: &AppState,
    app: &App,
    user: &AuthUser,
    ip: std::net::IpAddr,
    request: Request,
) -> Response {
    // The same rules as for its window (RAST's access layer has already
    // applied read-only grants to the apps it knows by path).
    match may_open(state, user, app).await {
        Ok(true) => {}
        Ok(false) => return AppError::Forbidden("access denied").into_response(),
        Err(error) => return error.into_response(),
    }
    match send(app, user, &ip.to_string(), request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn connect(app: &App) -> AppResult<UnixStream> {
    // A server that is starting gets a few seconds to open its socket.
    let mut last = None;
    for _ in 0..100 {
        match UnixStream::connect(app.app_socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
        let state = app.status().state;
        if state == "stopped" || state == "none" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    tracing::warn!(app = %app.app_id, error = ?last, "the app is not answering");
    Err(AppError::unavailable(format!(
        "{} is starting or stopped; try again in a moment",
        app.name
    )))
}

async fn send(app: &App, user: &AuthUser, ip: &str, request: Request) -> AppResult<Response> {
    let stream = connect(app).await?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| AppError::unavailable(format!("cannot reach {}: {e}", app.name)))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let (parts, body) = request.into_parts();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let mut builder = hyper::Request::builder()
        .method(parts.method)
        .uri(target)
        .header(header::HOST, "rast-app");
    let headers = builder.headers_mut().expect("fresh request");
    copy_headers(&parts.headers, headers, true);
    let identity = json!({
        "id": user.id,
        "username": user.username,
        "displayName": user.display_name,
        "role": user.role.as_str(),
        "admin": user.is_admin(),
    });
    insert(headers, "x-rast-token", &app.token);
    insert(headers, "x-rast-user", &identity.to_string());
    insert(headers, "x-rast-client-ip", ip);
    let outgoing = builder
        .body(body)
        .map_err(|e| AppError::internal(format!("cannot forward the request: {e}")))?;
    let incoming = sender
        .send_request(outgoing)
        .await
        .map_err(|e| AppError::unavailable(format!("{} did not answer: {e}", app.name)))?;
    let (parts, body) = incoming.into_parts();
    let mut response = Response::new(Body::new(body));
    *response.status_mut() = parts.status;
    copy_headers(&parts.headers, response.headers_mut(), false);
    Ok(response)
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// Copy end-to-end headers. Towards the app: no cookies, no credentials and
/// no `X-Rast-*` a browser made up. Towards the browser: no cookies either.
fn copy_headers(from: &HeaderMap, to: &mut HeaderMap, towards_app: bool) {
    for (name, value) in from {
        let lower = name.as_str();
        if HOP_BY_HOP.contains(&lower) {
            continue;
        }
        if towards_app
            && (lower == "cookie" || lower == "authorization" || lower.starts_with("x-rast-"))
        {
            continue;
        }
        if !towards_app && lower == "set-cookie" {
            continue;
        }
        to.append(name.clone(), value.clone());
    }
}

/// What a web page window's files may do: run their own scripts and
/// WebAssembly in an opaque origin (`sandbox` without `allow-same-origin`,
/// also when opened in a tab of their own), so they reach neither the
/// session nor RAST's API, and keep no storage; they may reach the web
/// (as the same app would on the person's own computer).
const WEB_CSP: &str = "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads allow-pointer-lock; \
default-src 'self'; \
script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval' blob:; \
style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:; font-src 'self' data:; \
media-src 'self' data: blob:; worker-src 'self' blob:; connect-src 'self' data: blob: https:; \
frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

/// The secret part of a web page window's address: the same for a version's
/// files on this server, unguessable without the server's secret.
fn web_key(state: &AppState, app: &App) -> String {
    let message = format!("{}\n{}\n{}", app.app_id, app.version, app.revision);
    state.secrets.hmac_hex("app web files", message.as_bytes())
}

/// The media type RAST serves an app's file as.
fn media_type(file: &std::path::Path) -> &'static str {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// What the desktop needs to add an app window.
pub fn window_view(state: &AppState, app: &App) -> Value {
    let base = format!(
        "/api/appfiles/{}/{}/{}/",
        app.app_id, app.version, app.revision
    );
    let window = &app.spec.window;
    json!({
        "appId": app.app_id,
        "packageId": app.package_id,
        "name": app.name,
        "description": app.description,
        "version": app.version,
        "icon": app.icon,
        "entry": format!("{base}{}", window.entry),
        "styles": window.styles.iter().map(|s| format!("{base}{s}")).collect::<Vec<_>>(),
        "sdk": window.sdk,
        "singleton": window.singleton,
        "adminOnly": window.admin_only,
        "defaultSize": window.default_size,
        "minSize": window.min_size,
        "opens": window.opens,
        "web": window.is_web().then(|| {
            let page = std::path::Path::new(&window.entry)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("index.html");
            format!("/api/appweb/{}/{page}", web_key(state, app))
        }),
        "apiPrefix": app.api_prefix(),
        "server": app.status(),
    })
}

/// GET /api/appfiles — the installed apps' windows (each account sees the
/// ones its access rules open; administrators all).
pub async fn windows(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let mut out = Vec::new();
    for app in state.apps.list() {
        if may_open(&state, &user, &app).await? {
            out.push(window_view(&state, &app));
        }
    }
    Ok(Json(json!({ "apps": out })))
}

/// GET /api/appweb/{key}/{*path} — a file of a web page window (the folder
/// of its page), under [`WEB_CSP`]. The key in the address comes only with
/// `/api/appfiles` (to accounts that may open the app); the page runs in an
/// opaque origin, so its own requests carry no session and the files are
/// served by the key alone. A version's files never change.
pub async fn web_files(
    State(state): State<AppState>,
    Path((key, path)): Path<(String, String)>,
) -> AppResult<Response> {
    let not_found = || AppError::NotFound("app file not found");
    let app = state
        .apps
        .list()
        .into_iter()
        .filter(|app| app.spec.window.is_web())
        .find(|app| {
            let message = format!("{}\n{}\n{}", app.app_id, app.version, app.revision);
            state
                .secrets
                .verify_hmac_hex("app web files", message.as_bytes(), &key)
        })
        .ok_or_else(not_found)?;
    let relative = std::path::Path::new(&path);
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(not_found());
    }
    let folder = std::path::Path::new(&app.spec.window.entry)
        .parent()
        .unwrap_or(std::path::Path::new(""));
    let root = tokio::fs::canonicalize(app.root.join(folder))
        .await
        .map_err(|_| not_found())?;
    let file = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| not_found())?;
    if !file.starts_with(&root) {
        return Err(not_found());
    }
    let opened = tokio::fs::File::open(&file)
        .await
        .map_err(|_| not_found())?;
    let meta = opened.metadata().await.map_err(|_| not_found())?;
    if !meta.is_file() {
        return Err(not_found());
    }
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
        opened,
        256 * 1024,
    ));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(media_type(&file)),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(WEB_CSP),
    );
    // The page's origin is opaque: its scripts and WebAssembly load with CORS.
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn may_open(state: &AppState, user: &AuthUser, app: &App) -> AppResult<bool> {
    if app.spec.window.admin_only && !user.is_admin() {
        return Ok(false);
    }
    if !access::is_app_id(&app.app_id) {
        // An app RAST has no access rules for: administrators only.
        return Ok(user.is_admin());
    }
    if !access::app_installed(&state.db, &app.app_id).await {
        return Ok(false);
    }
    if user.is_admin() {
        return Ok(true);
    }
    Ok(access::app_access(&state.db, user, &app.app_id)
        .await?
        .allowed)
}

/// GET /api/appfiles/{appId}/{version}/{revision}/{*path} — a file of the
/// app's window (the folder of its entry module), for accounts that may
/// open the app. A version's files never change: cached for a year.
pub async fn files(
    State(state): State<AppState>,
    user: AuthUser,
    Path((app_id, version, revision, path)): Path<(String, String, u32, String)>,
) -> AppResult<Response> {
    let app = state
        .apps
        .get(&app_id)
        .filter(|a| a.version == version && a.revision == revision)
        .ok_or(AppError::NotFound("app file not found"))?;
    if !may_open(&state, &user, &app).await? {
        return Err(AppError::NotFound("app file not found"));
    }
    let relative = std::path::Path::new(&path);
    let folder = std::path::Path::new(&app.spec.window.entry)
        .parent()
        .unwrap_or(std::path::Path::new(""));
    let safe = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        && relative.starts_with(folder);
    if !safe {
        return Err(AppError::NotFound("app file not found"));
    }
    let root = tokio::fs::canonicalize(&app.root)
        .await
        .map_err(|_| AppError::NotFound("app file not found"))?;
    let file = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| AppError::NotFound("app file not found"))?;
    if !file.starts_with(&root) {
        return Err(AppError::NotFound("app file not found"));
    }
    if app.spec.window.is_web() {
        // A web page's files are served only sandboxed (`/api/appweb`).
        return Err(AppError::NotFound("app file not found"));
    }
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|_| AppError::NotFound("app file not found"))?;
    let mime = media_type(&file);
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
