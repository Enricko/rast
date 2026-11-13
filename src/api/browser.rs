//! Server-side page proxy for the Browser app package.
//!
//! The Browser app is a normal web app package, but a sandboxed page cannot
//! fetch other origins. This endpoint fetches one HTML page for it with a
//! deliberately narrow policy:
//!
//! * HTTPS only, standard port only, no credentials in the URL;
//! * the hostname must resolve exclusively to public addresses (a custom DNS
//!   resolver pins that resolution for the connection, blocking rebinding);
//! * redirects are followed manually and re-validated against the same rule;
//! * only `text/html` documents up to 4 MiB, fetched without cookies;
//! * the document is returned to the signed-in account with its relative URLs
//!   anchored at its own origin, a link-interception script for the app window
//!   and a CSP that keeps it sandboxed away from RAST's origin.

use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    time::Duration,
};

use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use ureq::{
    config::Config as UreqConfig,
    http::Uri,
    unversioned::{
        resolver::{ResolvedSocketAddrs, Resolver},
        transport::{DefaultConnector, NextTimeout},
    },
};

use crate::{access, auth::AuthUser, error::AppError, state::AppState};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(20);
const USER_AGENT: &str = "RAST-Browser/1 (+https://github.com/Enricko/rast)";

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/browser/page", get(page))
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub url: String,
}

/// GET /api/browser/page?url=https://…
pub async fn page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> Result<Response, AppError> {
    access::app_surface(&state.db, &state, &user, "packages").await?;
    let requested = query.url;
    if requested.len() > 2048 {
        return Err(AppError::bad_request("URL is too long"));
    }
    let result = tokio::task::spawn_blocking(move || fetch_page(&requested))
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
    let page = result.map_err(AppError::bad_request)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::X_FRAME_OPTIONS, "SAMEORIGIN".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src https: data: blob:; script-src https: 'unsafe-inline' 'unsafe-eval'; \
                 style-src https: 'unsafe-inline'; img-src https: data: blob:; \
                 font-src https: data:; media-src https:; connect-src https:; \
                 frame-src 'none'; object-src 'none'; base-uri https:; form-action 'none'; \
                 frame-ancestors 'self'"
                    .to_string(),
            ),
        ],
        page.html,
    )
        .into_response())
}

struct FetchedPage {
    html: String,
}

fn fetch_page(input: &str) -> Result<FetchedPage, String> {
    let mut current = parse_https_url(input)?;
    let agent = build_agent()?;
    for _ in 0..=MAX_REDIRECTS {
        let response = agent
            .get(current.to_string())
            .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
            .header("User-Agent", USER_AGENT)
            .call();
        let mut response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => {
                return Err(format!("The page answered HTTP {code}."));
            }
            Err(error) => return Err(format!("Cannot load the page: {error}")),
        };
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or("The page redirected without a location.")?;
            current = resolve_redirect(&current, location)?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("The page answered HTTP {}.", status.as_u16()));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.starts_with("text/html") && !content_type.starts_with("application/xhtml")
        {
            return Err("This page is not an HTML document.".into());
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .with_config()
            .limit(MAX_BYTES)
            .reader()
            .read_to_end(&mut body)
            .map_err(|error| format!("Cannot read the page: {error}"))?;
        let html = String::from_utf8_lossy(&body);
        let document = inject_for_browser(&html, &current)?;
        return Ok(FetchedPage { html: document });
    }
    Err("Too many redirects.".into())
}

fn build_agent() -> Result<ureq::Agent, String> {
    let config = UreqConfig::builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(Duration::from_secs(8)))
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(true)
        .build();
    Ok(ureq::Agent::with_parts(
        config,
        DefaultConnector::default(),
        PublicResolver,
    ))
}

/// Only HTTPS URLs on port 443, without credentials.
fn parse_https_url(input: &str) -> Result<Uri, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a web address.".into());
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let uri: Uri = candidate
        .parse()
        .map_err(|_| "This is not a valid web address.".to_string())?;
    if uri.scheme_str() != Some("https") {
        return Err("Only https:// addresses can be opened.".into());
    }
    let authority = uri.authority().ok_or("The address has no host.")?;
    if authority.as_str().contains('@') {
        return Err("Addresses with credentials are not supported.".into());
    }
    let host = authority.host();
    if host.is_empty() || host.len() > 255 || host.contains(['\r', '\n', ' ']) {
        return Err("The address has an invalid host.".into());
    }
    if let Some(port) = uri.port_u16() {
        if port != 443 {
            return Err("Only the standard HTTPS port can be opened.".into());
        }
    }
    Ok(uri)
}

fn resolve_redirect(base: &Uri, location: &str) -> Result<Uri, String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("The page redirected to an empty address.".into());
    }
    let absolute = if location.contains("://") {
        location.to_string()
    } else if location.starts_with("//") {
        format!("https:{location}")
    } else if let Some(path) = location.strip_prefix('/') {
        format!(
            "https://{}/{path}",
            base.authority().map(|a| a.as_str()).unwrap_or_default()
        )
    } else {
        let path = base.path();
        let directory = &path[..path.rfind('/').map(|index| index + 1).unwrap_or(0)];
        format!(
            "https://{}{}{}",
            base.authority().map(|a| a.as_str()).unwrap_or_default(),
            directory,
            location
        )
    };
    parse_https_url(&absolute)
}

fn inject_for_browser(html: &str, final_url: &Uri) -> Result<String, String> {
    let lower = html.to_ascii_lowercase();
    let injection = format!(
        "<base href=\"{}\"><script>window.__RAST_BROWSER__=true;\
         document.addEventListener('click',function(event){{var link=event.target.closest&&event.target.closest('a');\
         if(!link||!link.href)return;var href=link.href;if(!/^https:/i.test(href))return;\
         event.preventDefault();parent.postMessage({{type:'rast-browser-navigate',url:href}},'*');}},true);\
         document.addEventListener('submit',function(event){{event.preventDefault();}},true);</script>",
        escape_attribute(&final_url.to_string())
    );
    let mut output = String::with_capacity(html.len() + 256);
    if let Some(index) = lower.find("<head") {
        if let Some(end) = lower[index..].find('>') {
            let insert = index + end + 1;
            output.push_str(&html[..insert]);
            output.push_str(&injection);
            output.push_str(&html[insert..]);
            return Ok(output);
        }
    }
    if let Some(index) = lower.find("<html") {
        if let Some(end) = lower[index..].find('>') {
            let insert = index + end + 1;
            output.push_str(&html[..insert]);
            output.push_str(&injection);
            output.push_str(&html[insert..]);
            return Ok(output);
        }
    }
    Ok(format!("{injection}{html}"))
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_ipv4(mapped);
            }
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113))
}

/// Resolver that only ever returns public addresses. The connection then uses
/// exactly these addresses, so a hostname cannot resolve to a public address
/// for validation and to a private one for the request (DNS rebinding).
#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &UreqConfig,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().ok_or(ureq::Error::HostNotFound)?;
        let port = uri.port_u16().unwrap_or(443);
        let addresses: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|_| ureq::Error::HostNotFound)?
            .filter(|address| public_ip(address.ip()))
            .collect();
        if addresses.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut resolved = self.empty();
        for address in addresses.into_iter().take(8) {
            if resolved.try_push(address).is_err() {
                break;
            }
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_allowed() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn urls_are_restricted_to_https_on_the_default_port() {
        assert!(parse_https_url("example.com").is_ok());
        assert!(parse_https_url("https://example.com/path?q=1").is_ok());
        for bad in [
            "http://example.com",
            "ftp://example.com",
            "https://example.com:8080/",
            "https://user:pass@example.com/",
            "https:///no-host",
            "",
        ] {
            assert!(parse_https_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn relative_redirects_stay_on_the_same_origin() {
        let base = parse_https_url("https://example.com/a/b").unwrap();
        assert_eq!(
            resolve_redirect(&base, "/c").unwrap().to_string(),
            "https://example.com/c"
        );
        assert_eq!(
            resolve_redirect(&base, "d").unwrap().to_string(),
            "https://example.com/a/d"
        );
        assert_eq!(
            resolve_redirect(&base, "https://other.example/x")
                .unwrap()
                .to_string(),
            "https://other.example/x"
        );
    }

    #[test]
    fn html_gets_a_base_tag_and_link_interception() {
        let uri = parse_https_url("https://example.com/p").unwrap();
        let html = inject_for_browser(
            "<html><head><title>x</title></head><body>hi</body></html>",
            &uri,
        )
        .unwrap();
        assert!(html.contains("<base href=\"https://example.com/p\">"));
        assert!(html.contains("rast-browser-navigate"));
        let without_head = inject_for_browser("<body>hi</body>", &uri).unwrap();
        assert!(without_head.starts_with("<base href="));
    }
}
