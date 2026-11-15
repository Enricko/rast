//! HTTPS downloads with an origin allowlist checked on every redirect hop.
//!
//! Redirects are never followed automatically: each `Location` must be an
//! `https://` URL whose host is in the policy (for example `github.com` →
//! `release-assets.githubusercontent.com`). Bodies are size-limited, streamed
//! to disk with a running SHA-256, and cancellable. All functions block; call
//! them from `spawn_blocking`.

use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use sha2::{Digest, Sha256};

const MAX_HOPS: usize = 6;
const USER_AGENT: &str = concat!("rast/", env!("CARGO_PKG_VERSION"), " (package-center)");

/// Where downloads may come from.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    /// Exact host names (lowercase) allowed for every hop.
    pub hosts: Vec<String>,
    /// Allow `file://` URLs (administrator-configured local catalog mirrors).
    pub allow_file: bool,
    /// Allow every `https://` host on the public internet (a link an
    /// administrator typed). Hosts that resolve to a private, loopback or
    /// link-local address are refused at every hop.
    pub any_public: bool,
}

impl OriginPolicy {
    pub fn new<I: IntoIterator<Item = S>, S: Into<String>>(hosts: I) -> Self {
        Self {
            hosts: hosts
                .into_iter()
                .map(|h| h.into().to_ascii_lowercase())
                .collect(),
            allow_file: false,
            any_public: false,
        }
    }

    /// Any public `https://` host (see [`OriginPolicy::any_public`]).
    pub fn any_public() -> Self {
        Self {
            any_public: true,
            ..Self::default()
        }
    }

    pub fn allows(&self, url: &str) -> Result<(), String> {
        if let Some(path) = url.strip_prefix("file://") {
            return if self.allow_file && path.starts_with('/') {
                Ok(())
            } else {
                Err(format!("{url} is not an allowed download location"))
            };
        }
        let host = https_host(url)?;
        if self.any_public {
            return public_host(&host);
        }
        if self.hosts.iter().any(|h| h == &host) {
            Ok(())
        } else {
            Err(format!(
                "download origin {host} is not allowed (allowed: {})",
                self.hosts.join(", ")
            ))
        }
    }
}

/// Refuse hosts that are, or resolve to, an address inside this server or its
/// network.
fn public_host(host: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let refused = || format!("{host} is not a public internet address");
    let addrs: Vec<_> = (host, 443)
        .to_socket_addrs()
        .map_err(|_| format!("{host} could not be found"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} could not be found"));
    }
    if addrs.iter().all(|a| is_public_ip(a.ip())) {
        Ok(())
    } else {
        Err(refused())
    }
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 240)
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(std::net::IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Host of an `https://` URL; rejects other schemes, user info and odd ports.
pub fn https_host(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("{url} is not an https:// URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("{url} has an invalid host"));
    }
    let host = match authority.rsplit_once(':') {
        Some((host, "443")) => host,
        Some(_) => return Err(format!("{url} uses a non-standard port")),
        None => authority,
    };
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("{url} has an invalid host"));
    }
    Ok(host.to_ascii_lowercase())
}

fn resolve_location(current: &str, location: &str) -> Result<String, String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    if location.starts_with('/') && !location.starts_with("//") {
        let host = https_host(current)?;
        return Ok(format!("https://{host}{location}"));
    }
    Err(format!("unsupported redirect target {location:?}"))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(20)))
        .user_agent(USER_AGENT)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .https_only(true)
        .build()
        .new_agent()
}

/// Final response after validated redirects.
struct Resolved {
    response: ureq::http::Response<ureq::Body>,
    url: String,
}

fn open(
    url: &str,
    policy: &OriginPolicy,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<Resolved, String> {
    let agent = agent(timeout);
    let mut current = url.to_string();
    for _ in 0..MAX_HOPS {
        policy.allows(&current)?;
        let mut request = agent.get(&current);
        for (name, value) in headers {
            // Credentials (GitHub token) only go to the API host itself.
            if name.eq_ignore_ascii_case("authorization")
                && https_host(&current).ok().as_deref() != Some("api.github.com")
            {
                continue;
            }
            request = request.header(*name, *value);
        }
        let response = request
            .call()
            .map_err(|e| format!("request to {} failed: {e}", short(&current)))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("redirect from {} without a location", short(&current)))?;
            let next = resolve_location(&current, location)?;
            policy
                .allows(&next)
                .map_err(|e| format!("refused redirect from {}: {e}", short(&current)))?;
            current = next;
            continue;
        }
        if status == 404 {
            return Err(format!("{} was not found (404)", short(&current)));
        }
        if !(200..300).contains(&status) {
            return Err(format!("{} answered HTTP {status}", short(&current)));
        }
        return Ok(Resolved {
            response,
            url: current,
        });
    }
    Err(format!("too many redirects for {}", short(url)))
}

fn short(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    if base.len() > 120 {
        format!("{}…", &base[..120])
    } else {
        base.to_string()
    }
}

/// Fetch a small document (catalog, manifest, API JSON) into memory.
pub fn fetch_bytes(
    url: &str,
    policy: &OriginPolicy,
    headers: &[(&str, &str)],
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        policy.allows(url)?;
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {url}: {e}"))?;
        if meta.len() > max_bytes {
            return Err(format!("{url} is larger than {max_bytes} bytes"));
        }
        return std::fs::read(path).map_err(|e| format!("cannot read {url}: {e}"));
    }
    let resolved = open(url, policy, headers, Duration::from_secs(60))?;
    let mut body = resolved.response.into_body();
    let mut out = Vec::new();
    body.with_config()
        .limit(max_bytes)
        .reader()
        .read_to_end(&mut out)
        .map_err(|e| format!("cannot read {}: {e}", short(&resolved.url)))?;
    Ok(out)
}

/// Download progress callback: (bytes so far, expected total).
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// Stream an artifact to `target`, enforcing the expected size and SHA-256.
/// The file is removed again when anything does not match.
pub fn download_verified(
    url: &str,
    policy: &OriginPolicy,
    target: &Path,
    expected_size: u64,
    expected_sha256: &str,
    progress: Progress<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let result = download_inner(
        url,
        policy,
        target,
        expected_size,
        expected_sha256,
        progress,
        cancelled,
    );
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

fn download_inner(
    url: &str,
    policy: &OriginPolicy,
    target: &Path,
    expected_size: u64,
    expected_sha256: &str,
    progress: Progress<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    // Local mirrors (file://) are only reachable when the policy allows them.
    let (length, mut reader): (Option<u64>, Box<dyn Read>) =
        if let Some(path) = url.strip_prefix("file://") {
            policy.allows(url)?;
            let file = std::fs::File::open(path).map_err(|e| format!("cannot read {url}: {e}"))?;
            let length = file.metadata().ok().map(|m| m.len());
            (length, Box::new(file.take(expected_size + 1)))
        } else {
            let resolved = open(url, policy, &[], Duration::from_secs(30 * 60))?;
            let length = resolved
                .response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let body = resolved.response.into_body();
            (
                length,
                Box::new(body.into_with_config().limit(expected_size + 1).reader()),
            )
        };
    if let Some(length) = length {
        if length != expected_size {
            return Err(format!(
                "the server reports {length} bytes, the manifest expects {expected_size}"
            ));
        }
    }
    let mut file = std::fs::File::create(target)
        .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        if cancelled() {
            return Err("cancelled".into());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > expected_size {
            return Err(format!(
                "artifact is larger than the {expected_size} bytes the manifest declares"
            ));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write download: {e}"))?;
        progress(done, expected_size);
    }
    file.sync_all().ok();
    if done != expected_size {
        return Err(format!(
            "downloaded {done} bytes, the manifest declares {expected_size}"
        ));
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        return Err(format!(
            "SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        ));
    }
    Ok(())
}

/// Fetch at most `max_bytes` from the start of a document (for headers of
/// large files, such as a `.zsync` file's text header).
pub fn fetch_prefix(url: &str, policy: &OriginPolicy, max_bytes: u64) -> Result<Vec<u8>, String> {
    let resolved = open(url, policy, &[], Duration::from_secs(60))?;
    let mut out = Vec::new();
    resolved
        .response
        .into_body()
        .into_with_config()
        .limit(u64::MAX)
        .reader()
        .take(max_bytes)
        .read_to_end(&mut out)
        .map_err(|e| format!("cannot read {}: {e}", short(&resolved.url)))?;
    Ok(out)
}

/// Digests of a finished download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub size: u64,
    pub sha256: String,
    pub sha1: String,
}

/// Stream a download to `target` (at most `max_bytes`, exactly
/// `expected_size` when known) and return its size and digests; the caller
/// compares them with what the source published. The file is removed again
/// on failure.
pub fn download_hashed(
    url: &str,
    policy: &OriginPolicy,
    target: &Path,
    expected_size: Option<u64>,
    max_bytes: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Downloaded, String> {
    let result = (|| {
        use sha1::Digest as _;
        let resolved = open(url, policy, &[], Duration::from_secs(60 * 60))?;
        let limit = expected_size.unwrap_or(max_bytes).min(max_bytes);
        let mut reader = resolved
            .response
            .into_body()
            .into_with_config()
            .limit(limit + 1)
            .reader();
        let mut file = std::fs::File::create(target)
            .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
        let mut sha256 = Sha256::new();
        let mut sha1 = sha1::Sha1::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done: u64 = 0;
        loop {
            if cancelled() {
                return Err("cancelled".to_string());
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > limit {
                return Err(format!("the download is larger than {limit} bytes"));
            }
            sha256.update(&buf[..n]);
            sha1.update(&buf[..n]);
            file.write_all(&buf[..n])
                .map_err(|e| format!("cannot write download: {e}"))?;
        }
        file.sync_all().ok();
        if let Some(expected) = expected_size {
            if done != expected {
                return Err(format!("downloaded {done} bytes, expected {expected}"));
            }
        }
        Ok(Downloaded {
            size: done,
            sha256: hex::encode(sha256.finalize()),
            sha1: hex::encode(sha1.finalize()),
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_public_refuses_addresses_inside_the_network() {
        let policy = OriginPolicy::any_public();
        for url in [
            "https://127.0.0.1/app.deb",
            "https://localhost/app.deb",
            "https://10.0.0.5/app.deb",
            "https://192.168.1.1/app.deb",
            "https://169.254.169.254/latest",
            "http://example.com/app.deb",
            "https://example.com:8443/app.deb",
        ] {
            assert!(policy.allows(url).is_err(), "{url}");
        }
        for ip in ["8.8.8.8", "2606:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn host_parsing_rejects_tricks() {
        assert_eq!(https_host("https://github.com/a/b").unwrap(), "github.com");
        assert_eq!(
            https_host("https://GitHub.com:443/x").unwrap(),
            "github.com"
        );
        for bad in [
            "http://github.com/a",
            "https://user@github.com/a",
            "https://github.com:8443/a",
            "https:///a",
            "ftp://github.com",
            "https://git hub.com/",
        ] {
            assert!(https_host(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn policy_checks_each_hop() {
        let policy = OriginPolicy::new(["github.com", "release-assets.githubusercontent.com"]);
        assert!(policy
            .allows("https://github.com/o/r/releases/download/t/a")
            .is_ok());
        assert!(policy
            .allows("https://release-assets.githubusercontent.com/x")
            .is_ok());
        assert!(policy.allows("https://evil.example/a").is_err());
        assert!(policy.allows("https://github.com.evil.example/a").is_err());
        assert!(policy.allows("file:///etc/passwd").is_err());
        assert_eq!(
            resolve_location("https://github.com/a", "/b/c").unwrap(),
            "https://github.com/b/c"
        );
        assert!(resolve_location("https://github.com/a", "//evil/x").is_err());
        assert!(resolve_location("https://github.com/a", "javascript:x").is_err());
    }
}
