//! Authentication: roles, the authenticated-user extractor, cookies, client IP
//! resolution, password hashing, sessions, CSRF tokens and rate limiting.

pub mod csrf;
pub mod middleware;
pub mod password;
pub mod rate_limit;
pub mod session;

use std::net::IpAddr;

use axum::{
    extract::FromRequestParts,
    http::{header, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::AppError, state::AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::User => "user",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admin" => Some(Self::Admin),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Authenticated principal attached to request extensions by the middleware.
#[derive(Debug, Clone, Serialize)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub session_id: String,
    pub quota_bytes: i64,
    pub must_change_password: bool,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn require_admin(&self) -> Result<(), AppError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(AppError::Forbidden("administrator role required"))
        }
    }
}

/// Client IP resolved from the peer address and trusted proxy headers.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

impl ClientIp {
    pub fn as_string(&self) -> String {
        self.0.to_string()
    }
}

impl sqlx::Type<sqlx::Sqlite> for Role {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <String as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for Role {
    fn decode(
        value: sqlx::sqlite::SqliteValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let s = <&str as sqlx::Decode<sqlx::Sqlite>>::decode(value)?;
        Role::parse(s).ok_or_else(|| format!("invalid role: {s}").into())
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for Role {
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        <&str as sqlx::Encode<sqlx::Sqlite>>::encode(self.as_str(), buf)
    }
}

/// Extracts the TCP peer address if the server was started with connect info.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub std::net::SocketAddr);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::extract::ConnectInfo;
        let addr = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0)
            .unwrap_or_else(|| {
                std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0)
            });
        Ok(PeerAddr(addr))
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or(AppError::Unauthorized("authentication required"))
    }
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ClientIp>()
            .copied()
            .unwrap_or(ClientIp(std::net::IpAddr::V4(
                std::net::Ipv4Addr::LOCALHOST,
            ))))
    }
}

/// Admin-only extractor.
#[derive(Debug, Clone)]
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        user.require_admin()?;
        Ok(AdminUser(user))
    }
}

/// Parse one cookie value from the `Cookie` header.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Build a `Set-Cookie` value for the session token.
pub fn session_cookie(token: &str, ttl_hours: i64, secure: bool) -> String {
    let max_age = (ttl_hours.max(1) * 3600).to_string();
    let mut value = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        session::SESSION_COOKIE,
        token,
        max_age
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// Cookies that clear the session.
pub fn clear_cookies(secure: bool) -> Vec<String> {
    let mut out = Vec::new();
    for name in [session::SESSION_COOKIE, session::SESSION_ID_COOKIE] {
        let mut v = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
        if secure {
            v.push_str("; Secure");
        }
        out.push(v);
    }
    out
}

pub fn cookie_header(value: String) -> HeaderValue {
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Resolve the real client IP, honouring `CF-Connecting-IP` and
/// `X-Forwarded-For` only when the immediate peer is a configured trusted
/// proxy. Otherwise the peer address wins, preventing spoofed headers.
pub fn client_ip(headers: &HeaderMap, peer: IpAddr, cfg: &Config) -> IpAddr {
    let peer_trusted = cfg.trusted_proxies.iter().any(|net| net.contains(&peer));
    if !peer_trusted || !cfg.trust_forwarded_headers {
        return peer;
    }
    // Cloudflare Tunnel sets CF-Connecting-IP; trust it before XFF.
    if let Some(cf) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(ip) = cf.trim().parse::<IpAddr>() {
            return ip;
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        for candidate in xff.split(',').rev() {
            let candidate = candidate.trim();
            if candidate.is_empty() {
                continue;
            }
            if let Ok(ip) = candidate.parse::<IpAddr>() {
                let is_trusted = cfg.trusted_proxies.iter().any(|net| net.contains(&ip));
                if !is_trusted {
                    return ip;
                }
            }
        }
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use ipnet::IpNet;
    use std::str::FromStr;

    fn cfg_with_proxies(nets: &[&str]) -> Config {
        let trusted = nets
            .iter()
            .map(|n| IpNet::from_str(n).unwrap())
            .collect::<Vec<_>>();
        Config {
            bind: "127.0.0.1:8720".parse().unwrap(),
            public_url: None,
            data_dir: "/tmp/rast".into(),
            storage_dir: "/tmp/rast-data".into(),
            trusted_proxies: trusted,
            session_ttl_hours: 168,
            monitor_interval_secs: 5,
            max_upload_bytes: 1024,
            log_level: "info".into(),
            db_max_connections: 2,
            worker_socket: "/tmp/worker.sock".into(),
            docker_enabled: false,
            default_quota_bytes: 0,
            secure_cookies: false,
            job_concurrency: 1,
            setup_token_required: true,
            trust_forwarded_headers: true,
            packages_catalog_url: crate::packages::catalog::OFFICIAL_CATALOG_URL.into(),
            packages_catalog_keys: Vec::new(),
            packages_check_interval_minutes: 0,
            packages_github_token: None,
            packages_shell_path: false,
            packages_system_wide: false,
            native_enabled: false,
            native_helper_socket: "/run/rast/native.sock".into(),
            linux_xpra_bin: "/usr/bin/xpra".into(),
            linux_uid_base: crate::config::DEFAULT_LINUX_UID_BASE,
            linux_idle_minutes: crate::config::DEFAULT_LINUX_IDLE_MINUTES,
            linux_session_memory_mb: crate::config::DEFAULT_LINUX_SESSION_MEMORY_MB,
            linux_session_cpus: 2,
            linux_session_tasks: 2048,
            linux_app_memory_mb: crate::config::DEFAULT_LINUX_APP_MEMORY_MB,
            linux_app_cpus: crate::config::DEFAULT_LINUX_APP_CPUS,
            linux_app_tasks: crate::config::DEFAULT_LINUX_APP_TASKS,
            linux_admission_reserve_mb: crate::config::DEFAULT_LINUX_ADMISSION_RESERVE_MB,
            loaded_from: None,
        }
    }

    #[test]
    fn untrusted_peer_cannot_spoof_forwarded_ip() {
        let cfg = cfg_with_proxies(&["127.0.0.1/32"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let peer: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(client_ip(&headers, peer, &cfg), peer);
    }

    #[test]
    fn trusted_peer_honours_cf_and_xff() {
        let cfg = cfg_with_proxies(&["127.0.0.1/32"]);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            client_ip(&headers, peer, &cfg),
            "5.6.7.8".parse::<IpAddr>().unwrap()
        );
        headers.insert("cf-connecting-ip", HeaderValue::from_static("9.9.9.9"));
        assert_eq!(
            client_ip(&headers, peer, &cfg),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn cookie_parse_and_render() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; rast_session=token123; b=2"),
        );
        assert_eq!(
            cookie_value(&headers, "rast_session").as_deref(),
            Some("token123")
        );
        assert!(cookie_value(&headers, "missing").is_none());
        let c = session_cookie("tok", 24, true);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("Secure"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=86400"));
    }
}
