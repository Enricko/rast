//! The official package catalog: a JSON document published by
//! `Enricko/rast-packages` CI and signed with an Ed25519 key whose public half
//! is compiled into RAST.
//!
//! * `catalog.json` lists every package, its verified versions (full package
//!   manifests, the same schema as `rast-package.json`) and upstream versions
//!   that were detected but are not verified yet.
//! * `catalog.json.sig` holds `{keyId, algorithm: "ed25519", signature}` over
//!   the exact bytes of `catalog.json`.
//! * `sequence` only ever increases; RAST refuses a catalog older than the one
//!   it already accepted (no rollback to hide updates).
//! * The last verified catalog is cached under `<data_dir>/packages/catalog/`
//!   and re-verified when loaded.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};

use super::{
    manifest::{self, Manifest, Publisher},
    net::{self, OriginPolicy},
};

pub const OFFICIAL_CATALOG_URL: &str =
    "https://github.com/Enricko/rast-packages/releases/download/catalog/catalog.json";
/// Official signing keys (`key id`, base64 raw Ed25519 public key).
pub const OFFICIAL_KEYS: &[(&str, &str)] = &[(
    "rast-official-2026-11",
    "ckXYZF+YAsH3mbVaukQbzqNKfaER3ognkhuqtmZxV4M=",
)];
pub const CATALOG_KIND: &str = "rast-catalog";
pub const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
/// Hosts GitHub uses for release downloads.
pub const GITHUB_DOWNLOAD_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub id: String,
    pub key: Vec<u8>,
    pub official: bool,
}

/// Parse `"<key-id>:<base64>"` (configuration format).
pub fn parse_trusted_key(raw: &str) -> Result<TrustedKey, String> {
    let (id, b64) = raw
        .split_once(':')
        .ok_or_else(|| format!("catalog key {raw:?} must look like <key-id>:<base64>"))?;
    let key = B64
        .decode(b64.trim())
        .map_err(|_| format!("catalog key {id:?} is not valid base64"))?;
    if key.len() != 32 || id.trim().is_empty() {
        return Err(format!(
            "catalog key {id:?} must be a 32-byte Ed25519 public key"
        ));
    }
    Ok(TrustedKey {
        id: id.trim().to_string(),
        key,
        official: false,
    })
}

pub fn trusted_keys(extra: &[String]) -> Vec<TrustedKey> {
    let mut keys: Vec<TrustedKey> = OFFICIAL_KEYS
        .iter()
        .map(|(id, b64)| TrustedKey {
            id: id.to_string(),
            key: B64
                .decode(b64)
                .expect("official catalog key is valid base64"),
            official: true,
        })
        .collect();
    keys.extend(extra.iter().filter_map(|k| parse_trusted_key(k).ok()));
    keys
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureFile {
    key_id: String,
    algorithm: String,
    signature: String,
}

/// Verify `catalog` against the detached signature; returns the key used.
pub fn verify_signature(
    catalog: &[u8],
    signature_file: &[u8],
    keys: &[TrustedKey],
) -> Result<TrustedKey, String> {
    let sig: SignatureFile = serde_json::from_slice(signature_file)
        .map_err(|e| format!("catalog signature file is invalid: {e}"))?;
    if sig.algorithm != "ed25519" {
        return Err(format!(
            "catalog signature algorithm {:?} is not supported",
            sig.algorithm
        ));
    }
    let key = keys
        .iter()
        .find(|k| k.id == sig.key_id)
        .ok_or_else(|| format!("catalog is signed with an untrusted key ({})", sig.key_id))?;
    let raw = B64
        .decode(sig.signature.trim())
        .map_err(|_| "catalog signature is not valid base64".to_string())?;
    UnparsedPublicKey::new(&ED25519, &key.key)
        .verify(catalog, &raw)
        .map_err(|_| format!("catalog signature does not verify with key {}", key.id))?;
    Ok(key.clone())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// `stable`, `lts`, `security`, `preview`…
    #[serde(default)]
    pub status: String,
}

/// An upstream version the catalog workflow saw but has not verified yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    pub version: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub detected_at: String,
    /// `pending`, `verifying`, `building` or `failed`.
    pub state: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackage {
    id: String,
    #[serde(rename = "type")]
    package_type: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    versions: Vec<serde_json::Value>,
    #[serde(default)]
    detected: Vec<Detected>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCatalog {
    schema_version: u32,
    kind: String,
    #[serde(default)]
    name: String,
    publisher: Publisher,
    #[serde(default)]
    repository: Option<String>,
    sequence: u64,
    generated_at: String,
    #[serde(default)]
    expires_at: Option<String>,
    allowed_hosts: Vec<String>,
    packages: Vec<RawPackage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unsupported {
    pub version: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPackage {
    pub id: String,
    pub package_type: String,
    pub name: String,
    pub description: String,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub channels: Vec<Channel>,
    /// Valid versions, newest first.
    pub versions: Vec<Manifest>,
    /// Entries this RAST cannot use (newer schema, invalid data).
    pub unsupported: Vec<Unsupported>,
    pub detected: Vec<Detected>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub name: String,
    pub publisher: Publisher,
    pub repository: Option<String>,
    pub sequence: u64,
    pub generated_at: String,
    pub expires_at: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub packages: Vec<CatalogPackage>,
}

impl Catalog {
    pub fn package(&self, id: &str) -> Option<&CatalogPackage> {
        self.packages.iter().find(|p| p.id == id)
    }

    pub fn origin_policy(&self) -> OriginPolicy {
        OriginPolicy::new(self.allowed_hosts.iter().cloned())
    }

    /// Unix ms of `expiresAt`, when present and parseable.
    pub fn expires_at_ms(&self) -> Option<i64> {
        self.expires_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_millis())
    }
}

/// Parse a verified catalog document. Individual versions that this RAST
/// cannot use are reported per package instead of failing the whole catalog.
pub fn parse(bytes: &[u8]) -> Result<Catalog, String> {
    let raw: RawCatalog =
        serde_json::from_slice(bytes).map_err(|e| format!("catalog is not valid: {e}"))?;
    if raw.kind != CATALOG_KIND {
        return Err(format!("catalog kind must be {CATALOG_KIND:?}"));
    }
    if raw.schema_version != 1 {
        return Err(format!(
            "catalog schema version {} is not supported by this RAST",
            raw.schema_version
        ));
    }
    let allowed: BTreeSet<String> = raw
        .allowed_hosts
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    if allowed.is_empty() {
        return Err("catalog lists no allowed download hosts".into());
    }
    let mut packages = Vec::new();
    let mut seen = BTreeSet::new();
    for pkg in raw.packages {
        if !seen.insert(pkg.id.clone()) {
            return Err(format!("catalog lists package {:?} twice", pkg.id));
        }
        let mut versions = Vec::new();
        let mut unsupported = Vec::new();
        let mut version_keys = BTreeSet::new();
        for value in pkg.versions {
            let label = value
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let bytes = serde_json::to_vec(&value).unwrap_or_default();
            let checked = manifest::parse(&bytes).and_then(|m| {
                if m.id != pkg.id {
                    return Err(format!("manifest id {:?} does not match package", m.id));
                }
                if m.publisher.id != raw.publisher.id {
                    return Err("manifest publisher differs from the catalog publisher".into());
                }
                for artifact in &m.artifacts {
                    let host = net::https_host(&artifact.url)?;
                    if !allowed.contains(&host) {
                        return Err(format!(
                            "artifact host {host} is not in the catalog's allowed hosts"
                        ));
                    }
                }
                Ok(m)
            });
            match checked {
                Ok(m) => {
                    if version_keys.insert(m.version.clone()) {
                        versions.push(m);
                    } else {
                        unsupported.push(Unsupported {
                            version: label,
                            reason: "listed twice".into(),
                        });
                    }
                }
                Err(reason) => unsupported.push(Unsupported {
                    version: label,
                    reason,
                }),
            }
        }
        manifest::sort_newest_first(&mut versions);
        packages.push(CatalogPackage {
            id: pkg.id,
            package_type: pkg.package_type,
            name: pkg.name,
            description: pkg.description,
            homepage: pkg.homepage,
            license: pkg.license,
            channels: pkg.channels,
            versions,
            unsupported,
            detected: pkg.detected,
        });
    }
    Ok(Catalog {
        name: raw.name,
        publisher: raw.publisher,
        repository: raw.repository,
        sequence: raw.sequence,
        generated_at: raw.generated_at,
        expires_at: raw.expires_at,
        allowed_hosts: allowed.into_iter().collect(),
        packages,
    })
}

/// A catalog that passed signature verification.
#[derive(Debug, Clone)]
pub struct Verified {
    pub catalog: Catalog,
    pub key_id: String,
    pub official_key: bool,
    pub bytes: Vec<u8>,
    pub signature: Vec<u8>,
}

pub fn verify(bytes: &[u8], signature: &[u8], keys: &[TrustedKey]) -> Result<Verified, String> {
    let key = verify_signature(bytes, signature, keys)?;
    let catalog = parse(bytes)?;
    Ok(Verified {
        catalog,
        key_id: key.id,
        official_key: key.official,
        bytes: bytes.to_vec(),
        signature: signature.to_vec(),
    })
}

/// Origin policy for fetching the catalog itself.
pub fn catalog_fetch_policy(url: &str) -> OriginPolicy {
    let mut hosts: Vec<String> = GITHUB_DOWNLOAD_HOSTS
        .iter()
        .map(|h| h.to_string())
        .collect();
    if let Ok(host) = net::https_host(url) {
        hosts.push(host);
    }
    let mut policy = OriginPolicy::new(hosts);
    policy.allow_file = url.starts_with("file:///");
    policy
}

/// Download catalog + signature (blocking) and verify them.
pub fn fetch(url: &str, keys: &[TrustedKey]) -> Result<Verified, String> {
    let policy = catalog_fetch_policy(url);
    let bytes = net::fetch_bytes(url, &policy, &[], MAX_CATALOG_BYTES)?;
    let signature = net::fetch_bytes(&format!("{url}.sig"), &policy, &[], 8 * 1024)
        .map_err(|e| format!("catalog signature: {e}"))?;
    verify(&bytes, &signature, keys)
}

pub fn cache_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("catalog.json"), dir.join("catalog.json.sig"))
}

/// Write the verified catalog to the cache directory atomically.
pub fn store_cache(dir: &Path, verified: &Verified) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let (json_path, sig_path) = cache_paths(dir);
    for (path, data) in [
        (&json_path, &verified.bytes),
        (&sig_path, &verified.signature),
    ] {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}

/// Load and re-verify the cached catalog, if any.
pub fn load_cache(dir: &Path, keys: &[TrustedKey]) -> Option<Verified> {
    let (json_path, sig_path) = cache_paths(dir);
    let bytes = std::fs::read(json_path).ok()?;
    let signature = std::fs::read(sig_path).ok()?;
    match verify(&bytes, &signature, keys) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "ignoring cached package catalog");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::{rand::SystemRandom, signature::Ed25519KeyPair, signature::KeyPair};

    pub struct TestSigner {
        pair: Ed25519KeyPair,
        pub id: String,
    }

    impl TestSigner {
        pub fn new(id: &str) -> Self {
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
            Self {
                pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
                id: id.into(),
            }
        }
        pub fn config_key(&self) -> String {
            format!(
                "{}:{}",
                self.id,
                B64.encode(self.pair.public_key().as_ref())
            )
        }
        pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
            let sig = self.pair.sign(bytes);
            serde_json::to_vec(&serde_json::json!({
                "keyId": self.id,
                "algorithm": "ed25519",
                "signature": B64.encode(sig.as_ref()),
            }))
            .unwrap()
        }
    }

    pub fn catalog_json(sequence: u64, versions: Vec<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "kind": "rast-catalog",
            "name": "Test catalog",
            "publisher": {"id": "example", "name": "Example Publisher"},
            "sequence": sequence,
            "generatedAt": "2026-11-11T00:00:00Z",
            "expiresAt": "2099-01-01T00:00:00Z",
            "allowedHosts": ["github.com", "release-assets.githubusercontent.com"],
            "packages": [{
                "id": "lua", "type": "runtime", "name": "Lua",
                "versions": versions,
                "detected": [{"version": "5.4.9", "channel": "5.4", "state": "verifying"}]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn signature_must_match_a_trusted_key() {
        let signer = TestSigner::new("test-1");
        let other = TestSigner::new("test-1");
        let keys = trusted_keys(&[signer.config_key()]);
        let bytes = catalog_json(1, vec![crate::packages::manifest::tests::sample()]);
        let verified = verify(&bytes, &signer.sign(&bytes), &keys).unwrap();
        assert_eq!(verified.catalog.packages[0].versions.len(), 1);
        assert!(!verified.official_key);
        // Same key id, different key: refused.
        assert!(verify(&bytes, &other.sign(&bytes), &keys).is_err());
        // Tampered document: refused.
        let mut tampered = bytes.clone();
        let pos = tampered.len() - 3;
        tampered[pos] = b' ';
        assert!(verify(&tampered, &signer.sign(&bytes), &keys).is_err());
        // Unknown key id: refused.
        let stranger = TestSigner::new("stranger");
        assert!(verify(&bytes, &stranger.sign(&bytes), &keys).is_err());
        // The official key is always present.
        assert!(trusted_keys(&[]).iter().any(|k| k.official));
    }

    #[test]
    fn invalid_versions_are_reported_not_fatal() {
        let mut bad_host = crate::packages::manifest::tests::sample();
        bad_host["version"] = "5.4.7".into();
        bad_host["artifacts"][0]["url"] = "https://evil.example/lua.tar.gz".into();
        let mut future = crate::packages::manifest::tests::sample();
        future["version"] = "5.5.0".into();
        future["schemaVersion"] = 2.into();
        let catalog = parse(&catalog_json(
            7,
            vec![crate::packages::manifest::tests::sample(), bad_host, future],
        ))
        .unwrap();
        let pkg = catalog.package("lua").unwrap();
        assert_eq!(pkg.versions.len(), 1);
        assert_eq!(pkg.unsupported.len(), 2);
        assert!(pkg
            .unsupported
            .iter()
            .any(|u| u.reason.contains("allowed hosts")));
        assert_eq!(pkg.detected[0].state, "verifying");
        assert_eq!(catalog.sequence, 7);
    }

    #[test]
    fn key_config_format() {
        assert!(parse_trusted_key("abc").is_err());
        assert!(parse_trusted_key("id:not-base64!").is_err());
        assert!(parse_trusted_key("id:AAAA").is_err());
        let signer = TestSigner::new("k");
        assert!(parse_trusted_key(&signer.config_key()).is_ok());
    }
}
