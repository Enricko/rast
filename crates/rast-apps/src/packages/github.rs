//! "Install from GitHub": resolve a repository or release link to one
//! published release that carries a `rast-package.json` asset, then validate
//! the manifest against the release itself.
//!
//! Rules for third-party packages:
//! * the release must be published (not a draft);
//! * every artifact URL must be one of *that release's* asset download URLs
//!   (same repository, same tag) — no other origins;
//! * the artifact size and SHA-256 in the manifest must match the size and
//!   digest GitHub reports for the asset, when GitHub reports one;
//! * downloads (manifest and artifacts) may only redirect to GitHub's release
//!   asset hosts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    catalog::GITHUB_DOWNLOAD_HOSTS,
    manifest::{self, Manifest, MANIFEST_FILE},
    net::{self, OriginPolicy},
};

pub const API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubRef {
    pub owner: String,
    pub repo: String,
    /// Specific release tag; `None` means the latest published release.
    pub tag: Option<String>,
}

impl GithubRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !s.starts_with('-')
}

fn valid_repo(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Accepts `owner/repo`, `https://github.com/owner/repo[.git]`, and release
/// links (`/releases`, `/releases/latest`, `/releases/tag/<tag>`,
/// `/releases/download/<tag>/<asset>`).
pub fn parse_link(input: &str) -> Result<GithubRef, String> {
    let trimmed = input.trim();
    let path = if let Some(rest) = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("https://www.github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
    {
        rest
    } else if trimmed.starts_with("http://") || trimmed.contains("://") {
        return Err("Enter an https://github.com/… repository or release link.".into());
    } else {
        trimmed
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err("Enter a GitHub repository (owner/repo) or a release link.".into());
    }
    let owner = parts[0].to_string();
    let repo = parts[1].trim_end_matches(".git").to_string();
    if !valid_owner(&owner) || !valid_repo(&repo) {
        return Err(format!(
            "{owner}/{repo} is not a valid GitHub repository name."
        ));
    }
    let tag = match parts.get(2..).unwrap_or(&[]) {
        [] | ["releases"] | ["releases", "latest"] => None,
        ["releases", "tag", tag, ..] => Some(decode(tag)),
        ["releases", "download", tag, ..] => Some(decode(tag)),
        ["tree", ..] | ["blob", ..] => None,
        _ => return Err("This GitHub link is not a repository or release link.".into()),
    };
    if let Some(tag) = &tag {
        if tag.is_empty() || tag.len() > 128 || tag.chars().any(|c| c.is_control()) {
            return Err("The release tag in this link is not valid.".into());
        }
    }
    Ok(GithubRef { owner, repo, tag })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(rename(deserialize = "browser_download_url"))]
    pub download_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    #[serde(rename(deserialize = "id"))]
    pub id: u64,
    #[serde(rename(deserialize = "tag_name"))]
    pub tag: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default, rename(deserialize = "published_at"))]
    pub published_at: Option<String>,
    #[serde(rename(deserialize = "html_url"))]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

fn api_policy() -> OriginPolicy {
    OriginPolicy::new(["api.github.com"])
}

pub fn download_policy() -> OriginPolicy {
    OriginPolicy::new(GITHUB_DOWNLOAD_HOSTS.iter().copied())
}

fn api_get(url: &str, token: Option<&str>) -> Result<Vec<u8>, String> {
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    if let Some(auth) = auth.as_deref() {
        headers.push(("Authorization", auth));
    }
    net::fetch_bytes(url, &api_policy(), &headers, 4 * 1024 * 1024)
}

/// Resolve the release a link points at (latest published when no tag).
pub fn resolve(r: &GithubRef, token: Option<&str>) -> Result<Release, String> {
    let url = match &r.tag {
        Some(tag) => format!(
            "{API_BASE}/repos/{}/{}/releases/tags/{}",
            r.owner,
            r.repo,
            percent_encoding::utf8_percent_encode(tag, percent_encoding::NON_ALPHANUMERIC)
        ),
        None => format!("{API_BASE}/repos/{}/{}/releases/latest", r.owner, r.repo),
    };
    let body = api_get(&url, token).map_err(|e| {
        if e.contains("404") {
            match &r.tag {
                Some(tag) => format!("{} has no published release {tag}.", r.full_name()),
                None => format!(
                    "{} has no published release (or the repository is private or does not exist).",
                    r.full_name()
                ),
            }
        } else {
            e
        }
    })?;
    let release: Release =
        serde_json::from_slice(&body).map_err(|e| format!("unexpected GitHub response: {e}"))?;
    if release.draft {
        return Err("Draft releases cannot be installed.".into());
    }
    Ok(release)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub label: String,
    /// `ok`, `warning` or `failed`.
    pub status: String,
    pub detail: String,
}

fn check(label: &str, status: &str, detail: impl Into<String>) -> Check {
    Check {
        label: label.into(),
        status: status.into(),
        detail: detail.into(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspected {
    pub repo: String,
    pub release: Release,
    pub manifest: Manifest,
    pub manifest_sha256: String,
    pub checks: Vec<Check>,
}

fn asset_digest(asset: &Asset) -> Option<String> {
    asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|d| d.to_ascii_lowercase())
}

/// Validate a manifest against the release it came from.
pub fn cross_check(
    release: &Release,
    manifest: &Manifest,
    manifest_asset: &Asset,
    manifest_sha256: &str,
) -> Result<Vec<Check>, String> {
    let mut checks = vec![check(
        "Manifest",
        "ok",
        format!(
            "{MANIFEST_FILE} matches schema version {}",
            manifest::SCHEMA_VERSION
        ),
    )];
    match asset_digest(manifest_asset) {
        Some(d) if d == manifest_sha256 => checks.push(check(
            "Manifest digest",
            "ok",
            "matches the digest GitHub reports for the asset",
        )),
        Some(d) => {
            return Err(format!(
                "The downloaded manifest does not match GitHub's digest ({d})."
            ))
        }
        None => checks.push(check(
            "Manifest digest",
            "warning",
            "GitHub reports no digest for this asset",
        )),
    }
    for artifact in &manifest.artifacts {
        let asset = release
            .assets
            .iter()
            .find(|a| a.download_url == artifact.url)
            .ok_or_else(|| {
                format!(
                    "Artifact for {} is not an asset of release {} (only this release's assets may be downloaded).",
                    artifact.target, release.tag
                )
            })?;
        if asset.size != artifact.size {
            return Err(format!(
                "Artifact {} is {} bytes on GitHub but the manifest declares {}.",
                asset.name, asset.size, artifact.size
            ));
        }
        match asset_digest(asset) {
            Some(d) if d == artifact.sha256 => checks.push(check(
                &format!("Artifact {}", artifact.target),
                "ok",
                format!("{} — SHA-256 matches GitHub's digest", asset.name),
            )),
            Some(d) => {
                return Err(format!(
                    "Artifact {} has SHA-256 {d} on GitHub but the manifest declares {}.",
                    asset.name, artifact.sha256
                ))
            }
            None => checks.push(check(
                &format!("Artifact {}", artifact.target),
                "warning",
                format!(
                    "{} — GitHub reports no digest; the manifest SHA-256 is verified after download",
                    asset.name
                ),
            )),
        }
    }
    Ok(checks)
}

/// Step 1 of the import: resolve, download and validate (blocking).
pub fn inspect(r: &GithubRef, token: Option<&str>) -> Result<Inspected, String> {
    let release = resolve(r, token)?;
    let manifest_asset = release
        .assets
        .iter()
        .find(|a| a.name == MANIFEST_FILE)
        .cloned()
        .ok_or_else(|| {
            format!(
                "Release {} of {} has no {MANIFEST_FILE} asset, so it is not a RAST package release.",
                release.tag,
                r.full_name()
            )
        })?;
    if manifest_asset.size as usize > manifest::MAX_MANIFEST_BYTES {
        return Err(format!("{MANIFEST_FILE} is too large."));
    }
    let bytes = net::fetch_bytes(
        &manifest_asset.download_url,
        &download_policy(),
        &[],
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    let manifest_sha256 = hex::encode(Sha256::digest(&bytes));
    let manifest = manifest::parse(&bytes).map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
    let checks = cross_check(&release, &manifest, &manifest_asset, &manifest_sha256)?;
    Ok(Inspected {
        repo: r.full_name(),
        release,
        manifest,
        manifest_sha256,
        checks,
    })
}

/// Latest published release with a manifest (for update detection).
pub fn latest_manifest(
    repo: &str,
    token: Option<&str>,
) -> Result<Option<(Release, Manifest)>, String> {
    let r = parse_link(repo)?;
    match inspect(&r, token) {
        Ok(found) => Ok(Some((found.release, found.manifest))),
        Err(e) if e.contains("no published release") || e.contains("not a RAST package") => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_links() {
        let cases = [
            ("Enricko/rast-package-template", None),
            ("https://github.com/Enricko/rast-package-template", None),
            ("https://github.com/Enricko/rast-package-template.git", None),
            ("https://github.com/Enricko/rast-package-template/releases", None),
            ("https://github.com/Enricko/rast-package-template/releases/latest", None),
            (
                "https://github.com/Enricko/rast-package-template/releases/tag/v5.4.8-1",
                Some("v5.4.8-1"),
            ),
            (
                "https://github.com/Enricko/rast-package-template/releases/download/v5.4.8%2Br1/x.tar.gz",
                Some("v5.4.8+r1"),
            ),
        ];
        for (input, tag) in cases {
            let r = parse_link(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(r.full_name(), "Enricko/rast-package-template", "{input}");
            assert_eq!(r.tag.as_deref(), tag, "{input}");
        }
        for bad in [
            "http://github.com/a/b",
            "https://gitlab.com/a/b",
            "a",
            "https://github.com/a/b/issues/1",
            "https://github.com/../b",
        ] {
            assert!(parse_link(bad).is_err(), "{bad} accepted");
        }
    }

    fn release(assets: Vec<Asset>) -> Release {
        Release {
            id: 1,
            tag: "v5.4.8-1".into(),
            name: None,
            draft: false,
            prerelease: false,
            published_at: None,
            html_url: "https://github.com/example/lua/releases/tag/v5.4.8-1".into(),
            assets,
        }
    }

    #[test]
    fn manifest_must_match_release_assets() {
        let m: Manifest =
            serde_json::from_value(crate::packages::manifest::tests::sample()).unwrap();
        let artifact = &m.artifacts[0];
        let good = Asset {
            name: "lua-5.4.8-linux-x86_64.tar.gz".into(),
            size: artifact.size,
            digest: Some(format!("sha256:{}", artifact.sha256)),
            download_url: artifact.url.clone(),
        };
        let manifest_asset = Asset {
            name: MANIFEST_FILE.into(),
            size: 10,
            digest: Some(format!("sha256:{}", "b".repeat(64))),
            download_url:
                "https://github.com/example/lua/releases/download/v5.4.8-1/rast-package.json".into(),
        };
        let checks = cross_check(
            &release(vec![good.clone()]),
            &m,
            &manifest_asset,
            &"b".repeat(64),
        )
        .unwrap();
        assert!(checks.iter().all(|c| c.status == "ok"), "{checks:?}");

        // Wrong digest, wrong size, foreign URL, manifest digest mismatch.
        let mut wrong_digest = good.clone();
        wrong_digest.digest = Some(format!("sha256:{}", "c".repeat(64)));
        assert!(cross_check(
            &release(vec![wrong_digest]),
            &m,
            &manifest_asset,
            &"b".repeat(64)
        )
        .is_err());
        let mut wrong_size = good.clone();
        wrong_size.size += 1;
        assert!(cross_check(
            &release(vec![wrong_size]),
            &m,
            &manifest_asset,
            &"b".repeat(64)
        )
        .is_err());
        let mut other_release = good.clone();
        other_release.download_url =
            "https://github.com/example/lua/releases/download/v0/x.tar.gz".into();
        assert!(cross_check(
            &release(vec![other_release]),
            &m,
            &manifest_asset,
            &"b".repeat(64)
        )
        .is_err());
        assert!(cross_check(
            &release(vec![good.clone()]),
            &m,
            &manifest_asset,
            &"d".repeat(64)
        )
        .is_err());

        // No digest from GitHub: allowed with a warning (SHA-256 checked on download).
        let mut no_digest = good;
        no_digest.digest = None;
        let checks = cross_check(
            &release(vec![no_digest]),
            &m,
            &manifest_asset,
            &"b".repeat(64),
        )
        .unwrap();
        assert!(checks.iter().any(|c| c.status == "warning"));
    }
}
