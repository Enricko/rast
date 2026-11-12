//! Same-origin HTTP + WebSocket proxy for a person's Xpra seamless session.
//!
//! The browser embeds the Xpra HTML5 client from `/linux-apps/session/<key>/`
//! in a **same-origin, non-sandboxed** iframe (native apps are adopted as
//! individual RAST windows by the frontend shim). The Xpra client derives its
//! WebSocket target from the page URL, so it connects back to the very same
//! prefix; this module strips the prefix and relays to the loopback Xpra web
//! server (127.0.0.1, never exposed).
//!
//! Security model:
//! * the 32-hex session key is **not** a credential: every request must carry a
//!   valid RAST session cookie whose user owns the session (checked on every
//!   request and on every WebSocket upgrade);
//! * WebSocket upgrades additionally require a same-origin `Origin`;
//! * `Cookie`/`Authorization` are never forwarded to Xpra;
//! * HTML pages get the browser shim ([`inject_shim`]) and a CSP that allows
//!   exactly what the Xpra client needs;
//! * session keys are never logged, and never appear in error pages.
//!
//! The page is redirected once to `?sharing=yes`: Xpra's HTML5 client otherwise
//! steals the session on a second connection, which would make two RAST
//! windows (or two tabs of the same person) fight over one graphical session.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{
        ws::{Message as ClientMessage, WebSocket, WebSocketUpgrade},
        FromRequestParts, Request, State,
    },
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as UpstreamMessage};

use super::native_session::{NativeSession, SESSION_PATH_PREFIX};
use crate::state::AppState;

/// The frontend-owned shim, served from this proxy and injected into every
/// proxied HTML page.
///
/// The real file is `src/linux_apps/xpra_shim.js` (owned by the frontend
/// teammate). Until it lands this inline placeholder keeps the page loadable
/// and the proxy testable; switching it over is a one-line change to
/// `include_str!("xpra_shim.js")`.
/// The frontend-owned shim, embedded at build time and served from this proxy
/// for every proxied HTML page (`src/linux_apps/xpra_shim.js` is owned by the
/// frontend teammate; the proxy only serves and injects it).
pub const SHIM_JS: &str = include_str!("xpra_shim.js");

/// What the proxied Xpra page may load and connect to.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob: data:; \
connect-src 'self' ws: wss:; worker-src 'self' blob:; frame-ancestors 'self'; \
base-uri 'none'; form-action 'none'";

/// Request headers passed on to Xpra (never `Cookie`, `Authorization` or
/// `Accept-Encoding`: pages are injected as decoded HTML).
const REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "cache-control",
    "content-type",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "range",
    "user-agent",
];

/// Response headers passed back from Xpra.
const RESPONSE_HEADERS: &[&str] = &[
    "accept-ranges",
    "cache-control",
    "content-encoding",
    "content-language",
    "content-length",
    "content-range",
    "content-type",
    "etag",
    "expires",
    "last-modified",
    "vary",
];

const MAX_REQUEST_BODY: usize = 1024 * 1024;
const MAX_RESPONSE_HEAD: usize = 64 * 1024;
const MAX_HTML: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Frame cap for the relayed WebSocket (Xpra sends whole window updates).
const MAX_WS_MESSAGE: usize = 64 * 1024 * 1024;

/// Handler for `/linux-apps/session/<key>/…`: the Xpra HTML5 page, its assets
/// and its WebSocket, served through RAST with per-user authorization.
pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let Some(rest) = path.strip_prefix(SESSION_PATH_PREFIX) else {
        return not_found_page();
    };
    let (key, tail) = match rest.split_once('/') {
        Some((key, tail)) => (key, tail),
        None => (rest, ""),
    };
    let has_slash = rest.contains('/');
    if !valid_key(key) {
        return not_found_page();
    }
    if !has_slash {
        // `/linux-apps/session/<key>` -> `/linux-apps/session/<key>/`, so the
        // page's relative asset URLs resolve under the session prefix.
        let query = request
            .uri()
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        let location = format!("{SESSION_PATH_PREFIX}{key}/{query}");
        let response = (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response();
        return response_with_policy(response);
    }
    // Authorization happens on every single request, before the key is even
    // resolved: an unauthenticated request gets 401 and never learns whether a
    // key exists; an authenticated request for an unknown key gets 404; a
    // valid account that does not own the session gets 403.
    let user_id =
        match authenticate(&state.db, state.config.session_ttl_hours, request.headers()).await {
            Ok(user_id) => user_id,
            Err(response) => return response,
        };
    let Some(session) = state.linux_apps.find_native_session(key) else {
        return not_found_page();
    };
    if session.user_id() != user_id {
        return forbidden_page();
    }
    // The shim is RAST code, not Xpra content.
    if tail == "rast-shim.js" {
        if !matches!(request.method(), &Method::GET | &Method::HEAD) {
            return plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return shim_response(request.method() == Method::HEAD);
    }
    let Some(port) = session.upstream_port() else {
        return closed_page();
    };
    if is_websocket(request.headers()) {
        return websocket(request, session, port).await;
    }
    // One redirect per page load tells Xpra's HTML5 client to share the
    // session instead of stealing it (two RAST windows must coexist).
    if tail.is_empty() && matches!(request.method(), &Method::GET | &Method::HEAD) {
        if let Some(location) = sharing_redirect(request.uri().path(), request.uri().query()) {
            let mut response = (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return response;
        }
    }
    forward(request, session, port).await
}

/// 32 lowercase hex characters.
pub(crate) fn valid_key(key: &str) -> bool {
    key.len() == 32
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Does `key` appear in `url` as a session key? (Only for tests and logs; the
/// proxy itself never needs this.)
#[allow(dead_code)]
pub(crate) fn key_from_url(url: &str) -> Option<String> {
    let rest = url.split_once(SESSION_PATH_PREFIX)?.1;
    let key = rest.split('/').next()?;
    valid_key(key).then(|| key.to_string())
}

/// Append `sharing=yes` to the session page's query (once).
pub(crate) fn sharing_redirect(path: &str, query: Option<&str>) -> Option<String> {
    let has_sharing = query
        .map(|query| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some("sharing"))
        })
        .unwrap_or(false);
    if has_sharing {
        return None;
    }
    let target = match query {
        Some(query) if !query.is_empty() => format!("{path}?{query}&sharing=yes"),
        _ => format!("{path}?sharing=yes"),
    };
    Some(target)
}

/// Map a RAST session path to the Xpra upstream path (strip the key prefix,
/// preserve the query).
pub(crate) fn upstream_target(path: &str, query: Option<&str>) -> String {
    let rest = path
        .strip_prefix(SESSION_PATH_PREFIX)
        .unwrap_or(path)
        .split_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or("");
    let mut target = format!("/{rest}");
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    target
}

/// Put the shim `<script>` immediately before the page's last `</body>` (or at
/// the end when the page has no body tag).
pub fn inject_shim(html: &str, key: &str) -> String {
    let script = format!("<script src=\"{SESSION_PATH_PREFIX}{key}/rast-shim.js\"></script>");
    let lower = html.to_ascii_lowercase();
    match lower.rfind("</body>") {
        Some(at) => format!("{}{script}{}", &html[..at], &html[at..]),
        None => format!("{html}{script}"),
    }
}

fn shim_response(head_only: bool) -> Response {
    let mut response = if head_only {
        StatusCode::OK.into_response()
    } else {
        SHIM_JS.into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// Validate the RAST session cookie on an Xpra request and return the
/// account id: valid session and active account.
///
/// `Response` is large; the error variant is only used on the request path
/// where a response has to be built anyway.
#[allow(clippy::result_large_err)]
async fn authenticate(
    db: &sqlx::SqlitePool,
    session_ttl_hours: i64,
    headers: &HeaderMap,
) -> Result<i64, Response> {
    let Some(token) = crate::auth::cookie_value(headers, crate::auth::session::SESSION_COOKIE)
    else {
        return Err(unauthorized_page());
    };
    let record = match crate::auth::session::validate(db, &token, session_ttl_hours).await {
        Ok(Some(record)) => record,
        Ok(None) => return Err(unauthorized_page()),
        Err(error) => {
            tracing::warn!(error = %error, "cannot validate a session cookie for the Xpra proxy");
            return Err(plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ));
        }
    };
    let row: Option<(bool,)> = sqlx::query_as("SELECT active FROM users WHERE id = ?")
        .bind(record.user_id)
        .fetch_optional(db)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "cannot load the user for the Xpra proxy");
            plain_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        })?;
    match row {
        Some((true,)) => Ok(record.user_id),
        _ => Err(unauthorized_page()),
    }
}

/// Is a WebSocket upgrade from this origin allowed? The browser must be on
/// the same origin as RAST (or the configured public URL); `Sec-Fetch-Site`
/// is an extra cross-site guard.
fn upgrade_origin_allowed(headers: &HeaderMap, public_url: Option<&str>) -> bool {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Browsers always send Origin on WebSocket upgrades; a browser-driven
        // connection without one is not a same-origin RAST page.
        return false;
    };
    if origin == "null" || origin.is_empty() {
        return false;
    }
    if let Some(public_url) = public_url {
        if let (Some(expected), Some(actual)) = (
            crate::auth::middleware::origin_of(public_url),
            crate::auth::middleware::origin_of(origin),
        ) {
            if expected.eq_ignore_ascii_case(&actual) {
                return true;
            }
        }
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    same_origin(origin, host)
}

/// Does `origin` (`scheme://host[:port]`) name the same origin as the request's
/// `Host` authority?
pub(crate) fn same_origin(origin: &str, host: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return false,
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some((origin_host, origin_port)) = split_authority(&authority, default_port) else {
        return false;
    };
    let Some((request_host, request_port)) = split_authority(host, default_port) else {
        return false;
    };
    origin_host == request_host && origin_port == request_port
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    // IPv6 literals: [::1]:8720
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(default_port);
        return Some((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => Some((
            host.to_ascii_lowercase(),
            port.parse().unwrap_or(default_port),
        )),
        _ => Some((authority.to_ascii_lowercase(), default_port)),
    }
}

// ---------------------------------------------------------------------------
// HTML page / asset forwarding
// ---------------------------------------------------------------------------

async fn forward(request: Request, session: Arc<NativeSession>, port: u16) -> Response {
    let (parts, body) = request.into_parts();
    if !matches!(
        parts.method,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
    ) {
        return plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(body) => body,
        Err(_) => return plain_error(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
    };
    let target = upstream_target(parts.uri.path(), parts.uri.query());
    let mut head = format!(
        "{} {target} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n",
        parts.method
    );
    for name in REQUEST_HEADERS {
        for value in parts.headers.get_all(*name) {
            if let Ok(value) = value.to_str() {
                if !value.contains(['\r', '\n']) {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
            }
        }
    }
    if !body.is_empty() || matches!(parts.method, Method::POST | Method::PUT | Method::PATCH) {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    let mut stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => {
            return plain_error(
                StatusCode::BAD_GATEWAY,
                "the graphical session is not answering",
            )
        }
    };
    if stream.write_all(head.as_bytes()).await.is_err() || stream.write_all(&body).await.is_err() {
        return plain_error(
            StatusCode::BAD_GATEWAY,
            "the graphical session is not answering",
        );
    }
    let (status, headers, leftover) =
        match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
            Ok(Some(parsed)) => parsed,
            _ => {
                return plain_error(
                    StatusCode::BAD_GATEWAY,
                    "the graphical session sent an unreadable answer",
                )
            }
        };
    let mut response_headers = HeaderMap::new();
    let mut content_type = String::new();
    for (name, value) in &headers {
        let lower = name.to_ascii_lowercase();
        if !RESPONSE_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if lower == "content-type" {
            content_type = value.to_ascii_lowercase();
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(lower.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            response_headers.append(name, value);
        }
    }
    let html = content_type.starts_with("text/html") && parts.method != Method::HEAD;
    let body = if html {
        let mut page = leftover;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    if page.len() + n > MAX_HTML {
                        return plain_error(
                            StatusCode::BAD_GATEWAY,
                            "the session page is too large",
                        );
                    }
                    page.extend_from_slice(&chunk[..n]);
                }
                Err(_) => {
                    return plain_error(StatusCode::BAD_GATEWAY, "the session page was cut off")
                }
            }
        }
        let page = inject_shim(&String::from_utf8_lossy(&page), session.key());
        response_headers.remove(header::CONTENT_LENGTH);
        response_headers.remove(header::CONTENT_ENCODING);
        response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Body::from(page)
    } else {
        let first = futures::stream::once(async move {
            Ok::<_, std::io::Error>(axum::body::Bytes::from(leftover))
        });
        Body::from_stream(first.chain(tokio_util::io::ReaderStream::new(stream)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response_with_policy(response)
}

/// Read and parse an HTTP response head; returns the bytes after it.
async fn read_head(stream: &mut TcpStream) -> Option<(StatusCode, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&buffer) {
            Ok(httparse::Status::Complete(length)) => {
                let status = StatusCode::from_u16(response.code?).ok()?;
                let headers = response
                    .headers
                    .iter()
                    .map(|header| {
                        (
                            header.name.to_string(),
                            String::from_utf8_lossy(header.value).into_owned(),
                        )
                    })
                    .collect();
                return Some((status, headers, buffer[length..].to_vec()));
            }
            Ok(httparse::Status::Partial) if buffer.len() < MAX_RESPONSE_HEAD => continue,
            _ => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket relay
// ---------------------------------------------------------------------------

async fn websocket(request: Request, session: Arc<NativeSession>, port: u16) -> Response {
    if !upgrade_origin_allowed(request.headers(), None) {
        return forbidden_page();
    }
    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let target = upstream_target(parts.uri.path(), parts.uri.query());
    let Ok(mut upstream_request) = format!("ws://127.0.0.1:{port}{target}").into_client_request()
    else {
        return plain_error(StatusCode::BAD_GATEWAY, "invalid session address");
    };
    // Sub-protocols (if any) are negotiated end to end; credentials are not.
    if let Some(protocols) = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        upstream_request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, protocols.clone());
    }
    let connected = async {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        tokio_tungstenite::client_async(upstream_request, stream)
            .await
            .ok()
    };
    let Ok(Some((upstream, answer))) =
        tokio::time::timeout(Duration::from_secs(10), connected).await
    else {
        return plain_error(
            StatusCode::BAD_GATEWAY,
            "the graphical session refused the connection",
        );
    };
    let upgrade = match answer
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
    {
        Some(protocol) => upgrade.protocols([protocol.to_string()]),
        None => upgrade,
    };
    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| relay(socket, upstream, session))
}

/// Relay WebSocket frames both ways. Every frame from the browser counts as
/// activity (input, clipboard, resize, ping); the viewer count drives the
/// session's idle/empty rules.
async fn relay(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<TcpStream>,
    session: Arc<NativeSession>,
) {
    session.viewer_joined();
    let (mut client_tx, mut client_rx) = client.split();
    let (mut app_tx, mut app_rx) = upstream.split();
    let watcher = session.clone();
    let to_app = async move {
        while let Some(Ok(message)) = client_rx.next().await {
            watcher.input();
            let message = match message {
                ClientMessage::Text(text) => UpstreamMessage::text(text.as_str().to_owned()),
                ClientMessage::Binary(bytes) => UpstreamMessage::Binary(bytes),
                ClientMessage::Ping(bytes) => UpstreamMessage::Ping(bytes),
                ClientMessage::Pong(bytes) => UpstreamMessage::Pong(bytes),
                ClientMessage::Close(_) => break,
            };
            if app_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = app_tx.close().await;
    };
    let to_client = async move {
        while let Some(Ok(message)) = app_rx.next().await {
            let message = match message {
                UpstreamMessage::Text(text) => ClientMessage::Text(text.as_str().to_owned().into()),
                UpstreamMessage::Binary(bytes) => ClientMessage::Binary(bytes),
                UpstreamMessage::Ping(bytes) => ClientMessage::Ping(bytes),
                UpstreamMessage::Pong(bytes) => ClientMessage::Pong(bytes),
                UpstreamMessage::Close(_) => break,
                UpstreamMessage::Frame(_) => continue,
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };
    tokio::select! {
        _ = to_app => {}
        _ = to_client => {}
    }
    session.viewer_left();
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn response_with_policy(mut response: Response) -> Response {
    apply_policy(response.headers_mut());
    response
}

fn apply_policy(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // The page is embedded by RAST itself (same origin), not by anyone else.
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
}

fn closed_page() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "This graphical session is not running. Open the app again from RAST.",
    )
}

fn not_found_page() -> Response {
    page(StatusCode::NOT_FOUND, "This page does not exist.")
}

fn forbidden_page() -> Response {
    page(
        StatusCode::FORBIDDEN,
        "This graphical session belongs to another account.",
    )
}

fn unauthorized_page() -> Response {
    page(
        StatusCode::UNAUTHORIZED,
        "Sign in to RAST to open this graphical session.",
    )
}

fn plain_error(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn page(status: StatusCode, message: &'static str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>RAST session</title>\
         <body style=\"font:14px system-ui;color:#555;display:grid;place-items:center;height:90vh\">\
         {message}</body>"
    );
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use sqlx::SqlitePool;

    fn session(key: &str, user_id: i64) -> Arc<NativeSession> {
        NativeSession::for_test(key.to_string(), user_id)
    }

    async fn test_db() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::connect(&dir.path().join("proxy.db"), 2)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at, active)
             VALUES (1, 'alice', 'x', 'admin', 1, 1, 1), (2, 'bob', 'x', 'user', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    fn cookie_header(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("rast_session={token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn keys_are_lowercase_hex_of_fixed_length() {
        assert!(valid_key(&"0123456789abcdef".repeat(2)));
        assert!(!valid_key("short"));
        assert!(!valid_key(&"A".repeat(32)));
        assert!(!valid_key(&"g".repeat(32)));
        assert_eq!(
            key_from_url("/linux-apps/session/0123456789abcdef0123456789abcdef/"),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(key_from_url("/linux-apps/session/nope/"), None);
    }

    #[tokio::test]
    async fn the_api_router_sends_session_paths_to_the_native_proxy() {
        // Mirrors the registration in `crate::api::router` (Lead owns it):
        // the static `session` segment must win over the legacy
        // `/linux-apps/{*rest}` Docker proxy wildcard, because axum ranks
        // static segments above wildcards. If that ever regresses, the native
        // page would be served by the wrong handler.
        use tower::ServiceExt;
        let legacy = axum::Router::new().route(
            "/linux-apps/{*rest}",
            axum::routing::any(|| async { "legacy" }),
        );
        // Exact registration from `crate::api::public_routes`.
        let sessions = axum::Router::new()
            .route(
                "/linux-apps/session/{*rest}",
                axum::routing::any(|| async { "native" }),
            )
            .route(
                "/linux-apps/session/",
                axum::routing::any(|| async { "native" }),
            );
        let app = legacy.merge(sessions);
        let key = "a".repeat(32);
        for path in [
            format!("{SESSION_PATH_PREFIX}{key}/"),
            format!("{SESSION_PATH_PREFIX}{key}/js/Client.js"),
            format!("{SESSION_PATH_PREFIX}{key}/rast-shim.js"),
            format!("{SESSION_PATH_PREFIX}{key}"),
            SESSION_PATH_PREFIX.to_string(),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(&body[..], b"native", "{path} must reach the native proxy");
        }
        // The legacy Docker proxy keeps its own paths.
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/linux-apps/{}/", "b".repeat(32)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"legacy");
    }

    #[test]
    fn the_shim_goes_before_the_body_end() {
        let key = "a".repeat(32);
        let page = inject_shim(
            "<!doctype html><html><head></head><body><script>var x=1;</script></body></html>",
            &key,
        );
        let script = format!("<script src=\"{SESSION_PATH_PREFIX}{key}/rast-shim.js\"></script>");
        let at = page.find(&script).expect("shim script");
        assert!(page[..at].ends_with("<script>var x=1;</script>"));
        assert!(page[at..].starts_with(&script));
        assert!(page.ends_with("</body></html>"));
        assert_eq!(page.matches(&script).count(), 1);
        // A page without </body> still gets the shim (at the end).
        let plain = inject_shim("<div>plain</div>", &key);
        assert!(plain.starts_with("<div>plain</div>"));
        assert!(plain.ends_with("</script>"));
    }

    #[test]
    fn upstream_paths_strip_the_prefix_and_keep_the_query() {
        let key = "a".repeat(32);
        assert_eq!(
            upstream_target(&format!("{SESSION_PATH_PREFIX}{key}/"), None),
            "/"
        );
        assert_eq!(
            upstream_target(&format!("{SESSION_PATH_PREFIX}{key}/js/Client.js"), None),
            "/js/Client.js"
        );
        assert_eq!(
            upstream_target(&format!("{SESSION_PATH_PREFIX}{key}/"), Some("x=1&y=2")),
            "/?x=1&y=2"
        );
    }

    #[test]
    fn the_page_asks_xpra_to_share_the_session() {
        let key = "a".repeat(32);
        let path = format!("{SESSION_PATH_PREFIX}{key}/");
        assert_eq!(
            sharing_redirect(&path, None),
            Some(format!("{path}?sharing=yes"))
        );
        assert_eq!(
            sharing_redirect(&path, Some("debug_main=1")),
            Some(format!("{path}?debug_main=1&sharing=yes"))
        );
        assert_eq!(sharing_redirect(&path, Some("sharing=yes")), None);
        assert_eq!(sharing_redirect(&path, Some("a=1&sharing=no")), None);
    }

    #[test]
    fn policy_allows_websockets_workers_and_embedding() {
        let mut response = plain_error(StatusCode::OK, "x");
        apply_policy(response.headers_mut());
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.contains("connect-src 'self' ws: wss:"));
        assert!(csp.contains("worker-src 'self' blob:"));
        assert!(csp.contains("frame-ancestors 'self'"));
        assert!(!csp.contains("sandbox"));
        assert_eq!(
            response.headers()[header::X_FRAME_OPTIONS]
                .to_str()
                .unwrap(),
            "SAMEORIGIN"
        );
    }

    #[test]
    fn websocket_origins_must_match_the_request_host() {
        assert!(same_origin("https://rast.example.com", "rast.example.com"));
        assert!(same_origin("http://127.0.0.1:8720", "127.0.0.1:8720"));
        assert!(same_origin(
            "https://rast.example.com:443",
            "rast.example.com"
        ));
        assert!(!same_origin("https://evil.example.com", "rast.example.com"));
        assert!(!same_origin("http://other.example.com", "rast.example.com"));
        assert!(!same_origin("null", "rast.example.com"));
        assert!(!same_origin("ftp://rast.example.com", "rast.example.com"));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://rast.example.com"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("rast.example.com"));
        assert!(upgrade_origin_allowed(&headers, None));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example.com"),
        );
        assert!(!upgrade_origin_allowed(&headers, None));
        // A configured public URL is accepted even when the Host is the
        // internal bind address (reverse proxy).
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://rast.example.com"),
        );
        assert!(upgrade_origin_allowed(
            &headers,
            Some("https://rast.example.com/")
        ));
        headers.remove(header::ORIGIN);
        assert!(!upgrade_origin_allowed(&headers, None));
    }

    #[tokio::test]
    async fn authorization_needs_the_owning_accounts_cookie() {
        let (_dir, db) = test_db().await;
        let alice = crate::auth::session::create(&db, 1, 24, "127.0.0.1", "test")
            .await
            .unwrap();
        let bob = crate::auth::session::create(&db, 2, 24, "127.0.0.1", "test")
            .await
            .unwrap();
        // The cookie is validated first and the owner is returned; the key is
        // resolved afterwards and compared with this id by the handler.
        assert_eq!(
            authenticate(&db, 24, &cookie_header(&alice.token))
                .await
                .expect("owner"),
            1
        );
        assert_eq!(
            authenticate(&db, 24, &cookie_header(&bob.token))
                .await
                .expect("bob"),
            2
        );
        // No cookie / garbage cookie.
        assert_eq!(
            authenticate(&db, 24, &HeaderMap::new())
                .await
                .expect_err("no cookie")
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            authenticate(&db, 24, &cookie_header("not-a-token"))
                .await
                .expect_err("bad cookie")
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Revoked / expired sessions are rejected.
        crate::auth::session::revoke(&db, &alice.session_id)
            .await
            .unwrap();
        assert_eq!(
            authenticate(&db, 24, &cookie_header(&alice.token))
                .await
                .expect_err("revoked")
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // The key alone (no cookie) never authorizes a request.
        assert!(authenticate(&db, 24, &HeaderMap::new()).await.is_err());
        // Disabled accounts are rejected even with a live cookie.
        sqlx::query("UPDATE users SET active = 0 WHERE id = 2")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            authenticate(&db, 24, &cookie_header(&bob.token))
                .await
                .expect_err("inactive")
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn http_pages_are_injected_and_assets_pass_through() {
        // A fake Xpra web server: HTML for "/", JavaScript for assets.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    if request.starts_with("GET /js/app.js") {
                        let body = "console.log('xpra');";
                        let response = format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    } else {
                        let body =
                            "<!doctype html><html><body><div id=\"screen\"></div></body></html>";
                        let response = format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                             Set-Cookie: server=should-not-leak\r\nServer: xpra\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                });
            }
        });
        let key = "b".repeat(32);
        let session = session(&key, 1);
        let request = axum::http::Request::builder()
            .uri(format!("{SESSION_PATH_PREFIX}{key}/"))
            .body(Body::empty())
            .unwrap();
        let response = forward(request, session.clone(), port).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(response.headers().get(header::SERVER).is_none());
        assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "SAMEORIGIN");
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(&format!(
            "<script src=\"{SESSION_PATH_PREFIX}{key}/rast-shim.js\"></script>"
        )));
        assert!(body.contains("<div id=\"screen\">"));
        // Assets are relayed unchanged, including content type.
        let request = axum::http::Request::builder()
            .uri(format!("{SESSION_PATH_PREFIX}{key}/js/app.js"))
            .body(Body::empty())
            .unwrap();
        let response = forward(request, session, port).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/javascript"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"console.log('xpra');");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_relay_counts_viewers_and_input() {
        // A fake Xpra WebSocket server that echoes text frames.
        let upstream_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = upstream_listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut websocket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = websocket.next().await {
                        let reply = match message {
                            UpstreamMessage::Text(text) => {
                                UpstreamMessage::text(format!("echo:{text}"))
                            }
                            other => other,
                        };
                        if websocket.send(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let key = "c".repeat(32);
        let session = session(&key, 1);
        let before = session.last_input_for_test();
        let bridge_session = session.clone();
        let app = axum::Router::new().route(
            "/bridge",
            get(move |upgrade: WebSocketUpgrade| {
                let session = bridge_session.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        let stream = TcpStream::connect(("127.0.0.1", upstream_port))
                            .await
                            .expect("upstream connect");
                        let (upstream, _) = tokio_tungstenite::client_async(
                            format!("ws://127.0.0.1:{upstream_port}/"),
                            stream,
                        )
                        .await
                        .expect("upstream handshake");
                        relay(socket, upstream, session).await;
                    })
                }
            }),
        );
        let server = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(server, app).await;
        });
        let (mut viewer, _) = tokio_tungstenite::connect_async(format!("ws://{address}/bridge"))
            .await
            .expect("viewer connect");
        // Give the relay a moment to count the viewer.
        for _ in 0..20 {
            if session.viewers_for_test() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(session.viewers_for_test(), 1, "viewer joined");
        viewer
            .send(UpstreamMessage::text("m,10,20"))
            .await
            .expect("send input");
        let answer = tokio::time::timeout(Duration::from_secs(5), viewer.next())
            .await
            .expect("an answer")
            .expect("a frame")
            .expect("a valid frame");
        assert_eq!(answer.into_text().unwrap(), "echo:m,10,20");
        assert!(
            session.last_input_for_test() > before,
            "client frames count as input"
        );
        drop(viewer);
        for _ in 0..40 {
            if session.viewers_for_test() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(session.viewers_for_test(), 0, "viewer left");
        serving.abort();
    }
}
