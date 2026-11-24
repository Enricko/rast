//! Short-lived stream-token broker and authenticated native WebSocket relay.
//!
//! The browser HTML5 client attaches to a person's Xpra session through
//! [`super::native_proxy`]: it presents the RAST session cookie, a same-origin
//! `Origin` and takes the session key from the page URL. The **native** Xpra
//! client (RAST Desktop) is not a browser: it has no cookie jar and sends no
//! `Origin`. It needs a different, even narrower credential.
//!
//! This module issues that credential: a 120 s, single-use bearer token that an
//! already-authenticated API caller obtains with
//! `POST /api/linux-apps/session/stream-token` (cookie + CSRF, like every other
//! mutating API route). The token is bound to that account, to the RAST login
//! session that requested it and to the exact live [`NativeSession`], and is
//! consumed by the first successful WebSocket upgrade:
//!
//! ```text
//! RAST Desktop ──HTTPS POST /api/linux-apps/session/stream-token──▶ token+url
//! official Xpra ──WSS GET /linux-apps/native/<token>/<rest>?<query>──▶ this relay
//!                                                                    │ loopback
//!                                              ws://127.0.0.1:<xpra-port>/<rest>
//! ```
//!
//! Security decisions (deliberate, documented in `docs/NATIVE-BROKER.md`):
//!
//! * The attach route is mounted **outside** cookie authentication and
//!   **outside** the browser same-origin `Origin` check: the native client can
//!   satisfy neither. The single-use stream token is the sole credential of
//!   that route. It is only ever issued to an authenticated account.
//! * The token is 32 bytes from the OS CSPRNG, base64url encoded (43 chars).
//!   Only `SHA-256(token)` is kept, in memory, for at most 120 s; the token
//!   itself is never stored, logged, audited or put in an error body, and the
//!   URL returned to the client is never logged either.
//! * It is consumed atomically on the first successful upstream upgrade, so a
//!   leaked URL is useless after the legitimate client connected — and useless
//!   after 120 s in any case.
//! * It is bound to the user id **and** the native session key, so it cannot be
//!   replayed against another account and stops working as soon as the session
//!   it names is gone. The issuing RAST login session is re-checked at attach
//!   time and every 5 s while the stream is live (logout, revoke, expiry and
//!   deactivation all close it).
//! * At most [`MAX_STREAM_TOKENS_PER_USER`] tokens are outstanding per account
//!   (the oldest is evicted), issuance and attach attempts are rate limited,
//!   and an invalid token gets a bodyless `401` that never echoes it.
//! * Xpra keeps listening on loopback only: the relay is a WebSocket *client*
//!   to `127.0.0.1`, and no new public port is opened. The frames are relayed
//!   by the same code as the browser stream (same 64 MiB message cap, same
//!   ping/pong/close handling, same viewer/input accounting), so the HTML5 path
//!   keeps working unchanged.
//!
//! Audit events (existing activity log, no secrets): `linux_apps.stream_token.issued`,
//! `linux_apps.stream_token.denied`, `linux_apps.stream.attached`.

use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        ConnectInfo, FromRequestParts, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::{
    native_proxy::{
        auth_still_valid, is_websocket, relay_halves_traced, relay_until, MAX_WS_MESSAGE,
        REVALIDATE_EVERY,
    },
    native_session::NativeSession,
};

use crate::{
    audit::{self, AuditLog},
    auth::{self, rate_limit::Decision},
    state::AppState,
};

/// Path prefix of the native stream relay: `GET /linux-apps/native/<token>/…`.
pub const STREAM_PATH_PREFIX: &str = "/linux-apps/native/";
/// How long an issued stream token stays valid. A client obtains a fresh one
/// for every (re)connect; it is deliberately short.
pub const STREAM_TOKEN_TTL: Duration = Duration::from_secs(120);
/// [`STREAM_TOKEN_TTL`] in whole seconds (the client-facing `ttlSeconds`).
pub const STREAM_TOKEN_TTL_SECONDS: u64 = 120;
/// At most this many outstanding tokens per account (the oldest is evicted).
pub const MAX_STREAM_TOKENS_PER_USER: usize = 8;
/// Issuance requests allowed per account per minute.
const MAX_TOKEN_ISSUES_PER_MINUTE: usize = 30;
/// Attach attempts allowed per client address per minute. Guessing tokens is
/// useless against 256 random bits; the limit keeps denial responses from
/// turning into an audit-log write amplifier.
const MAX_ATTACH_ATTEMPTS_PER_MINUTE: usize = 60;
/// Random token entropy in bytes (32 bytes -> 43 base64url characters).
const TOKEN_BYTES: usize = 32;
const TOKEN_CHARS: usize = 43;
/// Slack for the loopback Xpra WebSocket handshake.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const AUDIT_ISSUED: &str = "linux_apps.stream_token.issued";
const AUDIT_DENIED: &str = "linux_apps.stream_token.denied";
const AUDIT_ATTACHED: &str = "linux_apps.stream.attached";

/// Additive desktop capability advertised in `GET /api/bootstrap`.
pub const DESKTOP_PROTOCOL: u8 = 1;
/// Oldest RAST Desktop release that speaks this protocol.
pub const DESKTOP_MIN_CLIENT_VERSION: &str = "0.1.0";

/// The `desktop` object of `GET /api/bootstrap` (public endpoint, additive).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCapability {
    pub protocol: u8,
    /// The server implements the short-lived stream-token broker.
    pub stream_tokens: bool,
    pub min_client_version: &'static str,
}

/// The capability object every RAST Desktop client checks before using native
/// streaming (`docs/API.md`, protocol §2). Older servers omit it entirely.
pub fn desktop_capability() -> DesktopCapability {
    DesktopCapability {
        protocol: DESKTOP_PROTOCOL,
        stream_tokens: true,
        min_client_version: DESKTOP_MIN_CLIENT_VERSION,
    }
}

// ---------------------------------------------------------------------------
// Grant store
// ---------------------------------------------------------------------------

/// One outstanding stream token. The token itself is *not* stored: only its
/// SHA-256 hash.
#[derive(Debug, Clone)]
pub(crate) struct Grant {
    /// Account the token was issued to.
    pub(crate) user_id: i64,
    /// RAST login session that issued the token (revocation closes the stream).
    pub(crate) auth_session_id: String,
    /// Secret key of the exact [`NativeSession`] the token is bound to.
    pub(crate) session_key: String,
    token_hash: [u8; 32],
    expires_at: Instant,
}

/// Result of looking a token up without consuming it.
#[derive(Debug)]
pub(crate) enum TokenLookup {
    Valid(Grant),
    Expired,
    Unknown,
}

/// What [`NativeBroker::issue`] hands back to the API caller.
#[derive(Debug)]
pub(crate) struct IssuedStreamToken {
    pub(crate) token: String,
    pub(crate) expires_at_ms: i64,
    pub(crate) ttl_seconds: u64,
}

/// In-memory broker of single-use native stream tokens.
///
/// Everything is `Mutex`-protected and bounded: [`MAX_STREAM_TOKENS_PER_USER`]
/// grants per account, pruned on every operation. No persistence by design —
/// a restarted server invalidates every token (the client reconnects with a
/// fresh one).
pub(crate) struct NativeBroker {
    grants: Mutex<VecDeque<Grant>>,
    ttl: Duration,
    max_outstanding: usize,
    issue_limiter: auth::rate_limit::RateLimiter,
    attach_limiter: auth::rate_limit::RateLimiter,
}

impl NativeBroker {
    pub(crate) fn new() -> Self {
        Self::with_limits(
            STREAM_TOKEN_TTL,
            MAX_STREAM_TOKENS_PER_USER,
            MAX_TOKEN_ISSUES_PER_MINUTE,
            MAX_ATTACH_ATTEMPTS_PER_MINUTE,
        )
    }

    /// The production limits; tests pass shorter windows/counts.
    fn with_limits(
        ttl: Duration,
        max_outstanding: usize,
        issues_per_minute: usize,
        attaches_per_minute: usize,
    ) -> Self {
        Self {
            grants: Mutex::new(VecDeque::new()),
            ttl,
            max_outstanding: max_outstanding.max(1),
            issue_limiter: auth::rate_limit::RateLimiter::new(
                issues_per_minute.max(1),
                Duration::from_secs(60),
            ),
            attach_limiter: auth::rate_limit::RateLimiter::new(
                attaches_per_minute.max(1),
                Duration::from_secs(60),
            ),
        }
    }

    fn grants(&self) -> MutexGuard<'_, VecDeque<Grant>> {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Issue a token bound to `user_id`, the RAST login session that requested
    /// it and the live native session identified by `session_key`.
    pub(crate) fn issue(
        &self,
        user_id: i64,
        auth_session_id: &str,
        session_key: &str,
    ) -> IssuedStreamToken {
        self.issue_with_ttl(user_id, auth_session_id, session_key, self.ttl)
    }

    fn issue_with_ttl(
        &self,
        user_id: i64,
        auth_session_id: &str,
        session_key: &str,
        ttl: Duration,
    ) -> IssuedStreamToken {
        let token = random_token();
        let now = Instant::now();
        let expires_at_ms = now_ms_expiry(ttl);
        let grant = Grant {
            user_id,
            auth_session_id: auth_session_id.to_string(),
            session_key: session_key.to_string(),
            token_hash: hash_token(&token),
            expires_at: now + ttl,
        };
        let mut grants = self.grants();
        grants.retain(|grant| grant.expires_at > now);
        // Evict this account's oldest grants until there is room, then add.
        while grants
            .iter()
            .filter(|grant| grant.user_id == user_id)
            .count()
            >= self.max_outstanding
        {
            let Some(position) = grants.iter().position(|grant| grant.user_id == user_id) else {
                break;
            };
            grants.remove(position);
        }
        grants.push_back(grant);
        IssuedStreamToken {
            token,
            expires_at_ms,
            ttl_seconds: ttl.as_secs(),
        }
    }

    /// Look a token up without consuming it. Expired grants are pruned; a
    /// found grant is cloned so the handler can resolve the session before the
    /// single-use consume (a failed attach must not burn the token).
    pub(crate) fn peek(&self, token: &str) -> TokenLookup {
        if !valid_token(token) {
            return TokenLookup::Unknown;
        }
        let hash = hash_token(token);
        let now = Instant::now();
        let mut grants = self.grants();
        let found = grants.iter().position(|grant| grant.token_hash == hash);
        match found {
            Some(position) => {
                let grant = grants[position].clone();
                if grant.expires_at <= now {
                    grants.remove(position);
                    TokenLookup::Expired
                } else {
                    TokenLookup::Valid(grant)
                }
            }
            None => {
                grants.retain(|grant| grant.expires_at > now);
                TokenLookup::Unknown
            }
        }
    }

    /// Consume a token atomically. Returns `true` exactly once per issued token
    /// (and never after the TTL). Called after the upstream Xpra WebSocket
    /// handshake succeeded and before the client receives its `101`, so a
    /// concurrent second attach loses cleanly.
    pub(crate) fn consume(&self, token: &str) -> bool {
        if !valid_token(token) {
            return false;
        }
        let hash = hash_token(token);
        let now = Instant::now();
        let mut grants = self.grants();
        let Some(position) = grants.iter().position(|grant| grant.token_hash == hash) else {
            return false;
        };
        let Some(grant) = grants.remove(position) else {
            return false;
        };
        grant.expires_at > now
    }

    /// Revoke every outstanding token of an account; returns how many were
    /// removed (the `DELETE /api/linux-apps/session/stream-tokens` answer).
    pub(crate) fn revoke_all(&self, user_id: i64) -> usize {
        let mut grants = self.grants();
        let before = grants.len();
        grants.retain(|grant| grant.user_id != user_id);
        before - grants.len()
    }

    /// Account issuance rate limit; `Err(retry_after_secs)` means `429`.
    pub(crate) fn admit_issue(&self, user_id: i64) -> Result<(), u64> {
        match self.issue_limiter.check(&user_id.to_string()) {
            Decision::Allowed => Ok(()),
            Decision::Limited { retry_after_secs } => Err(retry_after_secs),
        }
    }

    /// Attach-attempt rate limit, keyed by client address.
    pub(crate) fn admit_attach(&self, client: &str) -> Result<(), u64> {
        match self.attach_limiter.check(client) {
            Decision::Allowed => Ok(()),
            Decision::Limited { retry_after_secs } => Err(retry_after_secs),
        }
    }

    #[cfg(test)]
    fn outstanding_for_test(&self, user_id: i64) -> usize {
        self.grants()
            .iter()
            .filter(|grant| grant.user_id == user_id)
            .count()
    }

    #[cfg(test)]
    fn total_for_test(&self) -> usize {
        self.grants().len()
    }
}

impl Default for NativeBroker {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms_expiry(ttl: Duration) -> i64 {
    crate::db::now_ms() + i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of a token: the only form the server keeps. Tokens are high-entropy
/// random values, so a fast hash is the right primitive (same reasoning as
/// RAST login sessions).
fn hash_token(token: &str) -> [u8; 32] {
    let digest = Sha256::digest(token.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

/// Exactly 43 base64url characters — checked before hashing so arbitrary
/// request paths cannot make the broker do expensive work.
pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == TOKEN_CHARS
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A token may only be used against the exact live session it was issued for:
/// same account, same session key and the Xpra upstream actually listening.
pub(crate) fn grant_matches_session(grant: &Grant, session: &NativeSession) -> bool {
    grant.user_id == session.user_id()
        && grant.session_key == session.key()
        && session.upstream_port().is_some()
}

// ---------------------------------------------------------------------------
// URL and path helpers
// ---------------------------------------------------------------------------

/// Split `/linux-apps/native/<token>/<rest>` into the token and the upstream
/// path remainder (empty when the client attaches to the token root). Both the
/// with- and without-trailing-slash forms are accepted.
pub(crate) fn token_and_rest(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(STREAM_PATH_PREFIX)?;
    match rest.split_once('/') {
        Some((token, tail)) => Some((token, tail)),
        None => Some((rest, "")),
    }
}

/// The loopback Xpra WebSocket target for a relayed attach: strip RAST's token
/// prefix, keep the client's remainder and query verbatim.
pub(crate) fn upstream_ws_target(rest: &str, query: Option<&str>) -> String {
    let mut target = format!("/{rest}");
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    target
}

/// Build the `wss://…/linux-apps/native/<token>/` URL handed to the client.
///
/// Uses the configured `public_url` when present (its scheme selects `wss` or
/// `ws`); otherwise the request `Host` plus `X-Forwarded-Proto`, falling back
/// to the `secure_cookies` setting when the proxy header is absent. Returns
/// `None` when there is no usable authority, which the API turns into a
/// deliberate error instead of guessing.
pub(crate) fn stream_url(
    headers: &HeaderMap,
    public_url: Option<&str>,
    secure_cookies: bool,
    token: &str,
) -> Option<String> {
    let (scheme, authority) = match public_url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(public_url) => {
            let (scheme, rest) = public_url.split_once("://")?;
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("").trim();
            if !valid_authority(authority) {
                return None;
            }
            let scheme = match scheme.to_ascii_lowercase().as_str() {
                "https" => "wss",
                "http" => "ws",
                _ => return None,
            };
            (scheme, authority.to_string())
        }
        None => {
            let host = headers.get(header::HOST)?.to_str().ok()?.trim();
            if !valid_authority(host) {
                return None;
            }
            let forwarded = headers
                .get("x-forwarded-proto")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .map(|value| value.trim().to_ascii_lowercase());
            let scheme = match forwarded.as_deref() {
                Some("https") => "wss",
                Some("http") => "ws",
                _ if secure_cookies => "wss",
                _ => "ws",
            };
            (scheme, host.to_string())
        }
    };
    Some(format!(
        "{scheme}://{authority}{STREAM_PATH_PREFIX}{token}/"
    ))
}

/// A conservative `Host`/`public_url` authority: no whitespace, no path, no
/// userinfo. The value is only echoed back to the caller (who supplied it), but
/// rejecting junk keeps malformed URLs out of the API response.
fn valid_authority(authority: &str) -> bool {
    !authority.is_empty()
        && authority.len() <= 255
        && authority.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']' | b'_')
        })
}

/// Coarse remote-address class recorded with the attach audit event (the
/// protocol deliberately does not put the exact address in the activity log).
pub(crate) fn remote_address_class(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(ip) => {
            if ip.is_loopback() {
                "loopback"
            } else if ip.is_private() || ip.is_link_local() {
                "private"
            } else {
                "public"
            }
        }
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                "loopback"
            } else if ip.is_unique_local() || ip.is_unicast_link_local() {
                "private"
            } else {
                "public"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Native attach (WebSocket upgrade + relay)
// ---------------------------------------------------------------------------

type Upstream = tokio_tungstenite::WebSocketStream<TcpStream>;

/// Gated diagnostics switch used by the native relay (the browser path never
/// reads it). Set `RAST_NATIVE_RELAY_DEBUG=1` on the server to get info-level
/// frame counts/sizes and close reasons from native attaches. Frame contents,
/// the stream token, cookies and the issued URL are never logged.
fn relay_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("RAST_NATIVE_RELAY_DEBUG").is_some_and(|value| {
            let value = value.to_string_lossy();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
    })
}

/// Handler for `GET /linux-apps/native/<token>/<rest>?<query>` with
/// `Upgrade: websocket`.
///
/// Mounted as a **public** route (no cookie auth, no browser `Origin` check):
/// the single-use stream token in the path is the credential. Validation never
/// reveals which part failed (bodyless `401`), and the token is only consumed
/// once the loopback Xpra handshake succeeded.
pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let Some((token, rest)) = token_and_rest(&path) else {
        return unauthorized();
    };
    let token = token.to_string();
    let rest = rest.to_string();
    if !valid_token(&token) || !is_websocket(request.headers()) {
        return unauthorized();
    }
    let query = request.uri().query().map(str::to_string);
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let client_ip = auth::client_ip(request.headers(), peer_ip, &state.config);
    let broker = state.linux_apps.native_broker().clone();
    if let Err(retry_after_secs) = broker.admit_attach(&client_ip.to_string()) {
        return too_many_requests(retry_after_secs);
    }
    let grant = match broker.peek(&token) {
        TokenLookup::Valid(grant) => grant,
        TokenLookup::Expired => return deny(&state, None, "expired token").await,
        TokenLookup::Unknown => return deny(&state, None, "unknown token").await,
    };
    // The token names one exact session; authorization comes from the token,
    // but the session still has to be the one it was issued for and be live.
    let Some(session) = state.linux_apps.find_native_session(&grant.session_key) else {
        return deny(&state, Some(grant.user_id), "session is gone").await;
    };
    if !grant_matches_session(&grant, &session) {
        return deny(&state, Some(grant.user_id), "token/session mismatch").await;
    }
    if !auth_still_valid(&state.db, grant.user_id, &grant.auth_session_id).await {
        return deny(&state, Some(grant.user_id), "login session revoked").await;
    }
    let debug = relay_debug_enabled();
    let target = upstream_ws_target(&rest, query.as_deref());
    if debug {
        // `target` is the loopback Xpra path: the token prefix was stripped, so
        // this is not secret. The 5 KiB Xpra hello/packet contents are never
        // logged.
        tracing::info!(
            target = %target,
            has_query = query.is_some(),
            rest_len = rest.len(),
            "native relay: attach request"
        );
    }
    let Some(port) = session.upstream_port() else {
        return deny(&state, Some(grant.user_id), "session is gone").await;
    };
    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    // Sub-protocols are negotiated end to end so the native client keeps the
    // same framing/semantics as a direct Xpra connection; credentials are not
    // forwarded (only this header is).
    let protocols = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();
    let Some((upstream, selected_protocol)) = connect_upstream(port, &target, protocols).await
    else {
        if debug {
            tracing::info!(target = %target, "native relay: upstream websocket handshake failed");
        }
        return deny(&state, Some(grant.user_id), "upstream unavailable").await;
    };
    if debug {
        tracing::info!(
            target = %target,
            protocol = ?selected_protocol,
            "native relay: upstream websocket connected"
        );
    }
    // Single use: consume atomically before the client sees its 101. A second
    // attach racing on the same token loses here and gets a bodyless 401.
    if !broker.consume(&token) {
        return unauthorized();
    }
    audit_attached(&state, grant.user_id, client_ip).await;
    let upgrade = match selected_protocol {
        Some(protocol) => upgrade.protocols([protocol]),
        None => upgrade,
    };
    let user_id = grant.user_id;
    let auth_session_id = grant.auth_session_id;
    let relay_state = state.clone();
    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| async move {
            relay(
                socket,
                upstream,
                session,
                relay_state,
                user_id,
                auth_session_id,
                debug,
            )
            .await;
        })
}

/// Connect to the loopback Xpra WebSocket, forwarding the client's
/// sub-protocol offer and reporting the one the upstream selected.
pub(crate) async fn connect_upstream(
    port: u16,
    target: &str,
    protocols: Option<HeaderValue>,
) -> Option<(Upstream, Option<String>)> {
    let mut request = format!("ws://127.0.0.1:{port}{target}")
        .into_client_request()
        .ok()?;
    if let Some(protocols) = protocols {
        request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, protocols);
    }
    let connect = async {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        tokio_tungstenite::client_async(request, stream).await.ok()
    };
    let (upstream, answer) = tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, connect)
        .await
        .ok()??;
    let selected = answer
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    Some((upstream, selected))
}

/// Relay frames both ways until either side closes or the stream is revoked,
/// re-checking every [`REVALIDATE_EVERY`] that the RAST login session behind
/// the token is still valid and that the native session is still live.
async fn relay(
    client: WebSocket,
    upstream: Upstream,
    session: std::sync::Arc<NativeSession>,
    state: AppState,
    user_id: i64,
    auth_session_id: String,
    debug: bool,
) {
    let (to_app, to_client) = relay_halves_traced(client, upstream, session.clone(), debug);
    let watcher_session = session.clone();
    let mut revalidate = tokio::time::interval(REVALIDATE_EVERY);
    revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let revoked = async move {
        loop {
            revalidate.tick().await;
            if watcher_session.upstream_port().is_none()
                || !auth_still_valid(&state.db, user_id, &auth_session_id).await
            {
                tracing::info!(user_id, "closing a native stream after session revocation");
                return;
            }
        }
    };
    relay_until(to_app, to_client, session, revoked).await;
    if debug {
        tracing::info!(user_id, "native relay: connection finished");
    }
}

// ---------------------------------------------------------------------------
// Audit and failure responses
// ---------------------------------------------------------------------------

async fn audit_attached(state: &AppState, user_id: i64, client_ip: IpAddr) {
    let mut log = AuditLog::new(AUDIT_ATTACHED)
        .object("native_session", "native")
        .success()
        .detail(format!(
            "remote_address_class={}",
            remote_address_class(client_ip)
        ));
    log.user_id = Some(user_id);
    audit::record_with_events(&state.db, &state.events, log).await;
}

/// Audit a denied attach and answer with a bodyless 401. `reason` is a static
/// category: the token, URL, session key and cookie never reach the audit log.
async fn deny(state: &AppState, user_id: Option<i64>, reason: &'static str) -> Response {
    let mut log = AuditLog::new(AUDIT_DENIED)
        .object("native_stream", "native")
        .denied()
        .detail(reason);
    log.user_id = user_id;
    audit::record_with_events(&state.db, &state.events, log).await;
    unauthorized()
}

/// A bodyless, non-leaking failure: no token, no URL, no detail.
fn unauthorized() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "").into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn too_many_requests(retry_after_secs: u64) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "").into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        headers.insert(header::RETRY_AFTER, value);
    }
    response
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use axum::{http::Uri, routing::get, Router};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream as TokioTcpStream};
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request as WsRequest, Response as WsResponse},
        Message as WsMessage,
    };

    use super::*;

    fn fake_token() -> String {
        "A".repeat(TOKEN_CHARS)
    }

    // -- token format / hashing -------------------------------------------------

    #[test]
    fn tokens_are_32_random_base64url_bytes_and_only_hashes_are_stored() {
        let broker = NativeBroker::new();
        let one = broker.issue(1, "auth-session", "a".repeat(32).as_str());
        let two = broker.issue(1, "auth-session", "a".repeat(32).as_str());
        assert_eq!(one.token.len(), TOKEN_CHARS);
        assert_eq!(two.token.len(), TOKEN_CHARS);
        assert_ne!(one.token, two.token, "tokens must come from the OS CSPRNG");
        assert!(valid_token(&one.token));
        assert!(one
            .token
            .bytes()
            .all(|byte| { byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' }));
        // The token itself never enters the store; only its SHA-256 does.
        let grants = broker.grants();
        assert_eq!(grants.len(), 2);
        let debug = format!("{:?}", *grants);
        assert!(
            !debug.contains(&one.token) && !debug.contains(&two.token),
            "the broker must never keep the token itself"
        );
        assert_eq!(grants[0].token_hash, hash_token(&one.token));
        assert_eq!(&grants[0].token_hash[..], &hash_token(&one.token)[..]);
    }

    #[test]
    fn only_well_formed_tokens_are_recognized() {
        assert!(valid_token(&fake_token()));
        assert!(!valid_token("short"));
        assert!(!valid_token(&"A".repeat(TOKEN_CHARS - 1)));
        assert!(!valid_token(&"A".repeat(TOKEN_CHARS + 1)));
        assert!(!valid_token(&"!".repeat(TOKEN_CHARS)));
        let broker = NativeBroker::new();
        assert!(matches!(broker.peek("not-a-token"), TokenLookup::Unknown));
        assert!(!broker.consume("not-a-token"));
    }

    // -- TTL / single use -------------------------------------------------------

    #[test]
    fn tokens_expire_after_their_ttl() {
        let broker = NativeBroker::new();
        let issued = broker.issue_with_ttl(
            7,
            "auth",
            "b".repeat(32).as_str(),
            Duration::from_millis(40),
        );
        assert!(matches!(broker.peek(&issued.token), TokenLookup::Valid(_)));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            matches!(broker.peek(&issued.token), TokenLookup::Expired),
            "an expired token must not be usable"
        );
        assert!(!broker.consume(&issued.token));
        assert_eq!(broker.total_for_test(), 0, "expired grants are pruned");
    }

    #[test]
    fn a_token_is_consumed_exactly_once_even_under_a_race() {
        let broker = Arc::new(NativeBroker::new());
        let issued = broker.issue(1, "auth", "c".repeat(32).as_str());
        assert!(broker.consume(&issued.token));
        assert!(!broker.consume(&issued.token), "a token is single use");
        assert!(matches!(broker.peek(&issued.token), TokenLookup::Unknown));

        let racing = broker.issue(1, "auth", "c".repeat(32).as_str());
        let winners = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let broker = broker.clone();
                let token = racing.token.clone();
                let winners = winners.clone();
                scope.spawn(move || {
                    if broker.consume(&token) {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::SeqCst), 1, "exactly one winner");
    }

    // -- binding / eviction / revocation ---------------------------------------

    #[test]
    fn a_token_is_bound_to_its_user_and_live_session() {
        let broker = NativeBroker::new();
        let session_key = "c".repeat(32);
        let issued = broker.issue(1, "auth-session", &session_key);
        let grant = match broker.peek(&issued.token) {
            TokenLookup::Valid(grant) => grant,
            other => panic!("expected a valid grant, got {other:?}"),
        };
        assert_eq!(grant.user_id, 1);
        assert_eq!(grant.session_key, session_key);
        assert_eq!(grant.auth_session_id, "auth-session");

        let live = NativeSession::for_test(session_key.clone(), 1);
        live.set_upstream_port_for_test(Some(8720));
        assert!(grant_matches_session(&grant, &live));

        // A restarted session has a different key...
        let restarted = NativeSession::for_test("d".repeat(32), 1);
        restarted.set_upstream_port_for_test(Some(8721));
        assert!(!grant_matches_session(&grant, &restarted));

        // ...and an ended session has no upstream port any more.
        live.set_upstream_port_for_test(None);
        assert!(!grant_matches_session(&grant, &live));

        // Another account can never use the grant, even with a colliding key.
        let other_account = NativeSession::for_test(session_key, 2);
        other_account.set_upstream_port_for_test(Some(8722));
        assert!(!grant_matches_session(&grant, &other_account));
    }

    #[test]
    fn at_most_eight_tokens_are_outstanding_per_user_and_the_oldest_evicted() {
        let broker = NativeBroker::new();
        let session = "e".repeat(32);
        let mut tokens = Vec::new();
        for _ in 0..(MAX_STREAM_TOKENS_PER_USER + 1) {
            tokens.push(broker.issue(1, "auth", &session).token);
        }
        assert_eq!(
            broker.outstanding_for_test(1),
            MAX_STREAM_TOKENS_PER_USER,
            "old grants must be evicted"
        );
        assert!(
            matches!(broker.peek(&tokens[0]), TokenLookup::Unknown),
            "the oldest token is evicted"
        );
        for token in &tokens[1..] {
            assert!(matches!(broker.peek(token), TokenLookup::Valid(_)));
        }
        // Another account's tokens do not count toward this account's limit.
        for _ in 0..MAX_STREAM_TOKENS_PER_USER {
            broker.issue(2, "auth-2", &session);
        }
        assert_eq!(broker.outstanding_for_test(2), MAX_STREAM_TOKENS_PER_USER);
        assert_eq!(broker.outstanding_for_test(1), MAX_STREAM_TOKENS_PER_USER);
    }

    #[test]
    fn revoking_all_tokens_only_affects_that_user() {
        let broker = NativeBroker::new();
        let session = "f".repeat(32);
        for _ in 0..3 {
            broker.issue(1, "auth", &session);
        }
        for _ in 0..2 {
            broker.issue(2, "auth", &session);
        }
        assert_eq!(broker.revoke_all(1), 3);
        assert_eq!(broker.outstanding_for_test(1), 0);
        assert_eq!(broker.outstanding_for_test(2), 2);
        assert_eq!(broker.revoke_all(1), 0, "revoking again is a no-op");
    }

    #[test]
    fn issuance_is_rate_limited_per_account() {
        let broker = NativeBroker::with_limits(STREAM_TOKEN_TTL, 8, 2, 10);
        assert!(broker.admit_issue(1).is_ok());
        assert!(broker.admit_issue(1).is_ok());
        assert!(broker.admit_issue(1).is_err(), "third request is limited");
        assert!(broker.admit_issue(2).is_ok(), "limits are per account");
    }

    // -- URL / path helpers -----------------------------------------------------

    #[test]
    fn the_stream_url_comes_from_public_url_or_the_forwarded_request() {
        let token = fake_token();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("internal:8720"));
        // Configured public URL wins, HTTPS becomes WSS.
        assert_eq!(
            stream_url(&headers, Some("https://desktop.example.com"), false, &token),
            Some(format!(
                "wss://desktop.example.com{STREAM_PATH_PREFIX}{token}/"
            ))
        );
        // A trailing slash or path in public_url is tolerated (authority only).
        assert_eq!(
            stream_url(
                &headers,
                Some("http://desktop.example.com/rast"),
                false,
                &token
            ),
            Some(format!(
                "ws://desktop.example.com{STREAM_PATH_PREFIX}{token}/"
            ))
        );
        // No public URL: Host + X-Forwarded-Proto, as the reverse proxy sees it.
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https,http"));
        assert_eq!(
            stream_url(&headers, None, false, &token),
            Some(format!("wss://internal:8720{STREAM_PATH_PREFIX}{token}/"))
        );
        // No forwarded proto: the secure-cookie setting decides.
        headers.remove("x-forwarded-proto");
        assert_eq!(
            stream_url(&headers, None, true, &token),
            Some(format!("wss://internal:8720{STREAM_PATH_PREFIX}{token}/"))
        );
        assert_eq!(
            stream_url(&headers, None, false, &token),
            Some(format!("ws://internal:8720{STREAM_PATH_PREFIX}{token}/"))
        );
        // No Host and no public URL: refuse to guess.
        assert_eq!(stream_url(&HeaderMap::new(), None, true, &token), None);
        // Header junk never lands in the URL.
        headers.insert(header::HOST, HeaderValue::from_static("evil/../x"));
        assert_eq!(stream_url(&headers, None, true, &token), None);
    }

    #[test]
    fn attach_paths_strip_the_token_and_keep_the_rest() {
        let token = fake_token();
        assert_eq!(
            token_and_rest(&format!("{STREAM_PATH_PREFIX}{token}/")),
            Some((token.as_str(), ""))
        );
        assert_eq!(
            token_and_rest(&format!("{STREAM_PATH_PREFIX}{token}")),
            Some((token.as_str(), ""))
        );
        assert_eq!(
            token_and_rest(&format!("{STREAM_PATH_PREFIX}{token}/js/Client.js")),
            Some((token.as_str(), "js/Client.js"))
        );
        assert_eq!(token_and_rest("/linux-apps/native/"), Some(("", "")));
        assert_eq!(token_and_rest("/linux-apps/session/abc/"), None);
        assert_eq!(token_and_rest("/linux-apps/"), None);
    }

    #[test]
    fn the_upstream_target_keeps_rest_and_query_verbatim() {
        assert_eq!(upstream_ws_target("", None), "/");
        assert_eq!(upstream_ws_target("", Some("sharing=yes")), "/?sharing=yes");
        assert_eq!(upstream_ws_target("", Some("")), "/?");
        assert_eq!(
            upstream_ws_target("js/Client.js", Some("a=1&b=2")),
            "/js/Client.js?a=1&b=2"
        );
        assert_eq!(upstream_ws_target("chat/room", None), "/chat/room");
    }

    #[test]
    fn remote_addresses_are_classified_not_recorded_exactly() {
        assert_eq!(
            remote_address_class("127.0.0.1".parse().unwrap()),
            "loopback"
        );
        assert_eq!(remote_address_class("::1".parse().unwrap()), "loopback");
        assert_eq!(remote_address_class("10.0.0.5".parse().unwrap()), "private");
        assert_eq!(
            remote_address_class("172.16.1.2".parse().unwrap()),
            "private"
        );
        assert_eq!(remote_address_class("8.8.8.8".parse().unwrap()), "public");
    }

    // -- bootstrap capability ---------------------------------------------------

    #[test]
    fn the_bootstrap_desktop_capability_matches_the_protocol() {
        assert_eq!(
            serde_json::to_value(desktop_capability()).unwrap(),
            json!({"protocol": 1, "streamTokens": true, "minClientVersion": "0.1.0"})
        );
    }

    // -- routing ---------------------------------------------------------------

    #[tokio::test]
    async fn the_public_router_keeps_native_stream_paths_away_from_the_legacy_proxy() {
        // Mirrors `crate::api::public_routes`: the static `native` segment must
        // win over the legacy `/linux-apps/{*rest}` Docker proxy wildcard, the
        // same way `/linux-apps/session/…` already does. A regression here
        // would send the native client to the wrong handler.
        use tower::ServiceExt;
        let legacy = axum::Router::new().route(
            "/linux-apps/{*rest}",
            axum::routing::any(|| async { "legacy" }),
        );
        let native = axum::Router::new()
            .route(
                "/linux-apps/native/{*rest}",
                axum::routing::any(|| async { "native" }),
            )
            .route(
                "/linux-apps/native/",
                axum::routing::any(|| async { "native" }),
            );
        let app = legacy.merge(native);
        let token = fake_token();
        for path in [
            format!("{STREAM_PATH_PREFIX}{token}/"),
            format!("{STREAM_PATH_PREFIX}{token}/js/Client.js?x=1"),
            format!("{STREAM_PATH_PREFIX}{token}"),
            STREAM_PATH_PREFIX.to_string(),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(&body[..], b"native", "{path} must reach the native broker");
        }
        // The legacy Docker proxy keeps its own paths.
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/linux-apps/{}/", "b".repeat(32)))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"legacy");
    }

    // -- upstream handshake / relay ---------------------------------------------

    /// A mock Xpra WebSocket upstream that records the request path/protocol,
    /// sends one frame *immediately* after its handshake (before any client
    /// frame) and then echoes every frame back.
    async fn mock_xpra_upstream(
        send_welcome: bool,
    ) -> (
        u16,
        oneshot::Receiver<String>,
        oneshot::Receiver<Option<String>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = oneshot::channel();
        let (protocol_tx, protocol_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut path_tx = Some(path_tx);
            let mut protocol_tx = Some(protocol_tx);
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &WsRequest, mut response: WsResponse| {
                    if let Some(sender) = path_tx.take() {
                        let target = request
                            .uri()
                            .path_and_query()
                            .map(|value| value.as_str())
                            .unwrap_or("/")
                            .to_string();
                        let _ = sender.send(target);
                    }
                    if let Some(sender) = protocol_tx.take() {
                        let offered = request
                            .headers()
                            .get(header::SEC_WEBSOCKET_PROTOCOL)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        let _ = sender.send(offered);
                    }
                    // Accept the client's sub-protocol and answer the offer.
                    if request
                        .headers()
                        .get(header::SEC_WEBSOCKET_PROTOCOL)
                        .is_some()
                    {
                        response.headers_mut().insert(
                            header::SEC_WEBSOCKET_PROTOCOL,
                            HeaderValue::from_static("xpra-test"),
                        );
                    }
                    Ok(response)
                },
            )
            .await
            .unwrap();
            // The real Xpra server writes its hello very soon after the
            // handshake; send a frame before the relay has seen any client
            // frame so a lost early packet shows up in this test.
            if send_welcome {
                websocket
                    .send(WsMessage::Binary(vec![0x7f, 0x00, 0x42].into()))
                    .await
                    .unwrap();
            }
            while let Some(Ok(message)) = websocket.next().await {
                let reply = match message {
                    WsMessage::Text(text) => WsMessage::Text(text),
                    WsMessage::Binary(bytes) => WsMessage::Binary(bytes),
                    WsMessage::Ping(bytes) => WsMessage::Ping(bytes),
                    WsMessage::Pong(bytes) => WsMessage::Pong(bytes),
                    WsMessage::Close(frame) => {
                        let _ = websocket.send(WsMessage::Close(frame)).await;
                        return;
                    }
                    other => other,
                };
                if websocket.send(reply).await.is_err() {
                    return;
                }
            }
        });
        (port, path_rx, protocol_rx)
    }

    /// A test HTTP server with the native attach route wired exactly like the
    /// post-validation tail of `handle`: strip the token prefix, check the
    /// token/session binding, handshake the loopback Xpra upstream, consume the
    /// token once, then relay transparently.
    async fn spawn_native_bridge(
        upstream_port: u16,
        broker: Arc<NativeBroker>,
        session: Arc<NativeSession>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/linux-apps/native/{*rest}",
            get(
                move |upgrade: WebSocketUpgrade, uri: Uri, headers: HeaderMap| {
                    let broker = broker.clone();
                    let session = session.clone();
                    async move {
                        let path = uri.path().to_string();
                        let (token, rest) = token_and_rest(&path).expect("attach path");
                        if !valid_token(token) {
                            return unauthorized();
                        }
                        let grant = match broker.peek(token) {
                            TokenLookup::Valid(grant) => grant,
                            other => panic!("the test token must be valid, got {other:?}"),
                        };
                        if !grant_matches_session(&grant, &session) {
                            return unauthorized();
                        }
                        let target = upstream_ws_target(rest, uri.query());
                        let protocols = headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();
                        let Some((upstream, selected)) =
                            connect_upstream(upstream_port, &target, protocols).await
                        else {
                            return unauthorized();
                        };
                        if !broker.consume(token) {
                            return unauthorized();
                        }
                        let upgrade = match selected {
                            Some(protocol) => upgrade.protocols([protocol]),
                            None => upgrade,
                        };
                        let session = session.clone();
                        upgrade.on_upgrade(move |socket| async move {
                            let (to_app, to_client) =
                                relay_halves_traced(socket, upstream, session.clone(), false);
                            relay_until(to_app, to_client, session, std::future::pending::<()>())
                                .await;
                        })
                    }
                },
            ),
        );
        let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(server, app).await;
        });
        (address, serving)
    }

    #[tokio::test]
    async fn connect_upstream_forwards_the_offered_subprotocol() {
        let (port, path_rx, protocol_rx) = mock_xpra_upstream(true).await;
        let (upstream, selected) = connect_upstream(
            port,
            "/js/Client.js?sharing=yes",
            Some(HeaderValue::from_static("xpra-test,other")),
        )
        .await
        .expect("upstream handshake");
        assert_eq!(
            path_rx.await.unwrap(),
            "/js/Client.js?sharing=yes",
            "the loopback target keeps the remainder and query"
        );
        assert_eq!(
            protocol_rx.await.unwrap(),
            Some("xpra-test,other".to_string()),
            "the client's sub-protocol offer reaches Xpra"
        );
        assert_eq!(selected.as_deref(), Some("xpra-test"));
        drop(upstream);
    }

    #[tokio::test]
    async fn connect_upstream_reports_an_unreachable_xpra() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(
            connect_upstream(port, "/", None).await.is_none(),
            "a closed loopback port is a clean failure"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_native_relay_is_transparent_and_preserves_path_query_and_subprotocol() {
        let (upstream_port, path_rx, protocol_rx) = mock_xpra_upstream(true).await;
        let broker = Arc::new(NativeBroker::new());
        let session = NativeSession::for_test("e".repeat(32), 1);
        session.set_upstream_port_for_test(Some(upstream_port));
        let issued = broker.issue(1, "auth-session", session.key());
        let token = issued.token.clone();

        // The bridge performs exactly the steps of `handle` after the token
        // and session checks: strip the prefix, handshake Xpra, consume, relay.
        let (address, serving) =
            spawn_native_bridge(upstream_port, broker.clone(), session.clone()).await;

        let mut request = format!("ws://{address}{STREAM_PATH_PREFIX}{token}/chat/room?x=1&y=2")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("xpra-test"),
        );
        let (mut client, answer) = tokio_tungstenite::connect_async(request)
            .await
            .expect("native client attach");
        assert_eq!(
            answer
                .headers()
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .and_then(|value| value.to_str().ok()),
            Some("xpra-test"),
            "the upstream sub-protocol decision reaches the client"
        );
        assert_eq!(
            path_rx.await.unwrap(),
            "/chat/room?x=1&y=2",
            "the relay strips its token prefix and forwards rest+query to Xpra"
        );
        assert_eq!(
            protocol_rx.await.unwrap(),
            Some("xpra-test".to_string()),
            "the client's sub-protocol offer is forwarded to Xpra"
        );

        // The Xpra native client sends its ~5 KiB hello immediately as soon
        // as the 101 arrives (masked: RFC 6455; XPRA_WEBSOCKET_MASK=1). Send it
        // before reading anything and require the immediate upstream frame AND
        // the echo to arrive byte for byte.
        let hello_bytes: Vec<u8> = (0..5240u32).map(|i| (i % 251) as u8).collect();
        client
            .send(WsMessage::Binary(hello_bytes.clone().into()))
            .await
            .expect("send hello");
        let mut seen_welcome = false;
        let mut seen_echo = false;
        for _ in 0..2 {
            let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
                .await
                .expect("a relayed frame")
                .expect("a stream frame")
                .expect("a valid frame");
            match frame {
                WsMessage::Binary(bytes) if bytes.as_ref() == [0x7f, 0x00, 0x42] => {
                    assert!(!seen_welcome, "duplicate welcome frame");
                    seen_welcome = true;
                }
                WsMessage::Binary(bytes) => {
                    assert_eq!(
                        bytes.as_ref(),
                        hello_bytes.as_slice(),
                        "the 5 KiB hello must be relayed byte for byte"
                    );
                    assert!(!seen_echo, "duplicate echo");
                    seen_echo = true;
                }
                other => panic!("unexpected relayed frame: {other:?}"),
            }
        }
        assert!(
            seen_welcome,
            "the immediate upstream frame must reach the client"
        );
        assert!(seen_echo, "the 5 KiB client hello must be relayed upstream");

        // Single use: a second attach with the same token is refused.
        let second =
            tokio_tungstenite::connect_async(format!("ws://{address}{STREAM_PATH_PREFIX}{token}"))
                .await;
        assert!(second.is_err(), "a consumed token cannot attach again");

        assert_eq!(session.viewers_for_test(), 1, "the relay counts the viewer");
        drop(client);
        for _ in 0..40 {
            if session.viewers_for_test() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(session.viewers_for_test(), 0, "the viewer slot is released");
        serving.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn frames_from_a_client_keep_the_xpra_session_active() {
        // The native client frames count as activity exactly like browser
        // viewer frames (same relay code), so an attached desktop does not let
        // the session idle out.
        let (upstream_port, _path_rx, _protocol_rx) = mock_xpra_upstream(true).await;
        let session = NativeSession::for_test("f".repeat(32), 1);
        session.set_upstream_port_for_test(Some(upstream_port));
        let before = session.last_input_for_test();
        let bridge_session = session.clone();
        let app = Router::new().route(
            "/bridge",
            get(move |upgrade: WebSocketUpgrade| {
                let session = bridge_session.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        let stream = TokioTcpStream::connect(("127.0.0.1", upstream_port))
                            .await
                            .expect("upstream connect");
                        let (upstream, _) = tokio_tungstenite::client_async(
                            format!("ws://127.0.0.1:{upstream_port}/"),
                            stream,
                        )
                        .await
                        .expect("upstream handshake");
                        let (to_app, to_client) =
                            relay_halves_traced(socket, upstream, session.clone(), false);
                        relay_until(to_app, to_client, session, std::future::pending::<()>()).await;
                    })
                }
            }),
        );
        let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(server, app).await;
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/bridge"))
            .await
            .expect("connect");
        // The mock upstream sends one frame first; consume it.
        let _ = tokio::time::timeout(Duration::from_secs(5), client.next()).await;
        client
            .send(WsMessage::Binary(vec![1, 2, 3].into()))
            .await
            .expect("input");
        let _ = tokio::time::timeout(Duration::from_secs(5), client.next()).await;
        assert!(
            session.last_input_for_test() > before,
            "native client frames count as session activity"
        );
        drop(client);
        serving.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unmasked_client_frame_is_closed_strictly() {
        // Documented behaviour pinned by a test: the official Xpra client
        // defaults to sending UNMASKED WebSocket frames; RFC 6455 forbids that
        // from a client and axum/tungstenite closes such a connection with a
        // protocol error (the observed symptom was "0 packets received" on the
        // client). Xpra must be launched with XPRA_WEBSOCKET_MASK=1 (see
        // docs/NATIVE-BROKER.md); the relay never accepts masked-less frames.
        let (upstream_port, _path_rx, _protocol_rx) = mock_xpra_upstream(false).await;
        let broker = Arc::new(NativeBroker::new());
        let session = NativeSession::for_test("a".repeat(32), 1);
        session.set_upstream_port_for_test(Some(upstream_port));
        let issued = broker.issue(1, "auth-session", session.key());
        let (address, serving) = spawn_native_bridge(upstream_port, broker, session).await;

        let mut tcp = TokioTcpStream::connect(address).await.unwrap();
        let request = format!(
            "GET {STREAM_PATH_PREFIX}{token}/ HTTP/1.1\r\nHost: {address}\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            token = issued.token,
        );
        tcp.write_all(request.as_bytes()).await.unwrap();
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let read = tcp.read(&mut chunk).await.unwrap();
            assert!(read > 0, "server closed during the handshake");
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(buffer.len() <= 16 * 1024, "handshake response too large");
        }
        let head = String::from_utf8_lossy(&buffer);
        assert!(head.starts_with("HTTP/1.1 101"), "handshake: {head}");

        // Binary frame (FIN + opcode 2) with the MASK bit deliberately clear.
        let payload = [0x55u8, 0x00, 0x01, 0x02, 0x03];
        let mut frame = vec![0x82, payload.len() as u8];
        frame.extend_from_slice(&payload);
        tcp.write_all(&frame).await.unwrap();

        // The strict relay must reject it: a Close frame (opcode 8) or EOF.
        let mut answer = Vec::new();
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let read = tcp.read(&mut chunk).await;
                match read {
                    Ok(0) => return Ok::<bool, ()>(true),
                    Ok(read) => {
                        answer.extend_from_slice(&chunk[..read]);
                        if answer.len() >= 2 {
                            return Ok::<bool, ()>(answer[0] & 0x0f == 0x8);
                        }
                    }
                    Err(_) => return Ok::<bool, ()>(true),
                }
            }
        })
        .await;
        match outcome {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => panic!("the relay forwarded a frame instead of closing: {answer:?}"),
            Ok(Err(())) => panic!("reading the relay answer was interrupted"),
            Err(_) => panic!("the relay did not close an unmasked-frame connection"),
        }
        serving.abort();
    }
}
