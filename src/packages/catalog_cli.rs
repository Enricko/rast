//! The catalog workflow of `Enricko/rast-packages`, as a Rust CLI.
//!
//! This is the Rust port of the repository's `scripts/catalog.py` (and of the
//! validation helpers in `scripts/rastpkg.py`), run by
//! `.github/workflows/catalog.yml`:
//!
//! * [`Command::Previous`] downloads the published catalog with `gh` and
//!   verifies its Ed25519 signature.
//! * [`Command::Discover`] queries go.dev, nodejs.org, php.net and the
//!   python-build-standalone releases, then writes `plan.json` and the job
//!   matrices.
//! * [`Command::Interim`] publishes an interim catalog that announces the new
//!   versions as `detected` (`verifying`/`building`) before CI has checked
//!   them.
//! * [`Command::Verify`] downloads, verifies and smoke-tests the upstream
//!   artifacts for one host target.
//! * [`Command::PhpResult`] smoke-tests a CI-built PHP archive.
//! * [`Command::Assemble`] produces the final catalog from the plan, the
//!   previous catalog and the verification results.
//! * [`Command::Sign`] signs `catalog.json` with the Ed25519 key in
//!   `CATALOG_SIGNING_KEY` (PKCS#8 PEM).
//! * [`Command::Check`] validates every embedded manifest, verifies the
//!   signature, the sequence and (optionally) every artifact's published
//!   bytes.
//!
//! Validation is delegated to [`super::manifest`] and [`super::catalog`], the
//! same code RAST itself runs, instead of the Python rules in `rastpkg.py`.
//! Extraction uses [`super::archive`]'s safe tar.gz extractor. `gpgv`, `gh` and
//! `tar` are external commands, as in the reference; signatures use `ring`
//! (no `openssl`). All HTTP requests go through `ureq`, require the final URL
//! to be https and use the catalog user agent.
//!
//! Configuration (`catalog.config.json`) is read from the current directory,
//! which is the repository root in the workflow.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, TimeDelta, Timelike, Utc};
use ring::signature::{Ed25519KeyPair, UnparsedPublicKey, ED25519};
use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::{archive, authoring, catalog, manifest, net, version};

/// User agent for every request, identical to the Python reference.
const UA: &str = "rast-packages-catalog/1 (+https://github.com/Enricko/rast-packages)";
const GO_JSON: &str = "https://go.dev/dl/?mode=json&include=all";
const NODE_INDEX: &str = "https://nodejs.org/dist/index.json";
const NODE_KEYRING: &str =
    "https://raw.githubusercontent.com/nodejs/release-keys/HEAD/gpg/pubring.kbx";
const PBS_REPO: &str = "astral-sh/python-build-standalone";
const PHP_JSON: &str = "https://www.php.net/releases/index.php?json&version={channel}&max=1";
/// JSON documents fetched into memory are bounded.
const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;
/// Archives are streamed to disk and bounded while hashing.
const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const JSON_TIMEOUT: Duration = Duration::from_secs(120);
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(600);

/// A catalog subcommand. The Lead wires these to the CLI.
#[derive(Debug, Clone)]
pub enum Command {
    /// Download and verify the published catalog (absent on the first run).
    Previous { out: PathBuf },
    /// Discover upstream versions; write the plan and the GitHub job outputs.
    Discover {
        previous: PathBuf,
        out: PathBuf,
        withhold: String,
        force: bool,
        github_output: Option<PathBuf>,
    },
    /// Publish-ready catalog that announces new versions as `detected`.
    Interim {
        plan: PathBuf,
        previous: PathBuf,
        out: PathBuf,
    },
    /// Download, verify and smoke-test upstream artifacts for one target.
    Verify {
        plan: PathBuf,
        target: String,
        out: PathBuf,
    },
    /// Smoke-test a CI-built PHP archive and record the result.
    PhpResult {
        archive: PathBuf,
        version: String,
        target: String,
        attestation_url: String,
        out: PathBuf,
    },
    /// Final catalog: carried versions plus newly verified ones.
    Assemble {
        plan: PathBuf,
        previous: PathBuf,
        results: PathBuf,
        php_releases: Option<PathBuf>,
        out: PathBuf,
    },
    /// Sign `<catalog>` into `<catalog>.sig` with `CATALOG_SIGNING_KEY`.
    Sign { catalog: PathBuf },
    /// Validate a catalog, its signature, its sequence and its artifacts.
    Check {
        catalog: PathBuf,
        previous: Option<PathBuf>,
        download: Option<DownloadMode>,
    },
    /// Print the PHP build inputs from the plan/config for a workflow step.
    PhpInputs {
        plan: PathBuf,
        version: String,
        github_output: Option<PathBuf>,
    },
    /// Publish CI-built PHP archives as releases and report their tags.
    PhpPublish {
        results: PathBuf,
        archives: PathBuf,
        github_output: Option<PathBuf>,
    },
    /// Normalise downloaded workflow artifacts into `work/` for assembly.
    Collect {
        artifacts: PathBuf,
        work: PathBuf,
        /// Result of the verify job (`success`, `failure`, `skipped`, ...).
        verify_result: String,
    },
    /// Print a catalog's sequence number.
    Sequence { catalog: PathBuf },
    /// Build a static PHP CLI for one target with static-php-cli in Docker.
    BuildPhp {
        version: String,
        target: String,
        source_sha256: String,
        extensions: String,
        spc_version: String,
        builder: String,
        out: PathBuf,
    },
}

/// Which artifacts `check --download` fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadMode {
    /// Only artifacts published by the catalog's own repository.
    New,
    /// Every artifact.
    All,
}

/// Run one catalog subcommand.
pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Previous { out } => cmd_previous(&out),
        Command::Discover {
            previous,
            out,
            withhold,
            force,
            github_output,
        } => cmd_discover(&previous, &out, &withhold, force, github_output.as_deref()),
        Command::Interim {
            plan,
            previous,
            out,
        } => cmd_interim(&plan, &previous, &out),
        Command::Verify { plan, target, out } => cmd_verify(&plan, &target, &out),
        Command::PhpResult {
            archive,
            version,
            target,
            attestation_url,
            out,
        } => cmd_php_result(&archive, &version, &target, &attestation_url, &out),
        Command::Assemble {
            plan,
            previous,
            results,
            php_releases,
            out,
        } => cmd_assemble(&plan, &previous, &results, php_releases.as_deref(), &out),
        Command::Sign { catalog } => cmd_sign(&catalog),
        Command::Check {
            catalog,
            previous,
            download,
        } => cmd_check(&catalog, previous.as_deref(), download),
        Command::PhpInputs {
            plan,
            version,
            github_output,
        } => cmd_php_inputs(&plan, &version, github_output.as_deref()),
        Command::PhpPublish {
            results,
            archives,
            github_output,
        } => cmd_php_publish(&results, &archives, github_output.as_deref()),
        Command::Collect {
            artifacts,
            work,
            verify_result,
        } => cmd_collect(&artifacts, &work, &verify_result),
        Command::Sequence { catalog } => cmd_sequence(&catalog),
        Command::BuildPhp {
            version,
            target,
            source_sha256,
            extensions,
            spc_version,
            builder,
            out,
        } => cmd_build_php(
            &version,
            &target,
            &source_sha256,
            &extensions,
            &spc_version,
            &builder,
            &out,
        ),
    }
}

// ------------------------------------------------------------------ config

/// A JSON object deserialized in document order.
///
/// `serde_json::Map` is a `BTreeMap` (the `preserve_order` feature is off), so
/// the configuration's package/target order would be lost. The reference
/// iterates them in file order; this wrapper keeps it.
#[derive(Debug, Clone)]
struct Ordered<T> {
    entries: Vec<(String, T)>,
}

impl<T> Ordered<T> {
    fn get(&self, key: &str) -> Option<&T> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Ordered<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for OrderedVisitor<T> {
            type Value = Ordered<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, T>()? {
                    entries.push((key, value));
                }
                Ok(Ordered { entries })
            }
        }

        deserializer.deserialize_map(OrderedVisitor(PhantomData))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    name: String,
    publisher: manifest::Publisher,
    repository: String,
    key_id: String,
    public_key: String,
    min_rast_version: String,
    expires_days: i64,
    refresh_before_days: i64,
    allowed_hosts: Vec<String>,
    targets: Ordered<TargetConfig>,
    #[serde(default)]
    withheld: Vec<String>,
    #[serde(default)]
    revisions: BTreeMap<String, u32>,
    packages: Ordered<PackageConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetConfig {
    runner: String,
    go: String,
    node: String,
    python: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageConfig {
    name: String,
    description: String,
    homepage: String,
    license: String,
    #[serde(default)]
    keep_patches: u32,
    #[serde(default)]
    spc_version: Option<String>,
    #[serde(default)]
    builder: Option<String>,
    #[serde(default)]
    extensions: Option<String>,
    channels: Vec<Value>,
}

fn load_config() -> Result<Config, String> {
    let path = PathBuf::from("catalog.config.json");
    let bytes = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

fn channel_ids(package: &PackageConfig) -> Vec<String> {
    package
        .channels
        .iter()
        .filter_map(|channel| {
            channel
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn package_config<'a>(cfg: &'a Config, id: &str) -> Result<&'a PackageConfig, String> {
    cfg.packages
        .get(id)
        .ok_or_else(|| format!("catalog.config.json has no packages.{id}"))
}

fn runner_for<'a>(cfg: &'a Config, target: &str) -> Result<&'a str, String> {
    cfg.targets
        .get(target)
        .map(|config| config.runner.as_str())
        .ok_or_else(|| format!("catalog.config.json has no targets.{target}"))
}

// ----------------------------------------------------------------- helpers

fn log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

fn now() -> DateTime<Utc> {
    Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now)
}

fn iso(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(raw) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        return Ok(naive.and_utc());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc());
    }
    Err(format!("{raw:?} is not an ISO 8601 timestamp"))
}

fn json_text(value: &Value) -> Result<String, String> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialize JSON: {e}"))?;
    text.push('\n');
    Ok(text)
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    let text = json_text(value)?;
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// `<path><suffix>` (the reference appends `.sig` to the whole path string).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// Python's `repr` for a list of strings (only used in log lines).
fn py_list_repr(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| format!("'{}'", item.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", parts.join(", "))
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn gh_token() -> Option<String> {
    env_nonempty("GH_TOKEN").or_else(|| env_nonempty("GITHUB_TOKEN"))
}

/// `RUN_URL`: link to the workflow run, `None` for local runs.
fn run_url() -> Option<String> {
    let server = env::var("GITHUB_SERVER_URL").ok();
    run_url_from(
        server.as_deref(),
        env::var("GITHUB_REPOSITORY").ok().as_deref(),
        env::var("GITHUB_RUN_ID").ok().as_deref(),
    )
}

fn run_url_from(
    server: Option<&str>,
    repository: Option<&str>,
    run_id: Option<&str>,
) -> Option<String> {
    let repository = repository.filter(|value| !value.is_empty())?;
    let run_id = run_id.filter(|value| !value.is_empty())?;
    let server = server.unwrap_or("https://github.com");
    Some(format!("{server}/{repository}/actions/runs/{run_id}"))
}

fn run_note(run_url: Option<&str>) -> String {
    match run_url {
        Some(url) => format!(" ({url})"),
        None => String::new(),
    }
}

fn withheld_set(config: &[String], extra: &str) -> BTreeSet<String> {
    let mut items: BTreeSet<String> = config.iter().cloned().collect();
    for item in extra.split(',') {
        let item = item.trim();
        if !item.is_empty() {
            items.insert(item.to_string());
        }
    }
    items
}

fn default_channel(version: &str) -> String {
    version::Version::parse(version)
        .map(|parsed| parsed.default_channel())
        .unwrap_or_default()
}

/// The update stream: Node.js uses the major version, the others major.minor.
fn channel_of(package: &str, version: &str) -> Option<String> {
    if package == "nodejs" {
        Some(version.split('.').next().unwrap_or("").to_string())
    } else {
        None
    }
}

fn is_go_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    [major, minor, patch]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Version and revision ordering of catalog versions (ascending, as Python's
/// `(version_key(version), revision)` key).
fn compare_manifest_order(a: &Value, b: &Value) -> Ordering {
    let a_version = a.get("version").and_then(Value::as_str).unwrap_or("");
    let b_version = b.get("version").and_then(Value::as_str).unwrap_or("");
    let a_revision = a.get("revision").and_then(Value::as_u64).unwrap_or(0);
    let b_revision = b.get("revision").and_then(Value::as_u64).unwrap_or(0);
    version::compare(a_version, b_version).then(a_revision.cmp(&b_revision))
}

// ---------------------------------------------------------------- HTTP

fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(20)))
        .user_agent(UA)
        .max_redirects(10)
        .https_only(true)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn ensure_final_https(
    url: &str,
    response: &ureq::http::Response<ureq::Body>,
) -> Result<(), String> {
    use ureq::ResponseExt;
    if response.get_uri().scheme_str() != Some("https") {
        return Err(format!("{url} redirected to a non-https location"));
    }
    Ok(())
}

fn open_response(
    url: &str,
    token: Option<&str>,
    accept: Option<&str>,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let agent = http_agent(timeout);
    let mut request = agent.get(url);
    if let Some(token) = token {
        // Credentials only go to the GitHub API itself, never to a CDN host.
        if url.starts_with("https://api.github.com/") {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
    }
    if let Some(accept) = accept {
        request = request.header("Accept", accept);
    }
    let response = request
        .call()
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered HTTP {status}"));
    }
    ensure_final_https(url, &response)?;
    Ok(response)
}

fn http_get(url: &str, token: Option<&str>, accept: Option<&str>) -> Result<Vec<u8>, String> {
    let response = open_response(url, token, accept, JSON_TIMEOUT)?;
    let mut reader = response
        .into_body()
        .into_with_config()
        .limit(MAX_JSON_BYTES)
        .reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {url}: {error}"))?;
    Ok(bytes)
}

fn http_json(url: &str, token: Option<&str>) -> Result<Value, String> {
    let bytes = http_get(url, token, Some("application/json"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{url} is not valid JSON: {error}"))
}

/// Stream `url` to `dest`; return `(sha256, size)`. Redirects must stay https.
fn download(url: &str, dest: &Path) -> Result<(String, u64), String> {
    let response = open_response(url, None, None, ARCHIVE_TIMEOUT)?;
    let mut reader = response
        .into_body()
        .into_with_config()
        .limit(MAX_DOWNLOAD_BYTES + 1)
        .reader();
    let mut file =
        File::create(dest).map_err(|error| format!("cannot create {}: {error}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("download interrupted: {error}"))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > MAX_DOWNLOAD_BYTES {
            let _ = fs::remove_file(dest);
            return Err(format!(
                "{url} is larger than the {MAX_DOWNLOAD_BYTES} byte download limit"
            ));
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("cannot write download: {error}"))?;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Final URL of a HEAD request (redirects followed; must stay https).
fn head_final_url(url: &str) -> Result<String, String> {
    use ureq::ResponseExt;
    let agent = http_agent(Duration::from_secs(60));
    let response = agent
        .head(url)
        .call()
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered HTTP {status}"));
    }
    ensure_final_https(url, &response)?;
    Ok(response.get_uri().to_string())
}

// --------------------------------------------------------------- versions

fn manifest_key(manifest: &Value) -> Option<String> {
    let id = manifest.get("id").and_then(Value::as_str)?;
    let version = manifest.get("version").and_then(Value::as_str)?;
    Some(format!("{id}@{version}"))
}

fn artifact_targets(manifest: &Value) -> BTreeSet<String> {
    manifest
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|artifacts| {
            artifacts
                .iter()
                .filter_map(|artifact| {
                    artifact
                        .get("target")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `old and old["revision"] >= revision and targets_done and not force`.
fn is_carry(
    old: Option<&Value>,
    revision: u32,
    item_targets: &BTreeMap<String, TargetInfo>,
    force: bool,
) -> bool {
    let Some(old) = old else {
        return false;
    };
    let old_revision = old.get("revision").and_then(Value::as_u64).unwrap_or(0) as u32;
    let old_targets = artifact_targets(old);
    let targets_done = item_targets
        .keys()
        .all(|target| old_targets.contains(target));
    !force && old_revision >= revision && targets_done
}

// ------------------------------------------------------------- signatures

const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Extract and base64-decode the body of the first `-----BEGIN <label>-----`
/// PEM block.
fn pem_base64_der(text: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let rest = &text[start..];
    let stop = rest.find(&end)?;
    let body: String = rest[..stop]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    B64.decode(body).ok()
}

fn ed25519_public_key_from_der(der: &[u8]) -> Result<[u8; 32], String> {
    let raw: &[u8] = if der.len() == 32 {
        der
    } else if der.len() == ED25519_SPKI_PREFIX.len() + 32 && der.starts_with(&ED25519_SPKI_PREFIX) {
        &der[ED25519_SPKI_PREFIX.len()..]
    } else {
        return Err("public key is not an Ed25519 SubjectPublicKeyInfo".into());
    };
    raw.try_into()
        .map_err(|_| "public key is not a 32-byte Ed25519 key".to_string())
}

fn ed25519_public_key(pem: &str) -> Result<[u8; 32], String> {
    let der = pem_base64_der(pem, "PUBLIC KEY")
        .ok_or_else(|| "public key file has no PUBLIC KEY PEM block".to_string())?;
    ed25519_public_key_from_der(&der)
}

fn ed25519_key_pair(pem: &str) -> Result<Ed25519KeyPair, String> {
    let der = pem_base64_der(pem, "PRIVATE KEY")
        .ok_or_else(|| "CATALOG_SIGNING_KEY is not a PEM PKCS#8 private key".to_string())?;
    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
        .map_err(|error| format!("CATALOG_SIGNING_KEY is not a valid Ed25519 key: {error}"))
}

fn verify_signature_bytes(
    catalog_bytes: &[u8],
    signature_text: &str,
    public_key_pem: &str,
    key_id: &str,
) -> Result<(), String> {
    let signature: Value = serde_json::from_str(signature_text)
        .map_err(|error| format!("catalog signature file is invalid: {error}"))?;
    let algorithm = signature.get("algorithm").and_then(Value::as_str);
    let signed_with = signature.get("keyId").and_then(Value::as_str);
    if algorithm != Some("ed25519") || signed_with != Some(key_id) {
        return Err(format!(
            "unexpected signature header: {} {}",
            signed_with.unwrap_or("None"),
            algorithm.unwrap_or("None")
        ));
    }
    let raw = signature
        .get("signature")
        .and_then(Value::as_str)
        .ok_or("catalog signature file has no signature")?;
    let raw = B64
        .decode(raw.trim())
        .map_err(|_| "catalog signature is not valid base64".to_string())?;
    let key = ed25519_public_key(public_key_pem)?;
    UnparsedPublicKey::new(&ED25519, &key)
        .verify(catalog_bytes, &raw)
        .map_err(|_| "catalog signature does not verify".to_string())
}

fn verify_signature_files(
    catalog_path: &Path,
    signature_path: &Path,
    public_key_path: &Path,
    key_id: &str,
) -> Result<(), String> {
    let catalog_bytes = fs::read(catalog_path)
        .map_err(|error| format!("cannot read {}: {error}", catalog_path.display()))?;
    let signature = fs::read_to_string(signature_path)
        .map_err(|error| format!("cannot read {}: {error}", signature_path.display()))?;
    let public_key = fs::read_to_string(public_key_path)
        .map_err(|error| format!("cannot read {}: {error}", public_key_path.display()))?;
    verify_signature_bytes(&catalog_bytes, &signature, &public_key, key_id)
}

// ------------------------------------------------------------- discovery

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanItem {
    #[serde(default)]
    package: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    build_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index_npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sums_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<Value>,
    #[serde(default)]
    targets: BTreeMap<String, TargetInfo>,
    #[serde(default = "default_revision")]
    revision: u32,
    #[serde(default)]
    status: String,
}

fn default_revision() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    #[serde(default)]
    generated_at: String,
    #[serde(default)]
    previous_sequence: u64,
    #[serde(default)]
    withheld: Vec<String>,
    #[serde(default)]
    items: Vec<PlanItem>,
    #[serde(default)]
    dropped: Vec<String>,
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("upstream document has no string {key:?}"))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `cpython-<channel>.<patch>+<tag>-<arch>-unknown-linux-gnu-install_only.tar.gz`
/// (the reference's regex, without a regex dependency).
fn python_asset_version(name: &str, channel: &str, tag: &str) -> Option<String> {
    let rest = name.strip_prefix("cpython-")?;
    let prefix = format!("{channel}.");
    let number_and_rest = rest.strip_prefix(&prefix)?;
    let suffix = format!("+{tag}-");
    let (number, architecture) = number_and_rest.split_once(&suffix)?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let architecture = architecture.strip_suffix("-unknown-linux-gnu-install_only.tar.gz")?;
    if architecture.is_empty()
        || !architecture
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    Some(format!("{channel}.{number}"))
}

fn sort_items_desc(items: &mut [PlanItem]) {
    items.sort_by(|a, b| version::compare(&b.version, &a.version));
}

fn discover_go(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let data = http_json(GO_JSON, None)?;
    let releases = data
        .as_array()
        .ok_or_else(|| format!("{GO_JSON} did not return a JSON array"))?;
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        let mut found = Vec::new();
        for release in releases {
            let version = required_str(release, "version")?;
            let version = version.strip_prefix("go").unwrap_or(version);
            if !is_go_version(version)
                || default_channel(version) != channel
                || held.contains(&format!("go@{version}"))
            {
                continue;
            }
            let files = release
                .get("files")
                .and_then(Value::as_array)
                .ok_or("go.dev release has no files")?;
            let mut targets = BTreeMap::new();
            for (target, target_config) in &cfg.targets.entries {
                let file = files.iter().find(|file| {
                    file.get("os").and_then(Value::as_str) == Some("linux")
                        && file.get("arch").and_then(Value::as_str)
                            == Some(target_config.go.as_str())
                        && file.get("kind").and_then(Value::as_str) == Some("archive")
                });
                if let Some(file) = file {
                    let filename = required_str(file, "filename")?;
                    targets.insert(
                        target.clone(),
                        TargetInfo {
                            url: Some(format!("https://dl.google.com/go/{filename}")),
                            sha256: Some(required_str(file, "sha256")?.to_string()),
                            size: Some(
                                file.get("size")
                                    .and_then(Value::as_u64)
                                    .ok_or("go.dev file has no size")?,
                            ),
                            source: Some(format!("https://go.dev/dl/{filename}")),
                            ..TargetInfo::default()
                        },
                    );
                }
            }
            if !targets.is_empty() {
                found.push(PlanItem {
                    package: "go".into(),
                    version: version.to_string(),
                    channel: channel.clone(),
                    kind: "upstream".into(),
                    targets,
                    ..PlanItem::default()
                });
            }
        }
        sort_items_desc(&mut found);
        found.truncate(package.keep_patches as usize);
        items.extend(found);
    }
    Ok(items)
}

fn discover_node(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let index = http_json(NODE_INDEX, None)?;
    let releases = index
        .as_array()
        .ok_or_else(|| format!("{NODE_INDEX} did not return a JSON array"))?;
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        let mut found = Vec::new();
        for release in releases {
            let version = required_str(release, "version")?;
            let version = version.strip_prefix('v').unwrap_or(version);
            if version.split('.').next() != Some(channel.as_str())
                || !truthy(release.get("lts"))
                || held.contains(&format!("nodejs@{version}"))
            {
                continue;
            }
            let files = release.get("files").and_then(Value::as_array);
            let mut targets = BTreeMap::new();
            for (target, target_config) in &cfg.targets.entries {
                let platform = format!("linux-{}", target_config.node);
                let has_file = files.is_some_and(|files| {
                    files
                        .iter()
                        .any(|file| file.as_str() == Some(platform.as_str()))
                });
                if has_file {
                    let name = format!("node-v{version}-{platform}.tar.gz");
                    targets.insert(
                        target.clone(),
                        TargetInfo {
                            url: Some(format!("https://nodejs.org/dist/v{version}/{name}")),
                            file: Some(name),
                            source: Some(format!("https://nodejs.org/dist/v{version}/")),
                            ..TargetInfo::default()
                        },
                    );
                }
            }
            if !targets.is_empty() {
                found.push(PlanItem {
                    package: "nodejs".into(),
                    version: version.to_string(),
                    channel: channel.clone(),
                    kind: "upstream".into(),
                    release_date: release
                        .get("date")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    index_npm: release
                        .get("npm")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    lts: release
                        .get("lts")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    targets,
                    ..PlanItem::default()
                });
            }
        }
        sort_items_desc(&mut found);
        found.truncate(package.keep_patches as usize);
        items.extend(found);
    }
    Ok(items)
}

fn discover_python(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    let url = format!("https://api.github.com/repos/{PBS_REPO}/releases/latest");
    let release = http_json(&url, token)?;
    let tag = required_str(&release, "tag_name")?.to_string();
    let source = required_str(&release, "html_url")?.to_string();
    let published = release
        .get("published_at")
        .and_then(Value::as_str)
        .unwrap_or("");
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("python-build-standalone release has no assets")?;
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        let mut best: Option<String> = None;
        for asset in assets {
            let Some(name) = asset.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(version) = python_asset_version(name, &channel, &tag) else {
                continue;
            };
            if held.contains(&format!("python@{version}")) {
                continue;
            }
            let better = match &best {
                None => true,
                Some(current) => version::compare(&version, current) == Ordering::Greater,
            };
            if better {
                best = Some(version);
            }
        }
        let Some(best) = best else {
            continue;
        };
        let mut targets = BTreeMap::new();
        for (target, target_config) in &cfg.targets.entries {
            let name = format!(
                "cpython-{best}+{tag}-{}-unknown-linux-gnu-install_only.tar.gz",
                target_config.python
            );
            let asset = assets
                .iter()
                .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name.as_str()));
            if let Some(asset) = asset {
                let digest = asset.get("digest").and_then(Value::as_str).unwrap_or("");
                let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
                targets.insert(
                    target.clone(),
                    TargetInfo {
                        url: Some(required_str(asset, "browser_download_url")?.to_string()),
                        file: Some(name),
                        size: Some(
                            asset
                                .get("size")
                                .and_then(Value::as_u64)
                                .ok_or("python-build-standalone asset has no size")?,
                        ),
                        digest: Some(digest.to_string()),
                        source: Some(source.clone()),
                        ..TargetInfo::default()
                    },
                );
            }
        }
        items.push(PlanItem {
            package: "python".into(),
            version: best,
            channel,
            kind: "upstream".into(),
            release_date: Some(truncate_chars(published, 10)),
            build_tag: Some(tag.clone()),
            targets,
            sums_url: Some(format!(
                "https://github.com/{PBS_REPO}/releases/download/{tag}/SHA256SUMS"
            )),
            ..PlanItem::default()
        });
    }
    Ok(items)
}

fn discover_php(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        let url = PHP_JSON.replace("{channel}", &channel);
        let data = http_json(&url, None)?;
        let mut releases: Vec<(String, Value)> = if data.get("version").is_some() {
            vec![(required_str(&data, "version")?.to_string(), data.clone())]
        } else {
            data.as_object()
                .ok_or_else(|| format!("{url} did not return a JSON object"))?
                .iter()
                .map(|(version, release)| (version.clone(), release.clone()))
                .collect()
        };
        releases.sort_by(|(a, _), (b, _)| version::compare(b, a));
        for (version, release) in releases.into_iter().take(package.keep_patches as usize) {
            if held.contains(&format!("php@{version}")) {
                continue;
            }
            let mut sources: BTreeMap<String, Option<String>> = BTreeMap::new();
            if let Some(list) = release.get("source").and_then(Value::as_array) {
                for entry in list {
                    let Some(filename) = entry.get("filename").and_then(Value::as_str) else {
                        continue;
                    };
                    sources.insert(
                        filename.to_string(),
                        entry
                            .get("sha256")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    );
                }
            }
            let archive = format!("php-{version}.tar.xz");
            let sha256 = sources
                .get(&archive)
                .and_then(|value| value.as_ref())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("php.net lists no sha256 for {archive}"))?
                .clone();
            let mut targets = BTreeMap::new();
            for (target, _) in &cfg.targets.entries {
                targets.insert(target.clone(), TargetInfo::default());
            }
            items.push(PlanItem {
                package: "php".into(),
                version,
                channel: channel.clone(),
                kind: "built".into(),
                release_date: release
                    .get("date")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source: Some(json!({
                    "url": format!("https://www.php.net/distributions/{archive}"),
                    "sha256": sha256,
                    "all": sources,
                })),
                targets,
                ..PlanItem::default()
            });
        }
    }
    Ok(items)
}

fn load_previous(directory: Option<&Path>) -> Result<Option<Value>, String> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    if directory.as_os_str().is_empty() {
        return Ok(None);
    }
    let path = directory.join("catalog.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let document = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    Ok(Some(document))
}

/// Every manifest of the previous catalog, keyed `id@version`.
fn previous_manifests(previous: Option<&Value>) -> BTreeMap<String, Value> {
    let mut manifests = BTreeMap::new();
    let Some(previous) = previous else {
        return manifests;
    };
    if let Some(packages) = previous.get("packages").and_then(Value::as_array) {
        for package in packages {
            if let Some(versions) = package.get("versions").and_then(Value::as_array) {
                for manifest in versions {
                    if let Some(key) = manifest_key(manifest) {
                        manifests.insert(key, manifest.clone());
                    }
                }
            }
        }
    }
    manifests
}

fn read_plan(path: &Path) -> Result<Plan, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a valid plan: {error}", path.display()))
}

fn cmd_previous(out: &Path) -> Result<(), String> {
    let cfg = load_config()?;
    fs::create_dir_all(out).map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let output = ProcessCommand::new("gh")
        .args([
            "release",
            "download",
            "catalog",
            "--pattern",
            "catalog.json",
            "--pattern",
            "catalog.json.sig",
            "--dir",
        ])
        .arg(out)
        .arg("--clobber")
        .output()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if format!("{stderr}{stdout}")
            .to_ascii_lowercase()
            .contains("release not found")
        {
            log("no published catalog yet");
            return Ok(());
        }
        return Err(format!(
            "cannot download the published catalog: {}",
            stderr.trim()
        ));
    }
    verify_signature_files(
        &out.join("catalog.json"),
        &out.join("catalog.json.sig"),
        Path::new(&cfg.public_key),
        &cfg.key_id,
    )?;
    let bytes = fs::read(out.join("catalog.json"))
        .map_err(|error| format!("cannot read the downloaded catalog: {error}"))?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the downloaded catalog is not valid JSON: {error}"))?;
    let sequence = document
        .get("sequence")
        .and_then(Value::as_u64)
        .ok_or("the downloaded catalog has no sequence")?;
    log(format!("previous catalog sequence {sequence} verified"));
    Ok(())
}

fn cmd_discover(
    previous: &Path,
    out: &Path,
    withhold: &str,
    force: bool,
    github_output: Option<&Path>,
) -> Result<(), String> {
    let cfg = load_config()?;
    let held = withheld_set(&cfg.withheld, withhold);
    let token = gh_token();
    let prev = load_previous(Some(previous))?;
    let mut items = Vec::new();
    items.extend(discover_python(
        &cfg,
        package_config(&cfg, "python")?,
        &held,
        token.as_deref(),
    )?);
    items.extend(discover_php(&cfg, package_config(&cfg, "php")?, &held)?);
    items.extend(discover_go(&cfg, package_config(&cfg, "go")?, &held)?);
    items.extend(discover_node(&cfg, package_config(&cfg, "nodejs")?, &held)?);

    let prev_versions = previous_manifests(prev.as_ref());
    for item in &mut items {
        let key = format!("{}@{}", item.package, item.version);
        item.revision = cfg.revisions.get(&key).copied().unwrap_or(1);
        item.status = if is_carry(prev_versions.get(&key), item.revision, &item.targets, force) {
            "carry".into()
        } else {
            "new".into()
        };
    }
    let new_labels: Vec<String> = items
        .iter()
        .filter(|item| item.status == "new")
        .map(|item| format!("{} {}", item.package, item.version))
        .collect();
    let has_new = !new_labels.is_empty();
    let prev_sequence = prev
        .as_ref()
        .and_then(|document| document.get("sequence"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let expires = prev
        .as_ref()
        .and_then(|document| document.get("expiresAt"))
        .and_then(Value::as_str);
    let prev_truthy = truthy(prev.as_ref());
    let refresh = match (prev_truthy, expires) {
        (true, Some(expires)) => {
            let expires = parse_timestamp(expires)?;
            let window = TimeDelta::try_days(cfg.refresh_before_days)
                .ok_or("refreshBeforeDays is out of range")?;
            expires - now() < window
        }
        _ => false,
    };
    let current: BTreeSet<String> = items
        .iter()
        .map(|item| format!("{}@{}", item.package, item.version))
        .collect();
    let dropped: Vec<String> = prev_versions
        .keys()
        .filter(|key| !current.contains(*key))
        .cloned()
        .collect();

    let plan = Plan {
        generated_at: iso(now()),
        previous_sequence: prev_sequence,
        withheld: held.iter().cloned().collect(),
        items,
        dropped: dropped.clone(),
    };
    write_json(
        out,
        &serde_json::to_value(&plan).map_err(|e| e.to_string())?,
    )?;

    let verify_targets: BTreeSet<String> = plan
        .items
        .iter()
        .filter(|item| item.status == "new" && item.kind == "upstream")
        .flat_map(|item| item.targets.keys().cloned())
        .collect();
    let mut verify_include = Vec::new();
    for target in &verify_targets {
        verify_include.push(json!({
            "target": target,
            "runner": runner_for(&cfg, target)?,
        }));
    }
    let verify_matrix = json!({ "include": verify_include });
    let mut php_include = Vec::new();
    for item in plan
        .items
        .iter()
        .filter(|item| item.status == "new" && item.package == "php")
    {
        for target in item.targets.keys() {
            php_include.push(json!({
                "version": item.version,
                "target": target,
                "runner": runner_for(&cfg, target)?,
            }));
        }
    }
    let php_matrix = json!({ "include": php_include });
    let changed = has_new || !dropped.is_empty() || refresh || !prev_truthy;

    log(format!(
        "discovered {} versions; new: {}; dropped: {}; refresh: {refresh}",
        plan.items.len(),
        py_list_repr(&new_labels),
        py_list_repr(&dropped)
    ));

    if let Some(path) = github_output {
        let mut output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        let boolean = |value: bool| if value { "true" } else { "false" };
        writeln!(output, "changed={}", boolean(changed))
            .and_then(|()| writeln!(output, "has_new={}", boolean(has_new)))
            .and_then(|()| {
                writeln!(
                    output,
                    "verify_matrix={}",
                    serde_json::to_string(&verify_matrix).unwrap_or_default()
                )
            })
            .and_then(|()| {
                writeln!(
                    output,
                    "php_matrix={}",
                    serde_json::to_string(&php_matrix).unwrap_or_default()
                )
            })
            .and_then(|()| writeln!(output, "has_verify={}", boolean(!verify_targets.is_empty())))
            .and_then(|()| {
                let has_php = php_matrix
                    .get("include")
                    .and_then(Value::as_array)
                    .is_some_and(|include| !include.is_empty());
                writeln!(output, "has_php={}", boolean(has_php))
            })
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(())
}

// ------------------------------------------------------------------ verify

/// A private temporary directory, removed on drop.
struct WorkDir(PathBuf);

static WORK_COUNTER: AtomicU64 = AtomicU64::new(0);

impl WorkDir {
    fn new(prefix: &str) -> Result<Self, String> {
        let base = env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        for _ in 0..100 {
            let counter = WORK_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let path = base.join(format!("{prefix}{}-{nanos}-{counter}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(WorkDir(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("cannot create {}: {error}", path.display()));
                }
            }
        }
        Err("cannot create a temporary directory".into())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Captured {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

fn run_capture(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    environment: Option<&[(String, String)]>,
    timeout: Duration,
) -> Result<Captured, String> {
    let mut command = ProcessCommand::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(environment) = environment {
        command.env_clear();
        for (name, value) in environment {
            command.env(name, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    let mut stdout = child.stdout.take().ok_or("cannot capture stdout")?;
    let mut stderr = child.stderr.take().ok_or("cannot capture stderr")?;
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = stdout_sender.send(buffer);
    });
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        let _ = stderr_sender.send(buffer);
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let stdout = stdout_receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_default();
            let stderr = stderr_receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_default();
            return Ok(Captured {
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit_code: status.code().unwrap_or(-1),
            });
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} timed out after {}s",
                program.display(),
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn run_check(
    root: &Path,
    executable: &str,
    args: &[String],
    expect: &str,
    home: &Path,
) -> Result<String, String> {
    let environment = vec![
        (
            "PATH".to_string(),
            format!("{}/bin:/usr/local/bin:/usr/bin:/bin", root.display()),
        ),
        ("HOME".to_string(), home.display().to_string()),
        ("TMPDIR".to_string(), home.display().to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        // RAST runs under MemoryDenyWriteExecute; check the same way.
        ("NODE_OPTIONS".to_string(), "--jitless".to_string()),
        (
            "npm_config_cache".to_string(),
            home.join(".npm").display().to_string(),
        ),
        (
            "npm_config_update_notifier".to_string(),
            "false".to_string(),
        ),
    ];
    let captured = run_capture(
        &root.join(executable),
        args,
        Some(home),
        Some(&environment),
        Duration::from_secs(120),
    )?;
    let text = format!("{}{}", captured.stdout, captured.stderr)
        .trim()
        .to_string();
    if captured.exit_code != 0 {
        return Err(format!(
            "{executable} {} exited {}: {}",
            args.join(" "),
            captured.exit_code,
            truncate_chars(&text, 400)
        ));
    }
    if !text.contains(expect) {
        return Err(format!(
            "{executable} {} printed {:?}, expected it to contain {:?}",
            args.join(" "),
            truncate_chars(&text, 200),
            expect
        ));
    }
    Ok(text.lines().next().unwrap_or("").to_string())
}

fn node_shasums(version: &str, work: &Path) -> Result<BTreeMap<String, String>, String> {
    let base = format!("https://nodejs.org/dist/v{version}/");
    let sums_path = work.join(format!("node-{version}-SHASUMS256.txt"));
    let signature_path = work.join(format!("node-{version}-SHASUMS256.txt.sig"));
    let keyring_path = work.join("node-release-keys.kbx");
    fs::write(
        &sums_path,
        http_get(&format!("{base}SHASUMS256.txt"), None, None)?,
    )
    .map_err(|error| format!("cannot write {}: {error}", sums_path.display()))?;
    fs::write(
        &signature_path,
        http_get(&format!("{base}SHASUMS256.txt.sig"), None, None)?,
    )
    .map_err(|error| format!("cannot write {}: {error}", signature_path.display()))?;
    if !keyring_path.exists() {
        fs::write(&keyring_path, http_get(NODE_KEYRING, None, None)?)
            .map_err(|error| format!("cannot write {}: {error}", keyring_path.display()))?;
    }
    let args = vec![
        "--keyring".to_string(),
        keyring_path.display().to_string(),
        signature_path.display().to_string(),
        sums_path.display().to_string(),
    ];
    let captured = run_capture(
        Path::new("gpgv"),
        &args,
        None,
        None,
        Duration::from_secs(120),
    )?;
    if captured.exit_code != 0 {
        return Err(format!(
            "SHASUMS256.txt signature does not verify: {}",
            truncate_chars(captured.stderr.trim(), 300)
        ));
    }
    let text = fs::read_to_string(&sums_path)
        .map_err(|error| format!("cannot read {}: {error}", sums_path.display()))?;
    let mut sums = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 2 {
            sums.insert(parts[1].to_string(), parts[0].to_string());
        }
    }
    Ok(sums)
}

fn pbs_sums(
    url: &str,
    cache: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>, String> {
    if let Some(sums) = cache.get(url) {
        return Ok(sums.clone());
    }
    let bytes = http_get(url, None, None)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{url} is not UTF-8"))?;
    let mut sums = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 2 {
            sums.insert(parts[1].to_string(), parts[0].to_string());
        }
    }
    cache.insert(url.to_string(), sums.clone());
    Ok(sums)
}

fn optional_executables(package: &str) -> &'static [(&'static str, &'static str)] {
    match package {
        "nodejs" => &[("corepack", "bin/corepack")],
        _ => &[],
    }
}

struct VerifiedItem {
    sha256: String,
    size: u64,
    bundled: BTreeMap<String, String>,
    optional: BTreeSet<String>,
    checks: Vec<String>,
    verification: String,
}

fn verify_item(
    item: &PlanItem,
    target: &str,
    info: &TargetInfo,
    work: &Path,
    sums_cache: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> Result<VerifiedItem, String> {
    let archive_path = work.join("artifact.tar.gz");
    let (expected, verification) = match item.package.as_str() {
        "go" => (
            info.sha256.clone().ok_or_else(|| {
                format!(
                    "{} {} has no sha256 in the plan",
                    item.package, item.version
                )
            })?,
            "SHA-256 published in the go.dev/dl release index; downloaded and re-hashed in CI"
                .to_string(),
        ),
        "nodejs" => {
            let sums = node_shasums(&item.version, work)?;
            let file = info.file.as_deref().ok_or_else(|| {
                format!("nodejs {} has no artifact file in the plan", item.version)
            })?;
            let expected = sums
                .get(file)
                .cloned()
                .ok_or_else(|| format!("{file} is not listed in SHASUMS256.txt"))?;
            (
                expected,
                "SHA-256 from SHASUMS256.txt, whose signature was verified with gpgv against the nodejs/release-keys keyring".to_string(),
            )
        }
        "python" => {
            let url = item
                .sums_url
                .as_deref()
                .ok_or_else(|| format!("python {} has no sumsUrl in the plan", item.version))?;
            let sums = pbs_sums(url, sums_cache)?;
            let file = info.file.as_deref().ok_or_else(|| {
                format!("python {} has no artifact file in the plan", item.version)
            })?;
            let expected = sums
                .get(file)
                .cloned()
                .ok_or_else(|| format!("{file} is not listed in SHA256SUMS"))?;
            if let Some(digest) = info.digest.as_deref().filter(|digest| !digest.is_empty()) {
                if digest != expected {
                    return Err("GitHub asset digest differs from SHA256SUMS".into());
                }
            }
            (
                expected,
                "SHA-256 from the release SHA256SUMS, equal to the GitHub asset digest; GitHub build-provenance attestation verified with gh attestation verify".to_string(),
            )
        }
        _ => return Err("not an upstream package".into()),
    };
    let url = info
        .url
        .as_deref()
        .ok_or_else(|| format!("{target} has no artifact URL in the plan"))?;
    let (sha256, size) = download(url, &archive_path)?;
    if sha256 != expected {
        return Err(format!(
            "SHA-256 mismatch: expected {expected}, got {sha256}"
        ));
    }
    if let Some(expected_size) = info.size {
        if expected_size != size {
            return Err(format!(
                "size mismatch: expected {expected_size}, got {size}"
            ));
        }
    }
    if item.package == "python" {
        let captured = run_capture(
            Path::new("gh"),
            &[
                "attestation".to_string(),
                "verify".to_string(),
                archive_path.display().to_string(),
                "--repo".to_string(),
                PBS_REPO.to_string(),
            ],
            None,
            None,
            Duration::from_secs(120),
        )?;
        if captured.exit_code != 0 {
            let detail = if captured.stderr.trim().is_empty() {
                captured.stdout.trim()
            } else {
                captured.stderr.trim()
            };
            return Err(format!(
                "attestation verification failed: {}",
                truncate_chars(detail, 300)
            ));
        }
    }
    let root = work.join("root");
    if root.exists() {
        fs::remove_dir_all(&root)
            .map_err(|error| format!("cannot clear {}: {error}", root.display()))?;
    }
    fs::create_dir(&root).map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let mut progress = |_stats: &archive::ExtractStats| {};
    archive::extract_tar_gz(
        &archive_path,
        &root,
        1,
        archive::MAX_TOTAL_BYTES,
        &mut progress,
        &|| false,
    )?;
    let home = work.join("home");
    fs::create_dir_all(&home)
        .map_err(|error| format!("cannot create {}: {error}", home.display()))?;
    let mut bundled = BTreeMap::new();
    if item.package == "nodejs" {
        let npm = run_check(&root, "bin/npm", &["--version".to_string()], "", &home)?;
        let npm = npm.trim().to_string();
        if let Some(index_npm) = &item.index_npm {
            if index_npm != &npm {
                log(format!(
                    "note: index.json says npm {index_npm}, the archive ships {npm}"
                ));
            }
        }
        bundled.insert("npm".to_string(), npm);
    }
    if item.package == "python" {
        let pip = run_check(&root, "bin/pip3", &["--version".to_string()], "pip ", &home)?;
        let version = pip
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| format!("bin/pip3 --version printed {pip:?}"))?
            .to_string();
        bundled.insert("pip".to_string(), version);
    }
    let mut optional = BTreeSet::new();
    for (name, relative) in optional_executables(&item.package) {
        if root.join(relative).exists() {
            optional.insert((*name).to_string());
        }
    }
    let spec = runtime_spec(&item.package, &item.version, &bundled, &optional)?;
    for executable in &spec.executables {
        if !is_executable_file(&root.join(&executable.path)) {
            return Err(format!("executable {} is missing", executable.path));
        }
    }
    let mut checks = Vec::new();
    for check in &spec.checks {
        let executable = spec
            .executables
            .iter()
            .find(|executable| executable.name == check.executable)
            .ok_or_else(|| format!("check uses unknown executable {:?}", check.executable))?;
        checks.push(run_check(
            &root,
            &executable.path,
            &check.args,
            &check.expect,
            &home,
        )?);
    }
    Ok(VerifiedItem {
        sha256,
        size,
        bundled,
        optional,
        checks,
        verification,
    })
}

fn cmd_verify(plan_path: &Path, target: &str, out: &Path) -> Result<(), String> {
    let plan = read_plan(plan_path)?;
    let work = WorkDir::new("rast-verify-")?;
    let mut sums_cache: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut results = Vec::new();
    for item in &plan.items {
        if item.status != "new" || item.kind != "upstream" {
            continue;
        }
        let Some(info) = item.targets.get(target) else {
            continue;
        };
        let label = format!("{} {} {target}", item.package, item.version);
        log(format!("== {label}"));
        let mut result = Map::new();
        result.insert("package".into(), json!(item.package));
        result.insert("version".into(), json!(item.version));
        result.insert("target".into(), json!(target));
        result.insert("ok".into(), json!(false));
        match verify_item(item, target, info, work.path(), &mut sums_cache) {
            Ok(verified) => {
                result.insert("ok".into(), json!(true));
                result.insert("sha256".into(), json!(verified.sha256));
                result.insert("size".into(), json!(verified.size));
                result.insert(
                    "bundled".into(),
                    serde_json::to_value(&verified.bundled).unwrap_or(Value::Null),
                );
                result.insert(
                    "optional".into(),
                    serde_json::to_value(&verified.optional).unwrap_or(Value::Null),
                );
                result.insert("checks".into(), json!(verified.checks));
                result.insert("verification".into(), json!(verified.verification));
                log(format!("   ok: {:?}", verified.checks));
            }
            Err(error) => {
                let error = truncate_chars(&error, 500);
                result.insert("error".into(), json!(error));
                log(format!("   FAILED: {error}"));
            }
        }
        results.push(Value::Object(result));
        let _ = fs::remove_dir_all(work.path().join("root"));
        let _ = fs::remove_file(work.path().join("artifact.tar.gz"));
    }
    write_json(out, &Value::Array(results))
}

// --------------------------------------------------------------- php-result

fn run_php_modules(php: &Path) -> Result<String, String> {
    let captured = run_capture(
        php,
        &["-m".to_string()],
        None,
        None,
        Duration::from_secs(60),
    )?;
    if captured.exit_code != 0 {
        return Err(format!(
            "bin/php -m exited {}: {}",
            captured.exit_code,
            truncate_chars(
                format!("{}{}", captured.stdout, captured.stderr).trim(),
                400
            )
        ));
    }
    Ok(captured.stdout)
}

fn php_result_fields(
    archive_path: &Path,
    version: &str,
    attestation_url: &str,
    work: &Path,
) -> Result<Map<String, Value>, String> {
    let root = work.join("root");
    fs::create_dir(&root).map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let mut progress = |_stats: &archive::ExtractStats| {};
    archive::extract_tar_gz(
        archive_path,
        &root,
        1,
        archive::MAX_TOTAL_BYTES,
        &mut progress,
        &|| false,
    )?;
    let home = work.join("home");
    fs::create_dir(&home).map_err(|error| format!("cannot create {}: {error}", home.display()))?;
    let first = run_check(
        &root,
        "bin/php",
        &["-v".to_string()],
        &format!("PHP {version}"),
        &home,
    )?;
    let modules = run_php_modules(&root.join("bin/php"))?;
    let extensions: BTreeSet<String> = modules
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('['))
        .map(|line| line.trim().to_ascii_lowercase())
        .collect();
    let buildinfo_bytes = fs::read(root.join("BUILDINFO.json"))
        .map_err(|error| format!("BUILDINFO.json: {error}"))?;
    let buildinfo: Value = serde_json::from_slice(&buildinfo_bytes)
        .map_err(|error| format!("BUILDINFO.json is not valid JSON: {error}"))?;
    let sha256 = authoring::sha256_file(archive_path)?;
    let size = fs::metadata(archive_path)
        .map_err(|error| format!("cannot inspect the archive: {error}"))?
        .len();
    let file = archive_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or("the archive has no file name")?;
    let run_url = run_url();
    let attestation = if attestation_url.is_empty() {
        Value::Null
    } else {
        json!(attestation_url)
    };
    let build_url = run_url.map_or(Value::Null, |url| json!(url));

    let mut fields = Map::new();
    fields.insert("sha256".into(), json!(sha256));
    fields.insert("size".into(), json!(size));
    fields.insert("checks".into(), json!([first]));
    fields.insert(
        "extensions".into(),
        serde_json::to_value(&extensions).unwrap_or(Value::Null),
    );
    fields.insert("attestationUrl".into(), attestation);
    fields.insert("buildUrl".into(), build_url);
    fields.insert("buildinfo".into(), buildinfo);
    fields.insert("file".into(), json!(file));
    Ok(fields)
}

fn cmd_php_result(
    archive_path: &Path,
    version: &str,
    target: &str,
    attestation_url: &str,
    out: &Path,
) -> Result<(), String> {
    let work = WorkDir::new("rast-php-")?;
    let mut result = Map::new();
    result.insert("package".into(), json!("php"));
    result.insert("version".into(), json!(version));
    result.insert("target".into(), json!(target));
    result.insert("ok".into(), json!(false));
    match php_result_fields(archive_path, version, attestation_url, work.path()) {
        Ok(fields) => {
            result.insert("ok".into(), json!(true));
            for (name, value) in fields {
                result.insert(name, value);
            }
        }
        Err(error) => {
            result.insert("error".into(), json!(truncate_chars(&error, 500)));
        }
    }
    write_json(out, &Value::Object(result.clone()))?;
    if result.get("ok") != Some(&Value::Bool(true)) {
        return Err(format!(
            "PHP smoke test failed: {}",
            result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- assemble

/// Executables and version checks per runtime (the checks RAST re-runs).
fn runtime_spec(
    package: &str,
    version: &str,
    bundled: &BTreeMap<String, String>,
    optional: &BTreeSet<String>,
) -> Result<manifest::RuntimeSpec, String> {
    let mut executables = Vec::new();
    let mut checks = Vec::new();
    match package {
        "python" => {
            let major_minor = version.split('.').take(2).collect::<Vec<_>>().join(".");
            executables.push(manifest::Executable {
                name: "python3".into(),
                path: "bin/python3".into(),
            });
            executables.push(manifest::Executable {
                name: format!("python{major_minor}"),
                path: format!("bin/python{major_minor}"),
            });
            executables.push(manifest::Executable {
                name: "pip3".into(),
                path: "bin/pip3".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "python3".into(),
                args: vec!["--version".into()],
                expect: format!("Python {version}"),
                label: None,
            });
            if let Some(pip) = bundled.get("pip") {
                checks.push(manifest::VersionCheck {
                    executable: "pip3".into(),
                    args: vec!["--version".into()],
                    expect: format!("pip {pip}"),
                    label: Some("pip".into()),
                });
            }
        }
        "nodejs" => {
            executables.push(manifest::Executable {
                name: "node".into(),
                path: "bin/node".into(),
            });
            executables.push(manifest::Executable {
                name: "npm".into(),
                path: "bin/npm".into(),
            });
            executables.push(manifest::Executable {
                name: "npx".into(),
                path: "bin/npx".into(),
            });
            if optional.contains("corepack") {
                executables.push(manifest::Executable {
                    name: "corepack".into(),
                    path: "bin/corepack".into(),
                });
            }
            checks.push(manifest::VersionCheck {
                executable: "node".into(),
                args: vec!["--version".into()],
                expect: format!("v{version}"),
                label: None,
            });
            if let Some(npm) = bundled.get("npm") {
                checks.push(manifest::VersionCheck {
                    executable: "npm".into(),
                    args: vec!["--version".into()],
                    expect: npm.clone(),
                    label: Some("npm".into()),
                });
            }
        }
        "go" => {
            executables.push(manifest::Executable {
                name: "go".into(),
                path: "bin/go".into(),
            });
            executables.push(manifest::Executable {
                name: "gofmt".into(),
                path: "bin/gofmt".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "go".into(),
                args: vec!["version".into()],
                expect: format!("go{version}"),
                label: None,
            });
        }
        "php" => {
            executables.push(manifest::Executable {
                name: "php".into(),
                path: "bin/php".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "php".into(),
                args: vec!["-v".into()],
                expect: format!("PHP {version}"),
                label: None,
            });
        }
        other => return Err(format!("unknown package {other:?}")),
    }
    let bundled = bundled
        .iter()
        .map(|(name, version)| manifest::Bundled {
            name: name.clone(),
            version: version.clone(),
        })
        .collect();
    Ok(manifest::RuntimeSpec {
        executables,
        checks,
        bundled,
        extensions: Vec::new(),
    })
}

fn base_manifest(cfg: &Config, item: &PlanItem) -> Result<Value, String> {
    let package = package_config(cfg, &item.package)?;
    let mut manifest = Map::new();
    manifest.insert("schemaVersion".into(), json!(1));
    manifest.insert("kind".into(), json!("rast-package"));
    manifest.insert("id".into(), json!(item.package));
    manifest.insert("type".into(), json!("runtime"));
    manifest.insert("name".into(), json!(package.name));
    manifest.insert("description".into(), json!(package.description));
    manifest.insert("homepage".into(), json!(package.homepage));
    manifest.insert("license".into(), json!(package.license));
    manifest.insert(
        "publisher".into(),
        serde_json::to_value(&cfg.publisher).map_err(|error| error.to_string())?,
    );
    manifest.insert("version".into(), json!(item.version));
    manifest.insert("revision".into(), json!(item.revision));
    manifest.insert("minRastVersion".into(), json!(cfg.min_rast_version));
    if let Some(channel) =
        channel_of(&item.package, &item.version).filter(|value| !value.is_empty())
    {
        manifest.insert("channel".into(), json!(channel));
    }
    if let Some(release_date) = item
        .release_date
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        manifest.insert(
            "releaseDate".into(),
            json!(truncate_chars(release_date, 32)),
        );
    }
    Ok(Value::Object(manifest))
}

fn build_manifest(
    cfg: &Config,
    item: &PlanItem,
    results: &[Outcome],
    php_release: Option<&str>,
) -> Result<(Option<Value>, String), String> {
    let ok: Vec<&Outcome> = results.iter().filter(|result| result.ok).collect();
    let failed: Vec<&Outcome> = results.iter().filter(|result| !result.ok).collect();
    if ok.is_empty() {
        let mut details: Vec<String> = failed
            .iter()
            .map(|result| {
                format!(
                    "{}: {}",
                    result.target,
                    result.error.as_deref().unwrap_or("no result")
                )
            })
            .collect();
        if details.is_empty() {
            details.push("no target produced a verified artifact".into());
        }
        return Ok((None, details.join("; ")));
    }

    let mut bundled: BTreeMap<String, String> = BTreeMap::new();
    for result in &ok {
        for (name, version) in &result.bundled {
            bundled.insert(name.clone(), version.clone());
        }
    }
    let optional: BTreeSet<String> = ok
        .first()
        .map(|first| {
            first
                .optional
                .iter()
                .filter(|name| ok.iter().all(|result| result.optional.contains(*name)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let base = base_manifest(cfg, item)?;
    let mut manifest = base
        .as_object()
        .cloned()
        .ok_or("the generated manifest is not a JSON object")?;
    let mut spec = runtime_spec(&item.package, &item.version, &bundled, &optional)?;
    let extensions: BTreeSet<String> = ok
        .iter()
        .flat_map(|result| result.extensions.iter().cloned())
        .collect();
    if !extensions.is_empty() {
        spec.extensions = extensions.iter().cloned().collect();
    }
    manifest.insert(
        "runtime".into(),
        serde_json::to_value(&spec).map_err(|error| error.to_string())?,
    );

    let mut sorted = ok;
    sorted.sort_by(|a, b| a.target.cmp(&b.target));
    let mut artifacts = Vec::new();
    for result in sorted {
        let (url, provenance, requires) = if item.package == "php" {
            let file = result
                .file
                .as_deref()
                .ok_or_else(|| format!("php {} result has no file name", item.version))?;
            let release = php_release.unwrap_or("None");
            let buildinfo = result
                .buildinfo
                .as_ref()
                .ok_or_else(|| format!("php {} result has no buildinfo", item.version))?;
            let spc = buildinfo
                .get("spcVersion")
                .and_then(Value::as_str)
                .unwrap_or("None");
            let builder = buildinfo
                .get("builder")
                .and_then(Value::as_str)
                .unwrap_or("None");
            let source = item
                .source
                .as_ref()
                .ok_or_else(|| format!("php {} has no source in the plan", item.version))?;
            let source_url = required_str(source, "url")?.to_string();
            let source_sha256 = required_str(source, "sha256")?.to_string();
            let mut provenance = Map::new();
            provenance.insert("kind".into(), json!("built"));
            provenance.insert("source".into(), json!(source_url));
            provenance.insert("sourceSha256".into(), json!(source_sha256));
            provenance.insert(
                "verification".into(),
                json!(format!(
                    "php.net source SHA-256 checked before the build; static-php-cli {spc} verified by GitHub asset digest; built in {builder}"
                )),
            );
            if let Some(build_url) = result.build_url.as_deref() {
                provenance.insert("buildUrl".into(), json!(build_url));
            }
            if let Some(attestation_url) = result.attestation_url.as_deref() {
                provenance.insert("attestationUrl".into(), json!(attestation_url));
            }
            (
                format!("{}/releases/download/{release}/{file}", cfg.repository),
                Value::Object(provenance),
                json!({
                    "notes": "Static musl binary: no host libraries needed. TLS uses the host CA bundle (/etc/ssl/certs)."
                }),
            )
        } else {
            let info = item.targets.get(&result.target).ok_or_else(|| {
                format!(
                    "{} {} has no plan target {}",
                    item.package, item.version, result.target
                )
            })?;
            let url = info.url.as_deref().ok_or_else(|| {
                format!(
                    "{} {} target {} has no URL",
                    item.package, item.version, result.target
                )
            })?;
            let source = info.source.as_deref().ok_or_else(|| {
                format!(
                    "{} {} target {} has no source",
                    item.package, item.version, result.target
                )
            })?;
            let verification = result.verification.as_deref().ok_or_else(|| {
                format!(
                    "{} {} target {} result has no verification",
                    item.package, item.version, result.target
                )
            })?;
            let mut provenance = Map::new();
            provenance.insert("kind".into(), json!("upstream"));
            provenance.insert("source".into(), json!(source));
            provenance.insert("verification".into(), json!(verification));
            if let Some(run_url) = run_url() {
                provenance.insert("buildUrl".into(), json!(run_url));
            }
            let requires = match item.package.as_str() {
                "python" => json!({
                    "glibc": "2.17",
                    "notes": format!(
                        "python-build-standalone {} (x86_64/aarch64 gnu install_only).",
                        item.build_tag.as_deref().unwrap_or("None")
                    ),
                }),
                "nodejs" => json!({ "glibc": "2.28" }),
                "go" => json!({
                    "notes": "Statically linked toolchain; building cgo programs needs a host C compiler."
                }),
                _ => json!({}),
            };
            (url.to_string(), Value::Object(provenance), requires)
        };
        let sha256 = result
            .sha256
            .clone()
            .ok_or_else(|| format!("{} result has no sha256", result.target))?;
        let size = result
            .size
            .ok_or_else(|| format!("{} result has no size", result.target))?;
        let mut artifact = Map::new();
        artifact.insert("target".into(), json!(result.target));
        artifact.insert("url".into(), json!(url));
        artifact.insert("sha256".into(), json!(sha256));
        artifact.insert("size".into(), json!(size));
        artifact.insert("format".into(), json!("tar.gz"));
        artifact.insert("stripComponents".into(), json!(1));
        artifact.insert("provenance".into(), provenance);
        if requires
            .as_object()
            .is_some_and(|object| !object.is_empty())
        {
            artifact.insert("requires".into(), requires);
        }
        artifacts.push(Value::Object(artifact));
    }
    manifest.insert("artifacts".into(), Value::Array(artifacts));

    let mut notes = Vec::new();
    if !failed.is_empty() {
        let list: Vec<String> = failed
            .iter()
            .map(|result| {
                format!(
                    "{} ({})",
                    result.target,
                    truncate_chars(result.error.as_deref().unwrap_or("failed"), 80)
                )
            })
            .collect();
        notes.push(format!("Not available for: {}.", list.join(", ")));
    }
    if item.package == "nodejs" {
        notes.push(format!(
            "Bundled npm {} as found in the archive.",
            bundled.get("npm").map(String::as_str).unwrap_or("?")
        ));
    }
    if item.package == "php" {
        notes.push("Extensions: see the list; php.ini is not shipped (PHP defaults apply).".into());
    }
    if !notes.is_empty() {
        manifest.insert(
            "notes".into(),
            json!(truncate_chars(&notes.join(" "), 1000)),
        );
    }

    let bytes =
        serde_json::to_vec(&Value::Object(manifest.clone())).map_err(|error| error.to_string())?;
    let parsed = manifest::parse(&bytes)?;
    manifest::validate(&parsed)?;
    Ok((Some(Value::Object(manifest)), String::new()))
}

fn package_entries(cfg: &Config) -> Vec<(String, Value)> {
    cfg.packages
        .entries
        .iter()
        .map(|(id, package)| {
            let entry = json!({
                "id": id,
                "type": "runtime",
                "name": package.name,
                "description": package.description,
                "homepage": package.homepage,
                "license": package.license,
                "channels": package.channels,
                "versions": [],
                "detected": [],
            });
            (id.clone(), entry)
        })
        .collect()
}

fn push_version(packages: &mut [(String, Value)], version: &Value) -> Result<(), String> {
    let id = version
        .get("id")
        .and_then(Value::as_str)
        .ok_or("a previous manifest has no id")?;
    let entry = packages
        .iter_mut()
        .find(|(package_id, _)| package_id == id)
        .ok_or_else(|| format!("catalog package {id:?} is not configured"))?;
    let versions = entry
        .1
        .get_mut("versions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("package {id:?} has no versions list"))?;
    versions.push(version.clone());
    Ok(())
}

fn push_detected(
    packages: &mut [(String, Value)],
    package: &str,
    detected: Value,
) -> Result<(), String> {
    let entry = packages
        .iter_mut()
        .find(|(package_id, _)| package_id == package)
        .ok_or_else(|| format!("catalog package {package:?} is not configured"))?;
    let detected_list = entry
        .1
        .get_mut("detected")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("package {package:?} has no detected list"))?;
    detected_list.push(detected);
    Ok(())
}

fn sort_versions(packages: &mut [(String, Value)]) -> Result<(), String> {
    for (id, entry) in packages.iter_mut() {
        let versions = entry
            .get_mut("versions")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("package {id:?} has no versions list"))?;
        versions.sort_by(|a, b| compare_manifest_order(a, b).reverse());
    }
    Ok(())
}

fn catalog_doc(
    cfg: &Config,
    sequence: u64,
    packages: &mut [(String, Value)],
) -> Result<Value, String> {
    sort_versions(packages)?;
    let stamp = now();
    let expires =
        stamp + TimeDelta::try_days(cfg.expires_days).ok_or("expiresDays is out of range")?;
    let list: Vec<Value> = packages
        .iter()
        .map(|(_, package)| package.clone())
        .collect();
    Ok(json!({
        "schemaVersion": 1,
        "kind": "rast-catalog",
        "name": cfg.name,
        "publisher": cfg.publisher,
        "repository": cfg.repository,
        "sequence": sequence,
        "generatedAt": iso(stamp),
        "expiresAt": iso(expires),
        "allowedHosts": cfg.allowed_hosts,
        "packages": list,
    }))
}

fn carried_manifests(previous: Option<&Value>, keep: &BTreeSet<String>) -> BTreeMap<String, Value> {
    previous_manifests(previous)
        .into_iter()
        .filter(|(key, _)| keep.contains(key))
        .collect()
}

fn carry_keys(plan: &Plan) -> BTreeSet<String> {
    plan.items
        .iter()
        .filter(|item| item.status == "carry")
        .map(|item| format!("{}@{}", item.package, item.version))
        .collect()
}

fn channel_for_item(item: &PlanItem) -> String {
    channel_of(&item.package, &item.version)
        .filter(|channel| !channel.is_empty())
        .unwrap_or_else(|| item.channel.clone())
}

fn detected_entry(version: &str, channel: &str, kind: &str, stamp: &str, run_note: &str) -> Value {
    let (state, detail) = if kind == "built" {
        (
            "building",
            format!(
                "New upstream release; the catalog workflow is building and testing it{run_note}."
            ),
        )
    } else {
        (
            "verifying",
            format!(
                "New upstream release; the catalog workflow is verifying checksums and running it{run_note}."
            ),
        )
    };
    json!({
        "version": version,
        "channel": channel,
        "detectedAt": stamp,
        "state": state,
        "detail": detail,
    })
}

fn next_sequence(previous: u64, has_new: bool) -> u64 {
    previous + if has_new { 2 } else { 1 }
}

fn cmd_interim(plan_path: &Path, previous: &Path, out: &Path) -> Result<(), String> {
    let cfg = load_config()?;
    interim_with_config(&cfg, plan_path, previous, out)
}

fn interim_with_config(
    cfg: &Config,
    plan_path: &Path,
    previous: &Path,
    out: &Path,
) -> Result<(), String> {
    let plan = read_plan(plan_path)?;
    let prev = load_previous(Some(previous))?;
    let mut packages = package_entries(cfg);
    let keep = carry_keys(&plan);
    for manifest in carried_manifests(prev.as_ref(), &keep).values() {
        push_version(&mut packages, manifest)?;
    }
    // Versions that are no longer selected stay until the final catalog.
    if let Some(prev) = &prev {
        let dropped: BTreeSet<&str> = plan.dropped.iter().map(String::as_str).collect();
        if let Some(previous_packages) = prev.get("packages").and_then(Value::as_array) {
            for package in previous_packages {
                if let Some(versions) = package.get("versions").and_then(Value::as_array) {
                    for manifest in versions {
                        if let Some(key) = manifest_key(manifest) {
                            if dropped.contains(key.as_str()) {
                                push_version(&mut packages, manifest)?;
                            }
                        }
                    }
                }
            }
        }
    }
    // A version that is only being re-verified keeps its previous manifest and
    // is not announced again.
    let before = previous_manifests(prev.as_ref());
    let stamp = iso(now());
    let note = run_note(run_url().as_deref());
    for item in &plan.items {
        if item.status != "new" {
            continue;
        }
        let key = format!("{}@{}", item.package, item.version);
        if let Some(old) = before.get(&key) {
            push_version(&mut packages, old)?;
        } else {
            let detected = detected_entry(
                &item.version,
                &channel_for_item(item),
                &item.kind,
                &stamp,
                &note,
            );
            push_detected(&mut packages, &item.package, detected)?;
        }
    }
    let doc = catalog_doc(cfg, plan.previous_sequence + 1, &mut packages)?;
    write_json(out, &doc)
}

fn load_results(directory: &Path) -> Result<Vec<Value>, String> {
    let mut files = Vec::new();
    if directory.exists() {
        for entry in WalkDir::new(directory) {
            let entry =
                entry.map_err(|error| format!("cannot walk {}: {error}", directory.display()))?;
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            {
                files.push(entry.path().to_path_buf());
            }
        }
    }
    files.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    let mut results = Vec::new();
    for file in files {
        let bytes =
            fs::read(&file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let data: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{} is not valid JSON: {error}", file.display()))?;
        match data {
            Value::Array(items) => results.extend(items),
            other => results.push(other),
        }
    }
    Ok(results)
}

fn load_php_releases(argument: Option<&Path>) -> Result<Value, String> {
    let Some(argument) = argument else {
        return Ok(json!({}));
    };
    // The workflow passes the JSON object itself (as the Python script did);
    // a path to a JSON file is accepted too so the CLI can be used from a
    // shell without quoting tricks.
    let text = if argument.is_file() {
        fs::read_to_string(argument)
            .map_err(|error| format!("cannot read {}: {error}", argument.display()))?
    } else {
        argument.to_string_lossy().into_owned()
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("--php-releases is not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err("--php-releases must be a JSON object".into());
    }
    Ok(value)
}

fn cmd_assemble(
    plan_path: &Path,
    previous: &Path,
    results_path: &Path,
    php_releases_path: Option<&Path>,
    out: &Path,
) -> Result<(), String> {
    let cfg = load_config()?;
    assemble_with_config(
        &cfg,
        plan_path,
        previous,
        results_path,
        php_releases_path,
        out,
    )
}

fn assemble_with_config(
    cfg: &Config,
    plan_path: &Path,
    previous: &Path,
    results_path: &Path,
    php_releases_path: Option<&Path>,
    out: &Path,
) -> Result<(), String> {
    let plan = read_plan(plan_path)?;
    let prev = load_previous(Some(previous))?;
    let raw_results = load_results(results_path)?;
    let outcomes: Vec<Outcome> = raw_results
        .iter()
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid result entry: {error}"))
        })
        .collect::<Result<_, _>>()?;
    let php_releases = load_php_releases(php_releases_path)?;
    let mut packages = package_entries(cfg);
    let keep = carry_keys(&plan);
    for manifest in carried_manifests(prev.as_ref(), &keep).values() {
        push_version(&mut packages, manifest)?;
    }
    let before = previous_manifests(prev.as_ref());
    let stamp = iso(now());
    let mut summary = Vec::new();
    for item in plan.items.iter().filter(|item| item.status == "new") {
        let key = format!("{}@{}", item.package, item.version);
        let mut mine: Vec<Outcome> = outcomes
            .iter()
            .filter(|result| result.package == item.package && result.version == item.version)
            .cloned()
            .collect();
        for target in item.targets.keys() {
            if !mine.iter().any(|result| &result.target == target) {
                mine.push(Outcome {
                    package: item.package.clone(),
                    version: item.version.clone(),
                    target: target.clone(),
                    ok: false,
                    error: Some("no result (job failed or was skipped)".into()),
                    ..Outcome::default()
                });
            }
        }
        let php_release = php_releases.get(&item.version).and_then(Value::as_str);
        let (manifest, detail) = build_manifest(cfg, item, &mine, php_release)?;
        if let Some(old) = before.get(&key) {
            let old_targets = artifact_targets(old);
            let covers = manifest
                .as_ref()
                .map(|manifest| artifact_targets(manifest).is_superset(&old_targets))
                .unwrap_or(false);
            if !covers {
                // Re-verification must not take away a version that was
                // already verified: keep the previous manifest.
                push_version(&mut packages, old)?;
                let reason = if detail.is_empty() {
                    "fewer targets verified"
                } else {
                    detail.as_str()
                };
                summary.push(format!(
                    "kept {} {} from the previous catalog ({reason})",
                    item.package, item.version
                ));
                continue;
            }
        }
        if let Some(manifest) = manifest {
            let targets = artifact_targets(&manifest).into_iter().collect::<Vec<_>>();
            push_version(&mut packages, &manifest)?;
            summary.push(format!(
                "verified {} {} ({})",
                item.package,
                item.version,
                targets.join(", ")
            ));
        } else {
            let detected = json!({
                "version": item.version,
                "channel": channel_for_item(item),
                "detectedAt": stamp,
                "state": "failed",
                "detail": truncate_chars(&format!("Verification failed: {detail}"), 500),
            });
            push_detected(&mut packages, &item.package, detected)?;
            summary.push(format!(
                "FAILED {} {}: {detail}",
                item.package, item.version
            ));
        }
    }
    let has_new = plan.items.iter().any(|item| item.status == "new");
    let sequence = next_sequence(plan.previous_sequence, has_new);
    let doc = catalog_doc(cfg, sequence, &mut packages)?;
    let problems = catalog_problems(&doc)?;
    if !problems.is_empty() {
        return Err(format!(
            "catalog has invalid versions:\n{}",
            problems.join("\n")
        ));
    }
    write_json(out, &doc)?;
    log(if summary.is_empty() {
        "no new versions".to_string()
    } else {
        summary.join("\n")
    });
    if let Some(summary_path) = env_nonempty("GITHUB_STEP_SUMMARY") {
        let mut body = format!("### Catalog sequence {sequence}\n\n");
        body.push_str(
            &summary
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        body.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary_path)
            .map_err(|error| format!("cannot write {summary_path}: {error}"))?;
        file.write_all(body.as_bytes())
            .map_err(|error| format!("cannot write {summary_path}: {error}"))?;
    }
    Ok(())
}

// ----------------------------------------------------------------- signing

fn cmd_sign(catalog: &Path) -> Result<(), String> {
    let cfg = load_config()?;
    let key_pem = env::var("CATALOG_SIGNING_KEY").unwrap_or_default();
    if !key_pem.contains("PRIVATE KEY") {
        return Err("CATALOG_SIGNING_KEY is not set".into());
    }
    let key_pair = ed25519_key_pair(&key_pem)?;
    let catalog_bytes =
        fs::read(catalog).map_err(|error| format!("cannot read {}: {error}", catalog.display()))?;
    let signature = key_pair.sign(&catalog_bytes);
    let signature_file = json!({
        "keyId": cfg.key_id,
        "algorithm": "ed25519",
        "signature": B64.encode(signature.as_ref()),
    });
    let signature_path = append_suffix(catalog, ".sig");
    let mut text = serde_json::to_string(&signature_file)
        .map_err(|error| format!("cannot serialize the signature: {error}"))?;
    text.push('\n');
    fs::write(&signature_path, text)
        .map_err(|error| format!("cannot write {}: {error}", signature_path.display()))?;
    verify_signature_files(
        catalog,
        &signature_path,
        Path::new(&cfg.public_key),
        &cfg.key_id,
    )?;
    log(format!("signed {} with {}", catalog.display(), cfg.key_id));
    Ok(())
}

// ------------------------------------------------------------------- check

/// Validate a catalog document exactly like the reference: schema envelope,
/// sequence, every embedded manifest and every per-version problem.
fn catalog_problems(document: &Value) -> Result<Vec<String>, String> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| format!("the catalog is not serializable: {error}"))?;
    let parsed = catalog::parse(&bytes)?;
    if parsed.sequence < 1 {
        return Err("sequence must be a positive integer".into());
    }
    let mut problems = Vec::new();
    for package in &parsed.packages {
        for manifest in &package.versions {
            if let Err(error) = manifest::validate(manifest) {
                problems.push(format!("{} {}: {error}", package.id, manifest.version));
            }
        }
        for unsupported in &package.unsupported {
            problems.push(format!(
                "{} {}: {}",
                package.id, unsupported.version, unsupported.reason
            ));
        }
    }
    Ok(problems)
}

/// Where a download URL ultimately resolves: an `https://` URL whose host is
/// one of the catalog's `allowedHosts`.
fn final_host_allowed(url: &str, final_url: &str, hosts: &BTreeSet<String>) -> Result<(), String> {
    let host = net::https_host(final_url)
        .map_err(|error| format!("{url} redirects to {final_url}: {error}"))?;
    if !hosts.contains(&host) {
        return Err(format!(
            "{url} redirects to {final_url}, outside allowedHosts"
        ));
    }
    Ok(())
}

fn cmd_check(
    catalog_path: &Path,
    previous: Option<&Path>,
    download_mode: Option<DownloadMode>,
) -> Result<(), String> {
    let cfg = load_config()?;
    let bytes = fs::read(catalog_path)
        .map_err(|error| format!("cannot read {}: {error}", catalog_path.display()))?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", catalog_path.display()))?;
    let problems = catalog_problems(&document)?;
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let signature_path = append_suffix(catalog_path, ".sig");
    if signature_path.exists() {
        let signature = fs::read_to_string(&signature_path)
            .map_err(|error| format!("cannot read {}: {error}", signature_path.display()))?;
        let public_key = fs::read_to_string(Path::new(&cfg.public_key))
            .map_err(|error| format!("cannot read {}: {error}", cfg.public_key))?;
        verify_signature_bytes(&bytes, &signature, &public_key, &cfg.key_id)?;
    }
    let prev = load_previous(previous)?;
    let sequence = document
        .get("sequence")
        .and_then(Value::as_u64)
        .ok_or("the catalog has no sequence")?;
    if let Some(prev) = prev.as_ref().filter(|prev| truthy(Some(prev))) {
        let published = prev
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or("the published catalog has no sequence")?;
        if sequence <= published {
            return Err(format!(
                "sequence {sequence} must be greater than the published {published}"
            ));
        }
    }
    if let Some(mode) = download_mode {
        // Every artifact URL must resolve (redirects inside allowedHosts) to
        // the pinned bytes.
        let hosts: BTreeSet<String> = document
            .get("allowedHosts")
            .and_then(Value::as_array)
            .map(|hosts| {
                hosts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let work = WorkDir::new("rast-check-")?;
        if let Some(packages) = document.get("packages").and_then(Value::as_array) {
            for package in packages {
                let Some(versions) = package.get("versions").and_then(Value::as_array) else {
                    continue;
                };
                for manifest in versions {
                    let Some(artifacts) = manifest.get("artifacts").and_then(Value::as_array)
                    else {
                        continue;
                    };
                    for artifact in artifacts {
                        let url = required_str(artifact, "url")?;
                        if mode == DownloadMode::New && !url.starts_with(&cfg.repository) {
                            continue;
                        }
                        let final_url = head_final_url(url)?;
                        final_host_allowed(url, &final_url, &hosts)?;
                        let (sha256, size) = download(url, &work.path().join("a"))?;
                        let expected_sha256 = required_str(artifact, "sha256")?;
                        let expected_size = artifact
                            .get("size")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| format!("{url} has no size in the manifest"))?;
                        if sha256 != expected_sha256 || size != expected_size {
                            return Err(format!("{url}: published bytes differ from the manifest"));
                        }
                        log(format!("ok {url}"));
                    }
                }
            }
        }
    }
    let count = document
        .get("packages")
        .and_then(Value::as_array)
        .map(|packages| {
            packages
                .iter()
                .map(|package| {
                    package
                        .get("versions")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    log(format!(
        "catalog sequence {sequence}: {count} versions valid"
    ));
    Ok(())
}

// ------------------------------------------------------------------ results

/// One entry of `results/**/*.json`, as written by [`Command::Verify`] and
/// [`Command::PhpResult`]. Unknown fields are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    #[serde(default)]
    package: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    bundled: BTreeMap<String, String>,
    #[serde(default)]
    optional: Vec<String>,
    #[serde(default)]
    verification: Option<String>,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    attestation_url: Option<String>,
    #[serde(default)]
    build_url: Option<String>,
    #[serde(default)]
    buildinfo: Option<Value>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

// ------------------------------------------------- workflow helper commands

/// Append `KEY=VALUE` lines to the GitHub Actions output file.
fn append_github_output(path: Option<&Path>, lines: &[String]) -> Result<(), String> {
    let target = match path {
        Some(path) => Some(path.to_path_buf()),
        None => env::var("GITHUB_OUTPUT").ok().map(PathBuf::from),
    };
    if let Some(target) = target {
        let mut text = fs::read_to_string(&target).unwrap_or_default();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&target, text)
            .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<u64, String> {
    let mut copied = 0u64;
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| error.to_string())?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            fs::copy(entry.path(), &target).map_err(|error| {
                format!(
                    "cannot copy {} to {}: {error}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn cmd_php_inputs(
    plan_path: &Path,
    version: &str,
    github_output: Option<&Path>,
) -> Result<(), String> {
    let cfg = load_config()?;
    let php = package_config(&cfg, "php")?;
    let plan = read_plan(plan_path)?;
    let item = plan
        .items
        .iter()
        .find(|item| item.package == "php" && item.version == version)
        .ok_or_else(|| format!("plan has no php {version}"))?;
    let sha = item
        .source
        .as_ref()
        .and_then(|source| source.get("sha256"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("plan item php {version} has no source.sha256"))?;
    let extensions = php
        .extensions
        .as_deref()
        .ok_or("catalog.config.json packages.php.extensions is missing")?;
    let spc = php
        .spc_version
        .as_deref()
        .ok_or("catalog.config.json packages.php.spcVersion is missing")?;
    let builder = php
        .builder
        .as_deref()
        .ok_or("catalog.config.json packages.php.builder is missing")?;
    append_github_output(
        github_output,
        &[
            format!("sha={sha}"),
            format!("extensions={extensions}"),
            format!("spc={spc}"),
            format!("builder={builder}"),
        ],
    )
}

fn gh_run(args: &[&str]) -> Result<std::process::Output, String> {
    let output = ProcessCommand::new("gh")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn cmd_php_publish(
    results: &Path,
    archives: &Path,
    github_output: Option<&Path>,
) -> Result<(), String> {
    let cfg = load_config()?;
    let mut tags = serde_json::Map::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(results)
        .map_err(|error| format!("cannot read {}: {error}", results.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("php-") && name.ends_with(".json"))
        })
        .collect();
    entries.sort();
    for entry in entries {
        let value: Value = serde_json::from_slice(
            &fs::read(&entry)
                .map_err(|error| format!("cannot read {}: {error}", entry.display()))?,
        )
        .map_err(|error| format!("{} is not valid JSON: {error}", entry.display()))?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let version = value
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{} has no version", entry.display()))?;
        let file = value
            .get("file")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{} has no file", entry.display()))?;
        let revision = cfg
            .revisions
            .get(&format!("php@{version}"))
            .copied()
            .unwrap_or(1);
        let tag = format!("php-{version}-r{revision}");
        let release_exists = ProcessCommand::new("gh")
            .args(["release", "view", &tag])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !release_exists {
            let title = format!("PHP CLI {version} (revision {revision})");
            let run = run_url().unwrap_or_default();
            let notes = format!(
                "Static PHP {version} CLI built from php.net sources by static-php-cli in CI. Workflow run: {run}. Install through the RAST Package Center."
            );
            gh_run(&[
                "release",
                "create",
                &tag,
                "--title",
                &title,
                "--latest=false",
                "--notes",
                &notes,
            ])?;
        }
        let archive = archives.join(file);
        if !archive.is_file() {
            return Err(format!("missing PHP archive {}", archive.display()));
        }
        gh_run(&[
            "release",
            "upload",
            &tag,
            archive
                .to_str()
                .ok_or("PHP archive path is not valid UTF-8")?,
            "--clobber",
        ])?;
        tags.insert(version.to_string(), Value::String(tag));
    }
    append_github_output(
        github_output,
        &[format!("releases={}", Value::Object(tags))],
    )
}

fn cmd_collect(artifacts: &Path, work: &Path, verify_result: &str) -> Result<(), String> {
    fs::create_dir_all(work)
        .map_err(|error| format!("cannot create {}: {error}", work.display()))?;
    // The plan artifact (plan.json + previous/).
    let plan_dir = artifacts.join("plan");
    if plan_dir.is_dir() {
        copy_tree(&plan_dir, work)?;
    }
    let results = work.join("results");
    fs::create_dir_all(&results)
        .map_err(|error| format!("cannot create {}: {error}", results.display()))?;
    let php_archives = work.join("php-dist");
    fs::create_dir_all(&php_archives)
        .map_err(|error| format!("cannot create {}: {error}", php_archives.display()))?;
    let mut upstream_results = 0usize;
    let mut php_results = 0usize;
    let mut php_archives_copied = 0usize;
    let entries = fs::read_dir(artifacts)
        .map_err(|error| format!("cannot read {}: {error}", artifacts.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read artifact: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with("results-upstream-") && path.is_dir() {
            for file in fs::read_dir(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?
            {
                let file = file.map_err(|error| format!("cannot read result: {error}"))?;
                if file.path().extension().is_some_and(|ext| ext == "json") {
                    let target = results.join(file.file_name());
                    fs::copy(file.path(), &target).map_err(|error| {
                        format!("cannot copy {}: {error}", file.path().display())
                    })?;
                    upstream_results += 1;
                }
            }
        } else if name.starts_with("php-") && path.is_dir() {
            let php_results_dir = path.join("results");
            if php_results_dir.is_dir() {
                for file in fs::read_dir(&php_results_dir).map_err(|error| {
                    format!("cannot read {}: {error}", php_results_dir.display())
                })? {
                    let file = file.map_err(|error| format!("cannot read result: {error}"))?;
                    if file.path().extension().is_some_and(|ext| ext == "json") {
                        let target = results.join(file.file_name());
                        fs::copy(file.path(), &target).map_err(|error| {
                            format!("cannot copy {}: {error}", file.path().display())
                        })?;
                        php_results += 1;
                    }
                }
            }
            let dist = path.join("dist");
            if dist.is_dir() {
                for file in fs::read_dir(&dist)
                    .map_err(|error| format!("cannot read {}: {error}", dist.display()))?
                {
                    let file = file.map_err(|error| format!("cannot read archive: {error}"))?;
                    if file
                        .path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".tar.gz"))
                    {
                        fs::copy(file.path(), php_archives.join(file.file_name())).map_err(
                            |error| format!("cannot copy {}: {error}", file.path().display()),
                        )?;
                        php_archives_copied += 1;
                    }
                }
            }
        }
    }
    if verify_result == "success" && upstream_results == 0 {
        return Err("the verify jobs succeeded but none of their results were collected".into());
    }
    log(format!(
        "collected {upstream_results} upstream result(s), {php_results} PHP result(s), {php_archives_copied} PHP archive(s)"
    ));
    Ok(())
}

fn cmd_sequence(catalog: &Path) -> Result<(), String> {
    let bytes =
        fs::read(catalog).map_err(|error| format!("cannot read {}: {error}", catalog.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", catalog.display()))?;
    let sequence = value
        .get("sequence")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{} has no sequence", catalog.display()))?;
    println!("{sequence}");
    Ok(())
}

// ----------------------------------------------------------- PHP builds

const PHP_INSIDE: &str = r#"#!/bin/sh
set -eux
apk add --no-cache bash file wget cmake gcc g++ jq autoconf git libstdc++ linux-headers make m4 libgcc \
  binutils bison flex pkgconf automake libtool curl xz patch perl gettext-dev re2c
cd /build
./spc doctor --auto-fix
./spc download --with-php="$VERSION" --for-extensions="$EXTENSIONS" --retry=2 \
  --custom-url="php-src:https://www.php.net/distributions/php-$VERSION.tar.xz"
found=""
for f in downloads/php-"$VERSION".tar.* downloads/php-src*; do
  [ -f "$f" ] || continue
  sum=$(sha256sum "$f" | cut -d' ' -f1)
  echo "source archive $f sha256 $sum"
  if [ "$sum" = "$SRC_SHA" ]; then found="$f"; fi
done
[ -n "$found" ] || { echo "no downloaded PHP source matches the php.net SHA-256 $SRC_SHA" >&2; ls -la downloads >&2; exit 1; }
./spc build "$EXTENSIONS" --build-cli
./spc dump-license --for-extensions="$EXTENSIONS" --dump-dir=buildroot/license || true
./buildroot/bin/php -v
chown -R "$HOST_UID:$HOST_GID" /build
"#;

fn arch_for_target(target: &str) -> Result<&'static str, String> {
    match target {
        "linux-x86_64" => Ok("x86_64"),
        "linux-aarch64" => Ok("aarch64"),
        other => Err(format!("unknown target {other}")),
    }
}

fn current_arch() -> Result<String, String> {
    let output = ProcessCommand::new("uname")
        .arg("-m")
        .output()
        .map_err(|error| format!("cannot run uname: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn spc_asset_digest(spc_version: &str, asset: &str) -> Result<String, String> {
    let output = gh_run(&[
        "api",
        &format!("repos/crazywhalecc/static-php-cli/releases/tags/{spc_version}"),
    ])?;
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("unexpected static-php-cli release response: {error}"))?;
    value
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|asset_value| asset_value.get("name").and_then(Value::as_str) == Some(asset))
        })
        .and_then(|asset_value| asset_value.get("digest"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("static-php-cli release {spc_version} has no digest for {asset}"))
}

fn write_php_archive(stage: &Path, package: &str, out: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    struct Entry {
        relative: PathBuf,
        path: PathBuf,
    }
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(stage)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", stage.display()))?;
        if entry.file_type().is_file() {
            entries.push(Entry {
                relative: entry
                    .path()
                    .strip_prefix(stage)
                    .map_err(|error| error.to_string())?
                    .to_path_buf(),
                path: entry.path().to_path_buf(),
            });
        }
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    fs::create_dir_all(out).map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let archive = out.join(format!("{package}.tar.gz"));
    let file = fs::File::create(&archive)
        .map_err(|error| format!("cannot create {}: {error}", archive.display()))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::best());
    let mut builder = tar::Builder::new(encoder);
    for entry in entries {
        let relative = Path::new(package).join(&entry.relative);
        let metadata = fs::metadata(&entry.path)
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(if metadata.permissions().mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        let data = fs::read(&entry.path)
            .map_err(|error| format!("cannot read {}: {error}", entry.path.display()))?;
        builder
            .append_data(&mut header, &relative, data.as_slice())
            .map_err(|error| format!("cannot add {}: {error}", relative.display()))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|error| format!("cannot finish archive: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("cannot finish archive: {error}"))?;
    Ok(archive)
}

fn cmd_build_php(
    version: &str,
    target: &str,
    source_sha256: &str,
    extensions: &str,
    spc_version: &str,
    builder_image: &str,
    out: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let arch = arch_for_target(target)?;
    let host = current_arch()?;
    if host != arch {
        return Err(format!(
            "build {target} on a native {arch} machine (got {host})"
        ));
    }
    let work_dir = WorkDir::new("rast-php-")?;
    let work = work_dir.path().to_path_buf();
    let asset = format!("spc-linux-{arch}.tar.gz");
    gh_run(&[
        "release",
        "download",
        spc_version,
        "-R",
        "crazywhalecc/static-php-cli",
        "-p",
        &asset,
        "-D",
        work.to_str().ok_or("temporary path is not valid UTF-8")?,
    ])?;
    let expected = spc_asset_digest(spc_version, &asset)?;
    let spc_archive = work.join(&asset);
    let actual = format!("sha256:{}", sha256_of(&spc_archive)?);
    if actual != expected {
        return Err(format!(
            "static-php-cli digest mismatch: expected {expected:?}, got {actual:?}"
        ));
    }
    let mut progress = |_stats: &archive::ExtractStats| {};
    archive::extract_tar_gz(
        &spc_archive,
        &work,
        0,
        16 * 1024 * 1024 * 1024,
        &mut progress,
        &|| false,
    )?;
    let spc = work.join("spc");
    fs::set_permissions(&spc, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make {} executable: {error}", spc.display()))?;
    let inside = work.join("inside.sh");
    fs::write(&inside, PHP_INSIDE).map_err(|error| format!("cannot write inside.sh: {error}"))?;
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make inside.sh executable: {error}"))?;
    log(format!(
        "building PHP {version} ({target}) in {builder_image}"
    ));
    let mut docker = ProcessCommand::new("docker");
    docker
        .args(["run", "--rm", "-v"])
        .arg(format!("{}:/build", work.display()))
        .args(["-w", "/build"])
        .args(["-e", &format!("VERSION={version}")])
        .args(["-e", &format!("EXTENSIONS={extensions}")])
        .args(["-e", &format!("SRC_SHA={source_sha256}")])
        .args(["-e", &format!("HOST_UID={}", unsafe { libc::geteuid() })])
        .args(["-e", &format!("HOST_GID={}", unsafe { libc::getegid() })]);
    if env_nonempty("GITHUB_TOKEN").is_some() || env_nonempty("GH_TOKEN").is_some() {
        docker.args(["-e", "GITHUB_TOKEN"]);
    }
    docker
        .arg(builder_image)
        .arg("/build/inside.sh")
        .status()
        .map_err(|error| format!("cannot run docker: {error}"))?;
    let php = work.join("buildroot/bin/php");
    if !php.is_file() {
        return Err("static-php-cli produced no buildroot/bin/php".into());
    }
    let package = format!("php-{version}");
    let stage = work.join("stage").join(&package);
    let bin = stage.join("bin");
    fs::create_dir_all(&bin)
        .map_err(|error| format!("cannot create {}: {error}", bin.display()))?;
    fs::copy(&php, bin.join("php")).map_err(|error| format!("cannot copy PHP binary: {error}"))?;
    fs::set_permissions(bin.join("php"), fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make PHP executable: {error}"))?;
    let licenses = work.join("buildroot/license");
    if licenses.is_dir() {
        copy_tree(&licenses, &stage.join("licenses"))?;
    }
    let version_output = ProcessCommand::new(bin.join("php"))
        .arg("-v")
        .output()
        .map_err(|error| format!("cannot run the built PHP: {error}"))?;
    let text = String::from_utf8_lossy(&version_output.stdout);
    if !text.contains(&format!("PHP {version} ")) {
        return Err(format!(
            "the built PHP does not report {version}: {}",
            text.lines().next().unwrap_or("")
        ));
    }
    let run = run_url().unwrap_or_else(|| {
        format!(
            "{}/{}/actions/runs/0",
            env::var("GITHUB_SERVER_URL").unwrap_or_else(|_| "https://github.com".into()),
            env::var("GITHUB_REPOSITORY").unwrap_or_else(|_| "local".into())
        )
    });
    let buildinfo = json!({
        "php": version,
        "target": target,
        "source": format!("https://www.php.net/distributions/php-{version}.tar.xz"),
        "sourceSha256": source_sha256,
        "spcVersion": spc_version,
        "spcAssetDigest": expected,
        "builder": builder_image,
        "extensions": extensions,
        "workflowRun": run,
    });
    fs::write(
        stage.join("BUILDINFO.json"),
        serde_json::to_vec_pretty(&buildinfo).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write BUILDINFO.json: {error}"))?;
    let archive = write_php_archive(&stage, &package, out)?;
    let sha = sha256_of(&archive)?;
    println!("{sha}  {}", archive.display());
    append_github_output(None, &[format!("archive={}", archive.display())])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::KeyPair;

    const OFFICIAL_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MCowBQYDK2VwAyEAckXYZF+YAsH3mbVaukQbzqNKfaER3ognkhuqtmZxV4M=\n\
-----END PUBLIC KEY-----\n";

    fn test_config() -> Config {
        serde_json::from_str(
            r#"{
              "name": "RAST official catalog",
              "publisher": {
                "id": "rast",
                "name": "RAST",
                "url": "https://github.com/Enricko/rast-packages"
              },
              "repository": "https://github.com/Enricko/rast-packages",
              "keyId": "rast-official-2026-11",
              "publicKey": "keys/rast-official-2026-11.pub.pem",
              "minRastVersion": "0.5.0",
              "expiresDays": 30,
              "refreshBeforeDays": 10,
              "allowedHosts": ["github.com", "nodejs.org", "dl.google.com"],
              "targets": {
                "linux-x86_64": {"runner": "ubuntu-24.04", "go": "amd64", "node": "x64", "python": "x86_64"},
                "linux-aarch64": {"runner": "ubuntu-24.04-arm", "go": "arm64", "node": "arm64", "python": "aarch64"}
              },
              "withheld": [],
              "revisions": {"python@3.14.0": 2},
              "packages": {
                "nodejs": {
                  "name": "Node.js",
                  "description": "Node.js runtime",
                  "homepage": "https://nodejs.org",
                  "license": "MIT",
                  "keepPatches": 2,
                  "channels": [{"id": "24", "label": "Node.js 24", "status": "LTS"}]
                },
                "python": {
                  "name": "Python",
                  "description": "CPython interpreter",
                  "homepage": "https://www.python.org",
                  "license": "PSF-2.0",
                  "keepPatches": 1,
                  "channels": [{"id": "3.14", "label": "Python 3.14", "status": "latest"}]
                }
              }
            }"#,
        )
        .unwrap()
    }

    fn python_item() -> PlanItem {
        let mut targets = BTreeMap::new();
        targets.insert(
            "linux-x86_64".to_string(),
            TargetInfo {
                url: Some(
                    "https://github.com/astral-sh/python-build-standalone/releases/download/20251010/cpython-3.14.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz"
                        .into(),
                ),
                file: Some(
                    "cpython-3.14.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz".into(),
                ),
                size: Some(1234),
                digest: Some("a".repeat(64)),
                source: Some(
                    "https://github.com/astral-sh/python-build-standalone/releases/tag/20251010"
                        .into(),
                ),
                ..TargetInfo::default()
            },
        );
        PlanItem {
            package: "python".into(),
            version: "3.14.0".into(),
            channel: "3.14".into(),
            kind: "upstream".into(),
            release_date: Some("2026-01-05".into()),
            build_tag: Some("20251010".into()),
            targets,
            sums_url: Some(
                "https://github.com/astral-sh/python-build-standalone/releases/download/20251010/SHA256SUMS"
                    .into(),
            ),
            revision: 1,
            status: "new".into(),
            ..PlanItem::default()
        }
    }

    fn python_result() -> Outcome {
        Outcome {
            package: "python".into(),
            version: "3.14.0".into(),
            target: "linux-x86_64".into(),
            ok: true,
            sha256: Some("a".repeat(64)),
            size: Some(1234),
            bundled: BTreeMap::from([("pip".to_string(), "25.2".to_string())]),
            verification: Some(
                "SHA-256 from the release SHA256SUMS, equal to the GitHub asset digest; GitHub build-provenance attestation verified with gh attestation verify"
                    .into(),
            ),
            ..Outcome::default()
        }
    }

    #[test]
    fn channels_match_the_reference() {
        // Node.js streams are major versions, the others major.minor.
        assert_eq!(channel_of("nodejs", "24.22.2"), Some("24".to_string()));
        assert_eq!(channel_of("nodejs", "26"), Some("26".to_string()));
        assert_eq!(channel_of("go", "1.27.1"), None);
        assert_eq!(channel_of("python", "3.14.0"), None);
        assert_eq!(default_channel("3.14.10"), "3.14");
        assert_eq!(default_channel("8.4.27"), "8.4");
        assert_eq!(default_channel("1.27.1"), "1.27");
        // rastpkg.default_channel is major.minor even for Node.js; Node.js
        // discovery uses channel_of, i.e. the major version.
        assert_eq!(default_channel("24.22.2"), "24.22");
        assert_eq!(default_channel("24"), "24");
        assert_eq!(default_channel("not a version"), "");
    }

    #[test]
    fn versions_sort_like_the_reference() {
        let mut versions = vec![
            "1.26.9".to_string(),
            "1.27.0-rc1".to_string(),
            "1.26.11".to_string(),
            "1.27.0".to_string(),
        ];
        versions.sort_by(|a, b| version::compare(a, b));
        assert_eq!(versions, vec!["1.26.9", "1.26.11", "1.27.0-rc1", "1.27.0"]);
        // Go only accepts X.Y.Z.
        for good in ["1.27.1", "24.22.2", "8.4.27", "1.27.0"] {
            assert!(is_go_version(good), "{good}");
        }
        for bad in ["1.27", "1.27.1.1", "go1.27.1", "1.27.a", "1..1"] {
            assert!(!is_go_version(bad), "{bad}");
        }
    }

    #[test]
    fn withheld_versions_merge_and_sort() {
        let held = withheld_set(
            &["php@8.4.1".to_string()],
            " go@1.26.11, ,php@8.4.1, nodejs@24.22.2 ",
        );
        assert_eq!(
            held.into_iter().collect::<Vec<_>>(),
            vec!["go@1.26.11", "nodejs@24.22.2", "php@8.4.1"]
        );
        assert!(withheld_set(&[], "").is_empty());
        assert!(withheld_set(&[], " , , ").is_empty());
    }

    #[test]
    fn run_urls_match_the_workflow_urls() {
        assert_eq!(
            run_url_from(None, Some("Enricko/rast-packages"), Some("12345")).as_deref(),
            Some("https://github.com/Enricko/rast-packages/actions/runs/12345")
        );
        assert_eq!(
            run_url_from(
                Some("https://ghe.example"),
                Some("Enricko/rast-packages"),
                Some("7")
            )
            .as_deref(),
            Some("https://ghe.example/Enricko/rast-packages/actions/runs/7")
        );
        assert_eq!(run_url_from(None, Some("o/r"), None), None);
        assert_eq!(run_url_from(None, Some(""), Some("7")), None);
        assert_eq!(run_url_from(None, Some("o/r"), Some("")), None);
        assert_eq!(run_note(None), "");
        assert_eq!(
            run_note(Some("https://github.com/o/r/actions/runs/7")),
            " (https://github.com/o/r/actions/runs/7)"
        );
    }

    #[test]
    fn pem_blocks_and_spki_keys_are_parsed() {
        let der = pem_base64_der(OFFICIAL_PUBLIC_PEM, "PUBLIC KEY").unwrap();
        assert_eq!(der.len(), 44);
        let key = ed25519_public_key(OFFICIAL_PUBLIC_PEM).unwrap();
        assert_eq!(
            B64.encode(key),
            "ckXYZF+YAsH3mbVaukQbzqNKfaER3ognkhuqtmZxV4M="
        );
        // Raw 32-byte DER is accepted too; garbage is not.
        assert!(ed25519_public_key_from_der(&key).is_ok());
        assert!(ed25519_public_key_from_der(&[0u8; 10]).is_err());
        assert!(pem_base64_der("no pem here", "PUBLIC KEY").is_none());
        // A PKCS#8 PEM round-trip through ring signs and verifies.
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let private_pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            B64.encode(pkcs8.as_ref())
        );
        let public_pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64.encode(&spki)
        );
        let key_pair = ed25519_key_pair(&private_pem).unwrap();
        let signature = key_pair.sign(b"catalog bytes");
        let signature_file = json!({
            "keyId": "test-key",
            "algorithm": "ed25519",
            "signature": B64.encode(signature.as_ref()),
        })
        .to_string();
        verify_signature_bytes(b"catalog bytes", &signature_file, &public_pem, "test-key").unwrap();
        assert!(
            verify_signature_bytes(b"tampered", &signature_file, &public_pem, "test-key").is_err()
        );
        assert!(verify_signature_bytes(
            b"catalog bytes",
            &signature_file,
            &public_pem,
            "another-key"
        )
        .is_err());
        assert!(verify_signature_bytes(
            b"catalog bytes",
            r#"{"keyId":"test-key","algorithm":"rsa","signature":""}"#,
            &public_pem,
            "test-key"
        )
        .is_err());
    }

    #[test]
    fn python_asset_names_are_matched_like_the_regex() {
        assert_eq!(
            python_asset_version(
                "cpython-3.14.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz",
                "3.14",
                "20251010"
            ),
            Some("3.14.0".into())
        );
        assert_eq!(
            python_asset_version(
                "cpython-3.14.10+20251010-aarch64-unknown-linux-gnu-install_only.tar.gz",
                "3.14",
                "20251010"
            ),
            Some("3.14.10".into())
        );
        // Wrong channel, wrong tag, checksum file, other artifact kind.
        assert_eq!(
            python_asset_version(
                "cpython-3.13.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz",
                "3.14",
                "20251010"
            ),
            None
        );
        assert_eq!(
            python_asset_version(
                "cpython-3.14.0+20250101-x86_64-unknown-linux-gnu-install_only.tar.gz",
                "3.14",
                "20251010"
            ),
            None
        );
        assert_eq!(python_asset_version("SHA256SUMS", "3.14", "20251010"), None);
        assert_eq!(
            python_asset_version(
                "cpython-3.14.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz.sha256",
                "3.14",
                "20251010"
            ),
            None
        );
    }

    #[test]
    fn json_is_written_like_python() {
        let text = json_text(&json!({"b": [1, 2], "a": {"c": true}})).unwrap();
        assert_eq!(
            text,
            "{\n  \"a\": {\n    \"c\": true\n  },\n  \"b\": [\n    1,\n    2\n  ]\n}\n"
        );
        assert!(text.ends_with('\n'));
        assert_eq!(
            json_text(&json!({"name": "Python — 3.14"})).unwrap(),
            "{\n  \"name\": \"Python — 3.14\"\n}\n",
            "non-ASCII must not be escaped (ensure_ascii=False)"
        );
    }

    #[test]
    fn allowed_hosts_are_checked_like_rastpkg_https_host() {
        let hosts = BTreeSet::from(["github.com".to_string(), "nodejs.org".to_string()]);
        let url = "https://github.com/o/r/releases/download/t/a";
        for final_url in [
            "https://github.com/o/r/releases/download/t/a",
            "https://github.com:443/o/r",
        ] {
            final_host_allowed(url, final_url, &hosts).unwrap();
        }
        for final_url in [
            "https://evil.example/a",
            "http://github.com/a",
            "https://user@github.com/a",
            "https://github.com:8443/a",
        ] {
            let error = final_host_allowed(url, final_url, &hosts).unwrap_err();
            assert!(error.contains(url), "{final_url}: {error}");
            assert!(error.contains("redirects to"), "{final_url}: {error}");
        }
        let outside = final_host_allowed(url, "https://evil.example/a", &hosts).unwrap_err();
        assert!(outside.contains("outside allowedHosts"), "{outside}");
        let scheme = final_host_allowed(url, "http://github.com/a", &hosts).unwrap_err();
        assert!(scheme.contains("https"), "{scheme}");
    }

    #[test]
    fn carry_decisions_follow_revision_and_target_coverage() {
        let old = json!({
            "version": "3.14.0",
            "revision": 1,
            "artifacts": [{"target": "linux-x86_64"}],
        });
        let mut targets = BTreeMap::new();
        targets.insert("linux-x86_64".to_string(), TargetInfo::default());
        assert!(is_carry(Some(&old), 1, &targets, false));
        assert!(!is_carry(Some(&old), 2, &targets, false), "new revision");
        assert!(!is_carry(Some(&old), 1, &targets, true), "--force");
        targets.insert("linux-aarch64".to_string(), TargetInfo::default());
        assert!(
            !is_carry(Some(&old), 1, &targets, false),
            "missing artifact target"
        );
        assert!(!is_carry(None, 1, &targets, false));
    }

    #[test]
    fn sequences_and_detected_entries_follow_the_reference() {
        assert_eq!(next_sequence(4, true), 6, "interim + final");
        assert_eq!(next_sequence(4, false), 5);
        let built = detected_entry("8.5.1", "8.5", "built", "2026-01-01T00:00:00Z", " (run)");
        assert_eq!(built["state"], "building");
        assert_eq!(built["channel"], "8.5");
        assert_eq!(
            built["detail"],
            "New upstream release; the catalog workflow is building and testing it (run)."
        );
        let upstream = detected_entry("1.27.1", "1.27", "upstream", "2026-01-01T00:00:00Z", "");
        assert_eq!(upstream["state"], "verifying");
        assert_eq!(
            upstream["detail"],
            "New upstream release; the catalog workflow is verifying checksums and running it."
        );
    }

    #[test]
    fn runtime_specs_match_the_reference() {
        let bundled = BTreeMap::from([("npm".to_string(), "11.6.0".to_string())]);
        let optional = BTreeSet::from(["corepack".to_string()]);
        let node = runtime_spec("nodejs", "24.22.2", &bundled, &optional).unwrap();
        assert_eq!(
            node.executables
                .iter()
                .map(|executable| executable.name.as_str())
                .collect::<Vec<_>>(),
            vec!["node", "npm", "npx", "corepack"]
        );
        assert_eq!(node.checks.len(), 2);
        assert_eq!(node.checks[0].expect, "v24.22.2");
        assert_eq!(node.checks[1].expect, "11.6.0");
        assert_eq!(node.bundled[0].name, "npm");

        let python_bundled = BTreeMap::from([("pip".to_string(), "25.2".to_string())]);
        let python = runtime_spec("python", "3.14.0", &python_bundled, &BTreeSet::new()).unwrap();
        assert_eq!(python.executables[1].name, "python3.14");
        assert_eq!(python.checks[1].expect, "pip 25.2");
        assert_eq!(python.checks[1].label.as_deref(), Some("pip"));

        let go = runtime_spec("go", "1.27.1", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(go.checks[0].expect, "go1.27.1");
        let php = runtime_spec("php", "8.5.1", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(php.checks[0].expect, "PHP 8.5.1");
        assert!(runtime_spec("lua", "5.4.8", &BTreeMap::new(), &BTreeSet::new()).is_err());
    }

    #[test]
    fn package_entries_keep_the_configuration_order() {
        let cfg = test_config();
        let packages = package_entries(&cfg);
        assert_eq!(
            packages
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["nodejs", "python"]
        );
        assert_eq!(packages[0].1["type"], "runtime");
        assert_eq!(packages[0].1["channels"][0]["id"], "24");
        assert_eq!(packages[0].1["versions"], json!([]));
        assert_eq!(packages[0].1["detected"], json!([]));
    }

    #[test]
    fn assembled_python_manifest_has_the_expected_shape() {
        let cfg = test_config();
        let item = python_item();
        let (manifest, detail) = build_manifest(&cfg, &item, &[python_result()], None).unwrap();
        assert_eq!(detail, "");
        let manifest = manifest.expect("a verified target must produce a manifest");
        assert_eq!(manifest["schemaVersion"], 1);
        assert_eq!(manifest["kind"], "rast-package");
        assert_eq!(manifest["id"], "python");
        assert_eq!(manifest["type"], "runtime");
        assert_eq!(manifest["name"], "Python");
        assert_eq!(manifest["version"], "3.14.0");
        assert_eq!(manifest["revision"], 1);
        assert_eq!(manifest["minRastVersion"], "0.5.0");
        assert_eq!(manifest["releaseDate"], "2026-01-05");
        assert!(
            manifest.get("channel").is_none(),
            "Python streams are implicit major.minor"
        );
        assert_eq!(manifest["runtime"]["executables"][0]["path"], "bin/python3");
        assert_eq!(manifest["runtime"]["bundled"][0]["name"], "pip");
        assert_eq!(manifest["runtime"]["bundled"][0]["version"], "25.2");
        assert!(manifest["runtime"].get("extensions").is_none());
        assert_eq!(manifest["artifacts"][0]["target"], "linux-x86_64");
        assert_eq!(manifest["artifacts"][0]["stripComponents"], 1);
        assert_eq!(manifest["artifacts"][0]["format"], "tar.gz");
        assert_eq!(manifest["artifacts"][0]["provenance"]["kind"], "upstream");
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.17");
        assert_eq!(
            manifest["artifacts"][0]["url"],
            "https://github.com/astral-sh/python-build-standalone/releases/download/20251010/cpython-3.14.0+20251010-x86_64-unknown-linux-gnu-install_only.tar.gz"
        );
        // The result must survive RAST's own manifest parser.
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let parsed = manifest::parse(&bytes).unwrap();
        manifest::validate(&parsed).unwrap();

        // The minimal assembled catalog validates through catalog::parse.
        let mut packages = package_entries(&cfg);
        push_version(&mut packages, &manifest).unwrap();
        let doc = catalog_doc(&cfg, 3, &mut packages).unwrap();
        assert_eq!(doc["kind"], "rast-catalog");
        assert_eq!(doc["sequence"], 3);
        assert_eq!(doc["allowedHosts"][0], "github.com");
        assert!(catalog_problems(&doc).unwrap().is_empty());
        let catalog = catalog::parse(&serde_json::to_vec(&doc).unwrap()).unwrap();
        assert_eq!(catalog.sequence, 3);
        assert_eq!(catalog.packages[1].id, "python");
        assert_eq!(catalog.packages[1].versions.len(), 1);
        assert_eq!(catalog.packages[1].versions[0].version, "3.14.0");
        for key in ["generatedAt", "expiresAt"] {
            let raw = doc[key].as_str().unwrap();
            assert!(DateTime::parse_from_rfc3339(raw).is_ok(), "{key}: {raw}");
            assert!(raw.ends_with('Z'));
        }
    }

    #[test]
    fn node_manifests_get_an_explicit_major_channel() {
        let cfg = test_config();
        let item = PlanItem {
            package: "nodejs".into(),
            version: "24.22.2".into(),
            channel: "24".into(),
            kind: "upstream".into(),
            revision: 1,
            ..PlanItem::default()
        };
        let manifest = base_manifest(&cfg, &item).unwrap();
        assert_eq!(manifest["channel"], "24");
        let python = python_item();
        assert!(base_manifest(&cfg, &python)
            .unwrap()
            .get("channel")
            .is_none());
    }

    #[test]
    fn a_version_without_verified_targets_is_not_manifested() {
        let cfg = test_config();
        let item = python_item();
        let failed = Outcome {
            package: "python".into(),
            version: "3.14.0".into(),
            target: "linux-x86_64".into(),
            ok: false,
            error: Some("SHA-256 mismatch".into()),
            ..Outcome::default()
        };
        let (manifest, detail) = build_manifest(&cfg, &item, &[failed], None).unwrap();
        assert!(manifest.is_none());
        assert_eq!(detail, "linux-x86_64: SHA-256 mismatch");
        let (manifest, detail) = build_manifest(&cfg, &item, &[], None).unwrap();
        assert!(manifest.is_none());
        assert_eq!(detail, "no target produced a verified artifact");
    }

    #[test]
    fn plan_items_serialize_with_python_field_names() {
        let node = PlanItem {
            package: "nodejs".into(),
            version: "24.22.2".into(),
            channel: "24".into(),
            kind: "upstream".into(),
            release_date: Some("2026-01-05".into()),
            index_npm: Some("11.6.0".into()),
            lts: Some("Jod".into()),
            targets: BTreeMap::from([(
                "linux-x86_64".to_string(),
                TargetInfo {
                    url: Some(
                        "https://nodejs.org/dist/v24.22.2/node-v24.22.2-linux-x64.tar.gz".into(),
                    ),
                    file: Some("node-v24.22.2-linux-x64.tar.gz".into()),
                    source: Some("https://nodejs.org/dist/v24.22.2/".into()),
                    ..TargetInfo::default()
                },
            )]),
            revision: 2,
            status: "new".into(),
            ..PlanItem::default()
        };
        let value = serde_json::to_value(&node).unwrap();
        for key in [
            "package",
            "version",
            "channel",
            "kind",
            "releaseDate",
            "indexNpm",
            "lts",
            "targets",
            "revision",
            "status",
        ] {
            assert!(value.get(key).is_some(), "plan item has no {key}");
        }
        assert!(value.get("buildTag").is_none());
        assert!(value.get("sumsUrl").is_none());
        assert_eq!(
            value["targets"]["linux-x86_64"]["file"],
            "node-v24.22.2-linux-x64.tar.gz"
        );
        let parsed: PlanItem = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.version, "24.22.2");
        assert_eq!(parsed.revision, 2);
        assert_eq!(parsed.index_npm.as_deref(), Some("11.6.0"));

        let php = PlanItem {
            package: "php".into(),
            version: "8.5.0".into(),
            channel: "8.5".into(),
            kind: "built".into(),
            source: Some(json!({
                "url": "https://www.php.net/distributions/php-8.5.0.tar.xz",
                "sha256": "b".repeat(64),
                "all": {"php-8.5.0.tar.xz": "b".repeat(64)},
            })),
            targets: BTreeMap::from([("linux-x86_64".to_string(), TargetInfo::default())]),
            revision: 1,
            status: "new".into(),
            ..PlanItem::default()
        };
        let value = serde_json::to_value(&php).unwrap();
        assert_eq!(value["targets"]["linux-x86_64"], json!({}));
        assert_eq!(value["source"]["sha256"], "b".repeat(64));
        assert!(value.get("releaseDate").is_none());
    }

    #[test]
    fn verify_results_deserialize_for_assembly() {
        let value = json!({
            "package": "nodejs",
            "version": "24.22.2",
            "target": "linux-x86_64",
            "ok": true,
            "sha256": "a".repeat(64),
            "size": 4096,
            "bundled": {"npm": "11.6.0"},
            "optional": ["corepack"],
            "checks": ["v24.22.2"],
            "verification": "SHA-256 from SHASUMS256.txt",
            "extensions": [],
        });
        let outcome: Outcome = serde_json::from_value(value).unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.target, "linux-x86_64");
        assert_eq!(outcome.bundled["npm"], "11.6.0");
        assert!(outcome.optional.contains(&"corepack".to_string()));
        assert!(outcome.sha256.is_some());

        let failure: Outcome = serde_json::from_value(json!({
            "package": "go",
            "version": "1.27.1",
            "target": "linux-aarch64",
            "ok": false,
            "error": "size mismatch: expected 1, got 2",
        }))
        .unwrap();
        assert!(!failure.ok);
        assert_eq!(
            failure.error.as_deref(),
            Some("size mismatch: expected 1, got 2")
        );
    }

    #[test]
    fn catalog_problems_report_every_invalid_version() {
        let cfg = test_config();
        let item = python_item();
        let (manifest, _) = build_manifest(&cfg, &item, &[python_result()], None).unwrap();
        let manifest = manifest.unwrap();
        let mut packages = package_entries(&cfg);
        push_version(&mut packages, &manifest).unwrap();
        let doc = catalog_doc(&cfg, 1, &mut packages).unwrap();

        // An artifact host outside allowedHosts is a per-version problem.
        let mut foreign = doc.clone();
        foreign["packages"][1]["versions"][0]["artifacts"][0]["url"] =
            json!("https://evil.example/cpython.tar.gz");
        let problems = catalog_problems(&foreign).unwrap();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("allowed hosts")),
            "{problems:?}"
        );

        // A version listed twice is a problem, not a silent drop.
        let mut duplicated = doc.clone();
        let manifest = duplicated["packages"][1]["versions"][0].clone();
        duplicated["packages"][1]["versions"]
            .as_array_mut()
            .unwrap()
            .push(manifest);
        let problems = catalog_problems(&duplicated).unwrap();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("listed twice")),
            "{problems:?}"
        );

        // The publisher of the catalog and of each manifest must agree.
        let mut publisher_changed = doc.clone();
        publisher_changed["publisher"]["id"] = json!("someone-else");
        let problems = catalog_problems(&publisher_changed).unwrap();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("publisher differs")),
            "{problems:?}"
        );
    }

    #[test]
    fn timestamps_and_previous_catalogs_are_loaded_like_the_reference() {
        for raw in [
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00+00:00",
            "2026-01-05T00:00:00",
            "2026-01-05",
        ] {
            assert_eq!(
                iso(parse_timestamp(raw).unwrap()),
                "2026-01-05T00:00:00Z",
                "{raw}"
            );
        }
        assert!(parse_timestamp("next tuesday").is_err());

        let dir = tempfile::tempdir().unwrap();
        assert!(load_previous(Some(dir.path())).unwrap().is_none());
        assert!(load_previous(Some(Path::new(""))).unwrap().is_none());
        assert!(load_previous(None).unwrap().is_none());
        fs::write(dir.path().join("catalog.json"), br#"{"sequence": 4}"#).unwrap();
        let previous = load_previous(Some(dir.path())).unwrap().unwrap();
        assert_eq!(previous["sequence"], 4);

        let previous = json!({
            "packages": [
                {"id": "python", "versions": [
                    {"id": "python", "version": "3.14.0", "revision": 1,
                     "artifacts": [{"target": "linux-x86_64"}]},
                ]},
                {"id": "go", "versions": [
                    {"id": "go", "version": "1.27.1", "revision": 1,
                     "artifacts": [{"target": "linux-x86_64"}]},
                ]},
            ]
        });
        let manifests = previous_manifests(Some(&previous));
        assert_eq!(
            manifests.keys().cloned().collect::<Vec<_>>(),
            vec!["go@1.27.1", "python@3.14.0"]
        );
        let keep = BTreeSet::from(["python@3.14.0".to_string()]);
        let carried = carried_manifests(Some(&previous), &keep);
        assert_eq!(carried.len(), 1);
        assert_eq!(carried["python@3.14.0"]["version"], "3.14.0");
        assert!(carried_manifests(None, &keep).is_empty());
    }

    #[test]
    fn result_files_are_collected_in_path_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(
            dir.path().join("a.json"),
            br#"{"package": "first", "version": "1.0.0", "target": "linux-x86_64", "ok": false}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("sub").join("b.json"),
            br#"[{"package": "second", "version": "2.0.0", "target": "linux-x86_64", "ok": true},
                 {"package": "third", "version": "3.0.0", "target": "linux-aarch64", "ok": true}]"#,
        )
        .unwrap();
        fs::write(dir.path().join("ignored.txt"), b"not JSON").unwrap();
        let results = load_results(dir.path()).unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result["package"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert!(load_results(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn php_release_maps_are_loaded_from_json_or_a_file() {
        assert_eq!(load_php_releases(None).unwrap(), json!({}));
        assert_eq!(load_php_releases(Some(Path::new(""))).unwrap(), json!({}));
        let literal = Path::new(r#"{"8.5.0": "php-8.5.0-r1"}"#);
        assert_eq!(
            load_php_releases(Some(literal)).unwrap(),
            json!({"8.5.0": "php-8.5.0-r1"})
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("releases.json");
        fs::write(&path, br#"{"8.4.0": "php-8.4.0-r1"}"#).unwrap();
        assert_eq!(
            load_php_releases(Some(&path)).unwrap(),
            json!({"8.4.0": "php-8.4.0-r1"})
        );
        assert!(load_php_releases(Some(Path::new("[]"))).is_err());
    }

    #[test]
    fn signature_files_are_verified_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let public_pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64.encode(&spki)
        );
        let catalog_bytes = br#"{"schemaVersion": 1}"#;
        let catalog_path = dir.path().join("catalog.json");
        let signature_path = dir.path().join("catalog.json.sig");
        let public_path = dir.path().join("catalog.pub.pem");
        fs::write(&catalog_path, catalog_bytes).unwrap();
        fs::write(&public_path, &public_pem).unwrap();
        let signature = pair.sign(catalog_bytes);
        fs::write(
            &signature_path,
            format!(
                "{{\"keyId\": \"key-1\", \"algorithm\": \"ed25519\", \"signature\": \"{}\"}}\n",
                B64.encode(signature.as_ref())
            ),
        )
        .unwrap();
        verify_signature_files(&catalog_path, &signature_path, &public_path, "key-1").unwrap();
        assert!(
            verify_signature_files(&catalog_path, &signature_path, &public_path, "key-2").is_err()
        );
        fs::write(&catalog_path, b"tampered").unwrap();
        assert!(
            verify_signature_files(&catalog_path, &signature_path, &public_path, "key-1").is_err()
        );
        assert!(
            verify_signature_files(&catalog_path, &signature_path, &public_path, "key-1").is_err()
        );
        assert!(verify_signature_files(
            &catalog_path,
            &dir.path().join("missing.sig"),
            &public_path,
            "key-1"
        )
        .is_err());
    }

    fn python_item_for(version: &str, targets: &[&str], revision: u32) -> PlanItem {
        let mut item = PlanItem {
            package: "python".into(),
            version: version.into(),
            channel: "3.14".into(),
            kind: "upstream".into(),
            release_date: Some("2026-01-05".into()),
            build_tag: Some("20251010".into()),
            sums_url: Some(
                "https://github.com/astral-sh/python-build-standalone/releases/download/20251010/SHA256SUMS"
                    .into(),
            ),
            revision,
            status: "new".into(),
            ..PlanItem::default()
        };
        for target in targets {
            let architecture = if *target == "linux-x86_64" {
                "x86_64"
            } else {
                "aarch64"
            };
            let file = format!(
                "cpython-{version}+20251010-{architecture}-unknown-linux-gnu-install_only.tar.gz"
            );
            item.targets.insert(
                (*target).to_string(),
                TargetInfo {
                    url: Some(format!(
                        "https://github.com/astral-sh/python-build-standalone/releases/download/20251010/{file}"
                    )),
                    file: Some(file),
                    size: Some(1234),
                    digest: Some("a".repeat(64)),
                    source: Some(
                        "https://github.com/astral-sh/python-build-standalone/releases/tag/20251010"
                            .into(),
                    ),
                    ..TargetInfo::default()
                },
            );
        }
        item
    }

    fn python_outcome(version: &str, target: &str) -> Outcome {
        Outcome {
            package: "python".into(),
            version: version.into(),
            target: target.into(),
            ok: true,
            sha256: Some("a".repeat(64)),
            size: Some(1234),
            bundled: BTreeMap::from([("pip".to_string(), "25.2".to_string())]),
            verification: Some("SHA-256 from the release SHA256SUMS".into()),
            ..Outcome::default()
        }
    }

    fn previous_manifest(cfg: &Config, version: &str, targets: &[&str]) -> Value {
        let item = python_item_for(version, targets, 1);
        let results: Vec<Outcome> = targets
            .iter()
            .map(|target| python_outcome(version, target))
            .collect();
        build_manifest(cfg, &item, &results, None)
            .unwrap()
            .0
            .unwrap()
    }

    fn write_previous_catalog(dir: &Path, sequence: u64, versions: Vec<Value>) {
        fs::create_dir_all(dir).unwrap();
        let document = json!({
            "sequence": sequence,
            "expiresAt": "2099-01-01T00:00:00Z",
            "packages": [{"id": "python", "versions": versions}],
        });
        fs::write(
            dir.join("catalog.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn interim_announces_only_new_versions_and_keeps_old_ones() {
        let cfg = test_config();
        let dir = tempfile::tempdir().unwrap();
        let previous_dir = dir.path().join("previous");
        let old_311 = previous_manifest(&cfg, "3.11.0", &["linux-x86_64"]);
        let old_312 = previous_manifest(&cfg, "3.12.1", &["linux-x86_64"]);
        let old_313 = previous_manifest(&cfg, "3.13.0", &["linux-x86_64"]);
        write_previous_catalog(
            &previous_dir,
            4,
            vec![old_311, old_313.clone(), old_312.clone()],
        );

        let mut carry = python_item_for("3.12.1", &["linux-x86_64"], 1);
        carry.status = "carry".into();
        let revalidated = python_item_for("3.13.0", &["linux-x86_64"], 2);
        let detected = python_item_for("3.15.0", &["linux-x86_64"], 1);
        let plan = Plan {
            generated_at: "2026-01-01T00:00:00Z".into(),
            previous_sequence: 4,
            withheld: Vec::new(),
            items: vec![carry, revalidated, detected],
            dropped: vec!["python@3.11.0".into()],
        };
        let plan_path = dir.path().join("plan.json");
        write_json(&plan_path, &serde_json::to_value(&plan).unwrap()).unwrap();
        let out = dir.path().join("interim/catalog.json");
        interim_with_config(&cfg, &plan_path, &previous_dir, &out).unwrap();

        let doc: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
        assert_eq!(doc["sequence"], 5);
        let python = doc["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["id"] == "python")
            .unwrap();
        let versions: Vec<&str> = python["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|manifest| manifest["version"].as_str().unwrap())
            .collect();
        assert_eq!(versions, vec!["3.13.0", "3.12.1", "3.11.0"]);
        let detected = python["detected"].as_array().unwrap();
        assert_eq!(detected.len(), 1, "only 3.15.0 is announced: {detected:?}");
        assert_eq!(detected[0]["version"], "3.15.0");
        assert_eq!(detected[0]["channel"], "3.14");
        assert_eq!(detected[0]["state"], "verifying");
        assert!(detected[0]["detail"]
            .as_str()
            .unwrap()
            .contains("verifying checksums"));
        assert!(catalog_problems(&doc).unwrap().is_empty());
    }

    #[test]
    fn assemble_keeps_previous_targets_and_records_failed_versions() {
        let cfg = test_config();
        let dir = tempfile::tempdir().unwrap();
        let previous_dir = dir.path().join("previous");
        let old_312 = previous_manifest(&cfg, "3.12.1", &["linux-x86_64"]);
        let old_313 = previous_manifest(&cfg, "3.13.0", &["linux-x86_64", "linux-aarch64"]);
        write_previous_catalog(&previous_dir, 4, vec![old_312, old_313]);

        let mut carry = python_item_for("3.12.1", &["linux-x86_64"], 1);
        carry.status = "carry".into();
        // Re-verification of 3.13.0 covers fewer targets than the published
        // manifest, so the previous manifest must survive unchanged.
        let revalidated = python_item_for("3.13.0", &["linux-x86_64"], 2);
        let failing = python_item_for("3.15.0", &["linux-x86_64"], 1);
        let plan = Plan {
            generated_at: "2026-01-01T00:00:00Z".into(),
            previous_sequence: 4,
            withheld: Vec::new(),
            items: vec![carry, revalidated, failing],
            dropped: Vec::new(),
        };
        let plan_path = dir.path().join("plan.json");
        write_json(&plan_path, &serde_json::to_value(&plan).unwrap()).unwrap();
        let results_dir = dir.path().join("results");
        fs::create_dir_all(&results_dir).unwrap();
        let verified = python_outcome("3.13.0", "linux-x86_64");
        let results = json!([{
            "package": verified.package,
            "version": verified.version,
            "target": verified.target,
            "ok": true,
            "sha256": verified.sha256,
            "size": verified.size,
            "bundled": verified.bundled,
            "verification": verified.verification,
        }]);
        fs::write(
            results_dir.join("upstream-linux-x86_64.json"),
            serde_json::to_vec(&results).unwrap(),
        )
        .unwrap();
        let summary_path = dir.path().join("summary.md");
        // Only this test reads GITHUB_STEP_SUMMARY.
        env::set_var("GITHUB_STEP_SUMMARY", &summary_path);
        let out = dir.path().join("out/catalog.json");
        assemble_with_config(&cfg, &plan_path, &previous_dir, &results_dir, None, &out).unwrap();
        env::remove_var("GITHUB_STEP_SUMMARY");

        let doc: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
        assert_eq!(doc["sequence"], 6, "interim +1, final +2");
        let python = doc["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["id"] == "python")
            .unwrap();
        let versions = python["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0]["version"], "3.13.0");
        assert_eq!(
            versions[0]["revision"], 1,
            "the published manifest is kept, not replaced by revision 2"
        );
        assert_eq!(versions[0]["artifacts"].as_array().unwrap().len(), 2);
        assert_eq!(versions[1]["version"], "3.12.1");
        let detected = python["detected"].as_array().unwrap();
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0]["version"], "3.15.0");
        assert_eq!(detected[0]["state"], "failed");
        assert!(detected[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with("Verification failed: "));
        assert!(catalog_problems(&doc).unwrap().is_empty());

        let summary = fs::read_to_string(&summary_path).unwrap();
        assert!(summary.starts_with("### Catalog sequence 6\n\n"));
        assert!(summary
            .contains("- kept python 3.13.0 from the previous catalog (fewer targets verified)"));
        assert!(summary.contains(
            "- FAILED python 3.15.0: linux-x86_64: no result (job failed or was skipped)"
        ));
    }

    #[test]
    fn manifests_are_sorted_newest_first_with_revisions() {
        let mut packages = vec![(
            "python".to_string(),
            json!({
                "versions": [
                    {"version": "3.13.1", "revision": 1},
                    {"version": "3.14.0", "revision": 1},
                    {"version": "3.14.0", "revision": 2},
                ]
            }),
        )];
        sort_versions(&mut packages).unwrap();
        let versions = packages[0].1["versions"].as_array().unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|version| (
                    version["version"].as_str().unwrap(),
                    version["revision"].as_u64().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![("3.14.0", 2), ("3.14.0", 1), ("3.13.1", 1)]
        );
    }
}
