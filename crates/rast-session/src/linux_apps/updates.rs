//! Official update sources of AppImages: the update information embedded in
//! the AppImage itself (`gh-releases-zsync|…` or `zsync|<url>`).
//!
//! `.deb` apps are checked by the Docker helper with the apt sources their
//! own package registered (see `linux-check`).

use std::{path::Path, sync::atomic::AtomicBool};

use serde_json::Value;

use crate::{
    docker::linux::MAX_PACKAGE_BYTES,
    packages::{
        catalog::GITHUB_DOWNLOAD_HOSTS,
        net::{self, OriginPolicy},
    },
};

/// Shown as the newest version when the source says "different file" but
/// not which version it is.
pub const NEWER_BUILD: &str = "newer build";
const GITHUB_API: &str = "https://api.github.com";
const MAX_API_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ZSYNC_HEADER: u64 = 16 * 1024;

/// The newest file an update source offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub version: Option<String>,
    /// Decided by the source itself (zsync compares SHA-1 digests).
    pub newer: Option<bool>,
    pub file_name: String,
    pub url: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub sha1: Option<String>,
    pub hosts: Vec<String>,
}

impl Latest {
    /// What to record as the newest version, given the installed one.
    pub fn version_label(&self, installed: Option<&str>) -> Option<String> {
        match self.newer {
            Some(false) => installed
                .map(str::to_string)
                .or_else(|| self.version.clone()),
            Some(true) => Some(
                self.version
                    .clone()
                    .filter(|v| installed.is_none_or(|i| is_newer(v, i)))
                    .unwrap_or_else(|| NEWER_BUILD.to_string()),
            ),
            None => self.version.clone(),
        }
    }
}

/// Is `latest` newer than `current`? Dotted numbers are compared as numbers
/// (`1.10` > `1.9`); anything else counts as newer when it differs.
pub fn is_newer(latest: &str, current: &str) -> bool {
    if latest == NEWER_BUILD {
        return true;
    }
    let numbers = |v: &str| -> Vec<u64> {
        v.trim_start_matches(['v', 'V'])
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(u64::MAX))
            .collect()
    };
    let (a, b) = (numbers(latest), numbers(current));
    if a.is_empty() || b.is_empty() {
        return latest != current;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Ask an AppImage's update source for its newest file. Blocks.
pub fn latest(update: &Value, installed_sha1: Option<&str>) -> Result<Latest, String> {
    let text = |key: &str| {
        update
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("the update source has no {key}"))
    };
    match update.get("kind").and_then(Value::as_str) {
        Some("github") => github_latest(
            &text("owner")?,
            &text("repo")?,
            &text("tag")?,
            &text("file")?,
        ),
        Some("zsync") => zsync_latest(&text("url")?, installed_sha1),
        _ => Err("this app has no update source RAST can use".into()),
    }
}

/// Download the newest file to `target`, checking the digests the source
/// published. Returns the file's SHA-1. Blocks.
pub fn download(latest: &Latest, target: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let policy = OriginPolicy::new(latest.hosts.clone());
    let done = net::download_hashed(
        &latest.url,
        &policy,
        target,
        latest.size,
        MAX_PACKAGE_BYTES,
        &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
    )?;
    let mismatch = |what: &str| {
        let _ = std::fs::remove_file(target);
        Err(format!(
            "the downloaded file does not match its published {what}"
        ))
    };
    if latest
        .sha256
        .as_deref()
        .is_some_and(|d| !d.eq_ignore_ascii_case(&done.sha256))
    {
        return mismatch("SHA-256");
    }
    if latest
        .sha1
        .as_deref()
        .is_some_and(|d| !d.eq_ignore_ascii_case(&done.sha1))
    {
        return mismatch("SHA-1");
    }
    Ok(done.sha1)
}

fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.starts_with(['-', '.'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `*` wildcard match (AppImage update information uses it for file names).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

fn github_latest(owner: &str, repo: &str, tag: &str, file: &str) -> Result<Latest, String> {
    if !valid_name(owner, 39) || !valid_name(repo, 100) {
        return Err("the update source names an invalid GitHub repository".into());
    }
    let url = match tag {
        "latest" => format!("{GITHUB_API}/repos/{owner}/{repo}/releases/latest"),
        tag if valid_name(tag, 100) => {
            format!("{GITHUB_API}/repos/{owner}/{repo}/releases/tags/{tag}")
        }
        _ => return Err("the update source names an invalid release".into()),
    };
    let body = net::fetch_bytes(
        &url,
        &OriginPolicy::new(["api.github.com"]),
        &[("accept", "application/vnd.github+json")],
        MAX_API_BYTES,
    )?;
    let release: Value =
        serde_json::from_slice(&body).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    pick_github_asset(&release, file)
}

/// The release asset matching the AppImage's file pattern.
pub fn pick_github_asset(release: &Value, file: &str) -> Result<Latest, String> {
    let pattern = file.strip_suffix(".zsync").unwrap_or(file);
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let asset = release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|asset| {
            asset
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| glob_match(pattern, name) && !name.ends_with(".zsync"))
        })
        .ok_or_else(|| format!("the newest release ({tag}) has no file matching {pattern}"))?;
    let name = asset
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://github.com/"))
        .ok_or("the release file has no download address")?
        .to_string();
    let version = super::version_from_file(&name).or_else(|| {
        let tag = tag.trim_start_matches(['v', 'V']);
        tag.starts_with(|c: char| c.is_ascii_digit())
            .then(|| tag.to_string())
    });
    let sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_string);
    Ok(Latest {
        version,
        newer: None,
        file_name: name,
        url,
        size: asset.get("size").and_then(Value::as_u64),
        sha256,
        sha1: None,
        hosts: GITHUB_DOWNLOAD_HOSTS
            .iter()
            .map(|h| h.to_string())
            .collect(),
    })
}

fn zsync_latest(url: &str, installed_sha1: Option<&str>) -> Result<Latest, String> {
    let host = net::https_host(url)?;
    let mut hosts: Vec<String> = GITHUB_DOWNLOAD_HOSTS
        .iter()
        .map(|h| h.to_string())
        .collect();
    hosts.push(host);
    let head = net::fetch_prefix(url, &OriginPolicy::new(hosts.clone()), MAX_ZSYNC_HEADER)?;
    let latest = parse_zsync_header(&head, url, installed_sha1)?;
    Ok(Latest { hosts, ..latest })
}

/// The text header of a `.zsync` file (until the first empty line).
pub fn parse_zsync_header(
    raw: &[u8],
    zsync_url: &str,
    installed_sha1: Option<&str>,
) -> Result<Latest, String> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(": ") {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let file_name = fields
        .get("Filename")
        .cloned()
        .ok_or("the update file names no file")?;
    let relative = fields
        .get("URL")
        .cloned()
        .unwrap_or_else(|| file_name.clone());
    let url = if relative.starts_with("https://") {
        relative
    } else if relative.contains("://") || relative.starts_with('/') {
        return Err("the update file points to an unsupported address".into());
    } else {
        let base = zsync_url
            .rsplit_once('/')
            .map(|(b, _)| b)
            .unwrap_or(zsync_url);
        format!("{base}/{relative}")
    };
    let sha1 = fields.get("SHA-1").map(|s| s.to_ascii_lowercase());
    let newer = match (&sha1, installed_sha1) {
        (Some(new), Some(old)) => Some(!new.eq_ignore_ascii_case(old)),
        _ => Some(true),
    };
    Ok(Latest {
        version: super::version_from_file(&file_name),
        newer,
        file_name,
        url,
        size: fields.get("Length").and_then(|l| l.parse().ok()),
        sha256: None,
        sha1,
        hosts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("157.0.8103.92-1", "157.0.8103.78-1"));
        assert!(!is_newer("157.0.8103.78-1", "157.0.8103.78-1"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(!is_newer("1.9", "1.10"));
        assert!(is_newer("v2.0", "1.9"));
        assert!(is_newer(NEWER_BUILD, "1.0"));
    }

    #[test]
    fn wildcards_match_release_files() {
        assert!(glob_match(
            "Obsidian-*.AppImage",
            "Obsidian-1.9.12.AppImage"
        ));
        assert!(glob_match("*x86_64.AppImage", "app-2.0-x86_64.AppImage"));
        assert!(!glob_match(
            "Obsidian-*.AppImage",
            "Obsidian-1.9.12-arm64.AppImage.zsync"
        ));
        assert!(glob_match("exact.AppImage", "exact.AppImage"));
    }

    #[test]
    fn github_assets_are_picked_by_pattern() {
        let release = json!({
            "tag_name": "v1.9.12",
            "assets": [
                {"name": "Obsidian-1.9.12.AppImage.zsync", "browser_download_url": "https://github.com/o/r/releases/download/v1.9.12/Obsidian-1.9.12.AppImage.zsync"},
                {"name": "Obsidian-1.9.12.AppImage", "size": 120, "digest": "sha256:abc",
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.9.12/Obsidian-1.9.12.AppImage"},
            ]
        });
        let latest = pick_github_asset(&release, "Obsidian-*.AppImage.zsync").unwrap();
        assert_eq!(latest.version.as_deref(), Some("1.9.12"));
        assert_eq!(latest.size, Some(120));
        assert_eq!(latest.sha256.as_deref(), Some("abc"));
        assert!(pick_github_asset(&release, "Other-*.AppImage").is_err());
    }

    #[test]
    fn zsync_headers_resolve_relative_files() {
        let raw = b"zsync: 0.6.2\nFilename: Tool-3.1.AppImage\nLength: 99\nURL: Tool-3.1.AppImage\nSHA-1: ABCDEF\n\n\x00\x01binary";
        let latest = parse_zsync_header(
            raw,
            "https://example.org/dl/Tool-latest.AppImage.zsync",
            Some("abcdef"),
        )
        .unwrap();
        assert_eq!(latest.url, "https://example.org/dl/Tool-3.1.AppImage");
        assert_eq!(latest.newer, Some(false));
        assert_eq!(latest.version_label(Some("3.0")).as_deref(), Some("3.0"));
        let changed = parse_zsync_header(raw, "https://example.org/x.zsync", Some("000")).unwrap();
        assert_eq!(changed.version_label(Some("3.0")).as_deref(), Some("3.1"));
        assert_eq!(
            changed.version_label(Some("3.1")).as_deref(),
            Some(NEWER_BUILD)
        );
        let bad = b"Filename: x\nURL: http://evil/x\n\n";
        assert!(parse_zsync_header(bad, "https://example.org/x.zsync", None).is_err());
    }
}
