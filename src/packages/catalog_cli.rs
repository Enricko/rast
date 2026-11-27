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

use super::version::Version;
use super::{archive, authoring, catalog, install, manifest, net, version};

/// User agent for every request, identical to the Python reference.
const UA: &str = "rast-packages-catalog/1 (+https://github.com/Enricko/rast-packages)";
const GO_JSON: &str = "https://go.dev/dl/?mode=json&include=all";
const NODE_INDEX: &str = "https://nodejs.org/dist/index.json";
const NODE_KEYRING: &str =
    "https://raw.githubusercontent.com/nodejs/release-keys/HEAD/gpg/pubring.kbx";
const PBS_REPO: &str = "astral-sh/python-build-standalone";
const PHP_JSON: &str = "https://www.php.net/releases/index.php?json&version={channel}&max=1";
/// The signed Rust release channel manifest (TOML) for the stable channel.
const RUST_CHANNEL_TOML: &str = "https://static.rust-lang.org/dist/channel-rust-stable.toml";
/// The Rust release signing key (ASCII armored; pinned by fingerprint below).
const RUST_RELEASE_KEY: &str = "https://static.rust-lang.org/rust-key.gpg.ascii";
/// Fingerprint of the Rust release signing key (> 1.80 releases are signed
/// with it; a rotation needs a RAST update, which is deliberate).
const RUST_RELEASE_KEY_FINGERPRINT: &str = "108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE";
/// Rust dist components installed into one standalone toolchain prefix.
/// `(component name in the channel manifest, archive name prefix)`.
const RUST_COMPONENTS: &[(&str, &str)] = &[
    ("rustc", "rustc"),
    ("rust-std", "rust-std"),
    ("cargo", "cargo"),
    ("rustfmt-preview", "rustfmt"),
    ("clippy-preview", "clippy"),
];
/// Adoptium API: feature versions with assets.
const ADOPTIUM_AVAILABLE: &str = "https://api.adoptium.net/v3/info/available_releases";
/// Adoptium API: latest JDK assets of one feature version and architecture.
const ADOPTIUM_LATEST: &str = "https://api.adoptium.net/v3/assets/latest/{feature}/hotspot?architecture={arch}&image_type=jdk&os=linux&vendor=eclipse";
/// GitHub repositories whose stable releases provide self-contained binaries.
const BUN_REPO: &str = "oven-sh/bun";
/// Where AI Agent's App Center package was released before its source moved
/// to the package repository (`ai-agent-v<version>`); the default of
/// `packages.ai-agent.releaseRepos`.
const RAST_REPO: &str = "Enricko/rast";
const DENO_REPO: &str = "denoland/deno";
/// Deno attests with the in-toto release predicate, not SLSA provenance v1.
const DENO_RELEASE_PREDICATE: &str = "https://in-toto.io/attestation/release/v0.2";
/// .NET release metadata (the index points at each channel's releases.json).
const DOTNET_RELEASES_INDEX: &str =
    "https://dotnetcli.blob.core.windows.net/dotnet/release-metadata/releases-index.json";
/// LLVM/Clang official release binaries (`LLVM-<version>-Linux-<arch>.tar.xz`),
/// built on Ubuntu 22.04, with a SHA-256 in the GitHub asset digest and an
/// SLSA build-provenance bundle (`<asset>.jsonl`).
const CLANG_REPO: &str = "llvm/llvm-project";
/// Google's Flutter Linux release metadata (stable/beta/dev archives).
const FLUTTER_RELEASES_LINUX: &str =
    "https://storage.googleapis.com/flutter_infra_release/releases/releases_linux.json";
/// Official Flutter Linux archives, relative to this base.
const FLUTTER_ARCHIVE_BASE: &str = "https://storage.googleapis.com/flutter_infra_release/releases/";
/// Composer: a single-file phar per release, installed with a generated
/// `bin/composer` wrapper (GitHub asset digest + getcomposer.org SHA-256).
const COMPOSER_REPO: &str = "composer/composer";
/// Zig's official download index (per version and target: tarball, shasum, size).
const ZIG_INDEX: &str = "https://ziglang.org/download/index.json";
/// Kotlin command-line compiler releases (`kotlin-compiler-<version>.zip`).
const KOTLIN_REPO: &str = "JetBrains/kotlin";
/// CMake official release archives and their `SHA-256.txt` checksum file.
const CMAKE_REPO: &str = "Kitware/CMake";
/// Gradle's release metadata: one entry per version with download/checksum URLs.
const GRADLE_VERSIONS: &str = "https://services.gradle.org/versions/all";
/// Apache Maven's official archive directory listing (all Maven 3 versions).
const MAVEN_ARCHIVE: &str = "https://archive.apache.org/dist/maven/maven-3/";

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

    #[cfg(test)]
    fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        self.entries
            .iter_mut()
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
    /// Directory of the `catalog.config.json` that was loaded; package `icon`
    /// paths are relative to it. Never serialized/deserialized.
    #[serde(skip)]
    config_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetConfig {
    runner: String,
    go: String,
    node: String,
    python: String,
    /// Rust target triple, e.g. `x86_64-unknown-linux-gnu`.
    #[serde(default)]
    rust_triple: Option<String>,
    /// Adoptium architecture name (`x64`, `aarch64`).
    #[serde(default)]
    java_arch: Option<String>,
    /// Bun release architecture name (`x64`, `aarch64`).
    #[serde(default)]
    bun_arch: Option<String>,
    /// Deno release architecture name (`x86_64`, `aarch64`).
    #[serde(default)]
    deno_arch: Option<String>,
    /// .NET runtime identifier (`linux-x64`, `linux-arm64`).
    #[serde(default)]
    dotnet_rid: Option<String>,
    /// LLVM Linux asset architecture (`X64`, `ARM64`).
    #[serde(default)]
    clang_arch: Option<String>,
    /// Flutter Linux host architecture (`x64`; Google publishes no arm64
    /// Linux SDK archive, so linux-aarch64 maps to `arm64` and is skipped).
    #[serde(default)]
    flutter_arch: Option<String>,
    /// CMake Linux archive architecture (`x86_64`, `aarch64`).
    #[serde(default)]
    cmake_arch: Option<String>,
    /// Zig Linux target architecture (`x86_64`, `aarch64`).
    #[serde(default)]
    zig_arch: Option<String>,
}

impl TargetConfig {
    fn rust_triple(&self, target: &str) -> Result<&str, String> {
        self.rust_triple
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.rustTriple"))
    }
    fn java_arch(&self, target: &str) -> Result<&str, String> {
        self.java_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.javaArch"))
    }
    fn bun_arch(&self, target: &str) -> Result<&str, String> {
        self.bun_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.bunArch"))
    }
    fn deno_arch(&self, target: &str) -> Result<&str, String> {
        self.deno_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.denoArch"))
    }
    fn dotnet_rid(&self, target: &str) -> Result<&str, String> {
        self.dotnet_rid
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.dotnetRid"))
    }
    fn clang_arch(&self, target: &str) -> Result<&str, String> {
        self.clang_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.clangArch"))
    }
    fn flutter_arch(&self, target: &str) -> Result<&str, String> {
        self.flutter_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.flutterArch"))
    }
    fn cmake_arch(&self, target: &str) -> Result<&str, String> {
        self.cmake_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.cmakeArch"))
    }
    fn zig_arch(&self, target: &str) -> Result<&str, String> {
        self.zig_arch
            .as_deref()
            .ok_or_else(|| format!("catalog.config.json has no targets.{target}.zigArch"))
    }
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
    /// Ask `gh attestation verify` for the release artifacts of this package.
    /// Only enable it for a project that publishes GitHub build-provenance
    /// attestations (a missing attestation fails the verification job).
    #[serde(default)]
    github_attestations: bool,
    /// Package icon file, relative to `catalog.config.json` (or absolute):
    /// `.svg` or `.png`, at most 64 KiB decoded. Embedded base64 into the
    /// package entry of every catalog the workflow publishes.
    #[serde(default)]
    icon: Option<String>,
    /// An `app` package: its manifests' `app` section (window, server).
    /// Without it the package is a runtime.
    #[serde(default)]
    app: Option<Value>,
    /// Oldest RAST this package's versions need (default: the catalog's).
    #[serde(default)]
    min_rast_version: Option<String>,
    /// GitHub repositories (`owner/name`) whose releases provide an app
    /// package's versions, newest home first; a version found in several is
    /// taken from the first. AI Agent: default [`RAST_REPO`].
    #[serde(default)]
    release_repos: Vec<String>,
    channels: Vec<Value>,
}

impl PackageConfig {
    /// The package type in manifests and catalog entries.
    fn package_type(&self) -> &'static str {
        if self.app.is_some() {
            "app"
        } else {
            "runtime"
        }
    }

    /// Absolute path of the configured icon file, if any.
    fn icon_path(&self, config_dir: &Path) -> Option<PathBuf> {
        self.icon.as_deref().map(|icon| {
            let path = Path::new(icon);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                config_dir.join(path)
            }
        })
    }
}

fn load_config() -> Result<Config, String> {
    load_config_from(Path::new("catalog.config.json"))
}

/// Read the catalog configuration from `path` and remember its directory so
/// package `icon` paths resolve relative to the config file (not the current
/// directory).
fn load_config_from(path: &Path) -> Result<Config, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut config: Config = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not valid: {e}", path.display()))?;
    config.config_dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    Ok(config)
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

/// The update stream: Node.js uses the major version, JDKs the feature
/// version, .NET the major.minor band, and Rust/Bun/Deno the rolling stable
/// channel; the others use the version's implicit major.minor.
fn channel_of(package: &str, version: &str) -> Option<String> {
    match package {
        "nodejs" => Some(version.split('.').next().unwrap_or("").to_string()),
        "java" => Some(
            version
                .split('+')
                .next()
                .unwrap_or(version)
                .split('.')
                .next()
                .unwrap_or("")
                .to_string(),
        ),
        "dotnet" => {
            let mut parts = version.split('.');
            match (parts.next(), parts.next()) {
                (Some(major), Some(minor)) => Some(format!("{major}.{minor}")),
                _ => None,
            }
        }
        "rust" | "bun" | "deno" | "clang" | "flutter" | "composer" | "zig" | "kotlin" | "cmake"
        | "gradle" | "ai-agent" => Some("stable".to_string()),
        "maven" => {
            let mut parts = version.split('.');
            match (parts.next(), parts.next()) {
                (Some(major), Some(minor)) => Some(format!("{major}.{minor}")),
                _ => None,
            }
        }
        _ => None,
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

/// Bytes available to unprivileged users on the filesystem holding `path`.
/// Verification extracts into a temporary directory; never let one artifact
/// fill the disk (the install path has the same guard, plus the manifest's
/// pinned size and SHA-256).
fn available_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: valid NUL-terminated path and a zeroed out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
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

/// One archive of a target in the plan. Rust installs several components; the
/// other providers use only the target's primary archive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetArchive {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// .NET release metadata publishes SHA-512, not SHA-256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha512: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// Checksums/signature document that pins this archive (SHASUMS256.txt,
    /// .sha256.txt, …), fetched and parsed during verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sums_url: Option<String>,
    /// Archive format (`tar.gz`; `zip` for Bun/Deno).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strip_components: Option<u32>,
    /// Additional archives extracted into the same prefix (Rust components).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archives: Vec<TargetArchive>,
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
    /// This package's releases publish GitHub build-provenance attestations
    /// (from the package config); the verifier runs `gh attestation verify`.
    #[serde(default)]
    attestations: bool,
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

// ---------------------------------------------------- new upstream providers

fn is_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// `1.100.0 (0ef1bc386 2026-11-09)` -> `1.100.0`.
fn rust_version(raw: &str) -> Option<String> {
    let core = raw.split_whitespace().next()?;
    Version::parse(core).map(|_| core.to_string())
}

fn toml_table<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Table> {
    value.get(key)?.as_table()
}

/// Download and integrity-check the signed Rust stable channel manifest.
///
/// The manifest's SHA-256 is checked against the dist server's `.sha256`
/// file; [`verify_item`] additionally checks its OpenPGP signature. Discovery
/// only needs the cheap hash so a corrupt manifest never reaches a plan.
fn rust_channel_manifest() -> Result<(toml::Value, String, String), String> {
    let bytes = http_get(RUST_CHANNEL_TOML, None, None)?;
    let sums = String::from_utf8(http_get(
        &format!("{RUST_CHANNEL_TOML}.sha256"),
        None,
        None,
    )?)
    .map_err(|_| "Rust channel manifest .sha256 is not UTF-8".to_string())?;
    let expected = sums
        .split_whitespace()
        .next()
        .ok_or("Rust channel manifest .sha256 is empty")?;
    let actual = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    };
    if actual != expected {
        return Err(format!(
            "Rust channel manifest SHA-256 is {actual}, the dist server publishes {expected}"
        ));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| "Rust channel manifest is not UTF-8".to_string())?;
    let doc: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("Rust channel manifest is not valid TOML: {error}"))?;
    Ok((doc, actual, text))
}

/// Discover the Rust stable toolchain from the signed dist channel manifest.
/// The five components are installed into one standalone prefix: the artifact
/// is the `rustc` archive and `archives` lists rust-std, cargo, rustfmt and
/// clippy.
fn discover_rust(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let channels = channel_ids(package);
    if channels != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.rust.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let (doc, manifest_sha256, _text) = rust_channel_manifest()?;
    rust_plan_from_channel(cfg, package, &doc, &manifest_sha256, held)
}

/// Pure part of the Rust provider: build the plan item from the parsed
/// channel manifest (unit-tested against a recorded TOML fixture).
fn rust_plan_from_channel(
    cfg: &Config,
    package: &PackageConfig,
    doc: &toml::Value,
    manifest_sha256: &str,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let _ = package;
    let pkgs = toml_table(doc, "pkg").ok_or("Rust channel manifest has no [pkg] tables")?;
    let rustc = pkgs
        .get("rustc")
        .ok_or("Rust channel manifest has no pkg.rustc")?;
    let version = rustc
        .get("version")
        .and_then(toml::Value::as_str)
        .and_then(rust_version)
        .ok_or("Rust channel manifest has no rustc version")?;
    if held.contains(&format!("rust@{version}")) {
        return Ok(Vec::new());
    }
    let date = doc
        .get("date")
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let triple = target_config.rust_triple(target)?;
        let mut parts: Vec<TargetArchive> = Vec::new();
        let mut complete = true;
        for (component, archive_prefix) in RUST_COMPONENTS {
            let entry = pkgs
                .get(*component)
                .and_then(|pkg| pkg.get("target"))
                .and_then(toml::Value::as_table)
                .and_then(|targets| targets.get(triple))
                .and_then(toml::Value::as_table);
            let Some(entry) = entry else {
                log(format!(
                    "rust: component {component} has no {triple} build; {target} is not published"
                ));
                complete = false;
                break;
            };
            if entry.get("available").and_then(toml::Value::as_bool) != Some(true) {
                log(format!(
                    "rust: component {component} is unavailable for {triple}; {target} is not published"
                ));
                complete = false;
                break;
            }
            let url = entry
                .get("url")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("rust: {component} {triple} has no url"))?;
            let sha256 = entry
                .get("hash")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("rust: {component} {triple} has no hash"))?;
            let expected_prefix = format!("{archive_prefix}-{version}-");
            let file = url
                .rsplit('/')
                .next()
                .filter(|name| name.starts_with(&expected_prefix))
                .ok_or_else(|| {
                    format!("rust: {component} {triple} URL {url:?} is not the {version} archive")
                })?
                .to_string();
            parts.push(TargetArchive {
                name: Some((*component).to_string()),
                url: url.to_string(),
                sha256: Some(sha256.to_string()),
                size: None,
                file: Some(file),
            });
        }
        if !complete {
            continue;
        }
        let primary = parts.remove(0);
        let primary_sha256 = primary
            .sha256
            .clone()
            .ok_or("rust: rustc component has no hash")?;
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(primary.url),
                sha256: Some(primary_sha256),
                file: primary.file,
                source: Some(RUST_CHANNEL_TOML.to_string()),
                format: Some("tar.gz".into()),
                strip_components: Some(2),
                archives: parts,
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        log(format!(
            "rust {version}: no configured target has a complete component set; not published"
        ));
        return Ok(Vec::new());
    }
    Ok(vec![PlanItem {
        package: "rust".into(),
        version,
        channel: "stable".into(),
        kind: "upstream".into(),
        release_date: Some(date),
        revision: 1,
        sums_url: Some(format!("{RUST_CHANNEL_TOML}.sha256")),
        source: Some(json!({
            "channelManifest": RUST_CHANNEL_TOML,
            "channelManifestSha256": manifest_sha256,
        })),
        targets,
        ..PlanItem::default()
    }])
}

/// One JDK asset from the Adoptium API.
#[derive(Debug, Clone)]
struct AdoptiumAsset {
    release_name: String,
    release_link: String,
    semver: String,
    openjdk_version: String,
    link: String,
    name: String,
    checksum: String,
    size: u64,
    checksum_link: Option<String>,
    updated_at: Option<String>,
}

fn adoptium_asset(assets: &[Value], arch: &str) -> Result<AdoptiumAsset, String> {
    for value in assets {
        let Some(binary) = value.get("binary") else {
            continue;
        };
        let field = |name: &str| binary.get(name).and_then(Value::as_str).unwrap_or("");
        if field("os") != "linux"
            || field("image_type") != "jdk"
            || field("jvm_impl") != "hotspot"
            || field("architecture") != arch
            || value.get("vendor").and_then(Value::as_str) != Some("eclipse")
        {
            continue;
        }
        let Some(package) = binary.get("package") else {
            continue;
        };
        let name = required_str(package, "name")?;
        if !name.ends_with(".tar.gz") || name.contains("alpine") || name.contains("debugimage") {
            continue;
        }
        let version = value
            .get("version")
            .ok_or("Adoptium asset has no version")?;
        let semver = required_str(version, "semver")?.to_string();
        let openjdk_version = required_str(version, "openjdk_version")?.to_string();
        return Ok(AdoptiumAsset {
            release_name: required_str(value, "release_name")?.to_string(),
            release_link: required_str(value, "release_link")?.to_string(),
            semver,
            openjdk_version,
            link: required_str(package, "link")?.to_string(),
            name: name.to_string(),
            checksum: required_str(package, "checksum")?.to_string(),
            size: package
                .get("size")
                .and_then(Value::as_u64)
                .ok_or("Adoptium asset has no size")?,
            checksum_link: package
                .get("checksum_link")
                .and_then(Value::as_str)
                .map(str::to_string),
            updated_at: binary
                .get("updated_at")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Err(format!(
        "Adoptium API asset list has no linux {arch} hotspot JDK tar.gz"
    ))
}

/// The GitHub-computed digest/size of the Temurin release asset, from the
/// repository the Adoptium API links to. This is a second, independent source
/// for the checksum (GitHub computes it on upload).
fn adoptium_github_asset(
    feature: &str,
    release_name: &str,
    asset_name: &str,
    token: Option<&str>,
) -> Result<(String, u64, String), String> {
    let tag = release_name.replace('+', "%2B");
    let url = format!(
        "https://api.github.com/repos/adoptium/temurin{feature}-binaries/releases/tags/{tag}"
    );
    let release = http_json(&url, token)?;
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("GitHub release {tag} has no assets"))?;
    let asset = assets
        .iter()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(asset_name))
        .ok_or_else(|| format!("GitHub release {tag} has no asset {asset_name}"))?;
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or_else(|| format!("GitHub release asset {asset_name} has no SHA-256 digest"))?;
    let size = asset
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("GitHub release asset {asset_name} has no size"))?;
    Ok((
        digest.to_string(),
        size,
        required_str(&release, "html_url")?.to_string(),
    ))
}

/// `25.0.5+8.0.LTS` -> `25.0.5+8` (the manifest version; build metadata keeps
/// only the numeric build, so versions sort by the JDK patch).
fn java_manifest_version(semver: &str) -> Result<String, String> {
    let (core, build) = match semver.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (semver, None),
    };
    if Version::parse(core).is_none() {
        return Err(format!("Adoptium semver {semver:?} has no version core"));
    }
    let build: String = build
        .unwrap_or("")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if build.is_empty() {
        Ok(core.to_string())
    } else {
        Ok(format!("{core}+{build}"))
    }
}

/// The version a JDK's `java --version` / `javac -version` prints:
/// `25.0.5+8` -> `25.0.5`, JDK 8's `8.0.512+9` -> `1.8.0_512`.
fn java_display_version(version: &str) -> String {
    let core = version.split('+').next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.first() == Some(&"8") {
        format!("1.8.0_{}", parts.get(2).copied().unwrap_or("0"))
    } else {
        parts.iter().take(3).copied().collect::<Vec<_>>().join(".")
    }
}

/// Discover Eclipse Temurin JDKs: one latest asset per configured feature
/// version, from the Adoptium API, cross-checked against the GitHub release
/// asset digest.
/// One verified Temurin asset of one target, ready for the plan.
#[derive(Debug, Clone)]
struct JavaAssetPlan {
    target: String,
    asset: AdoptiumAsset,
    digest: String,
    size: u64,
    html_url: String,
}

fn discover_java(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    let available = http_json(ADOPTIUM_AVAILABLE, None)?;
    let available: BTreeSet<String> = available
        .get("available_releases")
        .and_then(Value::as_array)
        .map(|versions| {
            versions
                .iter()
                .filter_map(Value::as_u64)
                .map(|version| version.to_string())
                .collect()
        })
        .ok_or("Adoptium available_releases is not a list")?;
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        if !available.contains(&channel) {
            log(format!(
                "java: feature version {channel} is not in Adoptium available_releases; skipping"
            ));
            continue;
        }
        let mut found: Vec<JavaAssetPlan> = Vec::new();
        let mut failure: Option<String> = None;
        for (target, target_config) in &cfg.targets.entries {
            let arch = target_config.java_arch(target)?;
            let url = ADOPTIUM_LATEST
                .replace("{feature}", &channel)
                .replace("{arch}", arch);
            let assets = http_json(&url, None)?;
            let array = assets
                .as_array()
                .ok_or_else(|| format!("{url} did not return a JSON array"))?;
            let asset = match adoptium_asset(array, arch) {
                Ok(asset) => asset,
                Err(error) => {
                    log(format!("java {channel} {target}: {error}"));
                    failure = Some(format!("{target}: {error}"));
                    continue;
                }
            };
            let (digest, github_size, html_url) =
                adoptium_github_asset(&channel, &asset.release_name, &asset.name, token)?;
            if digest != asset.checksum {
                return Err(format!(
                    "java {}: Adoptium API sha256 {} differs from the GitHub asset digest {digest}",
                    asset.name, asset.checksum
                ));
            }
            if github_size != asset.size {
                return Err(format!(
                    "java {}: Adoptium API size {} differs from the GitHub asset size {github_size}",
                    asset.name, asset.size
                ));
            }
            found.push(JavaAssetPlan {
                target: target.clone(),
                asset,
                digest,
                size: github_size,
                html_url,
            });
        }
        match java_plan_from_assets(package, &channel, &found, held)? {
            Some(item) => items.push(item),
            None => {
                if let Some(failure) = failure {
                    log(format!("java {channel}: no asset ({failure})"));
                }
            }
        }
    }
    Ok(items)
}

/// Pure part of the Temurin provider: one plan item from the per-target
/// assets (unit-tested against recorded Adoptium/GitHub JSON).
fn java_plan_from_assets(
    package: &PackageConfig,
    channel: &str,
    found: &[JavaAssetPlan],
    held: &BTreeSet<String>,
) -> Result<Option<PlanItem>, String> {
    let _ = package;
    let Some(first) = found.first() else {
        return Ok(None);
    };
    let version = java_manifest_version(&first.asset.semver)?;
    if held.contains(&format!("java@{version}")) {
        return Ok(None);
    }
    let mut targets = BTreeMap::new();
    for plan in found {
        let target_version = java_manifest_version(&plan.asset.semver)?;
        if target_version != version {
            log(format!(
                "java {channel} {}: upstream reports {target_version}, other targets report {version}; skipping {}",
                plan.target, plan.target
            ));
            continue;
        }
        targets.insert(
            plan.target.clone(),
            TargetInfo {
                url: Some(plan.asset.link.clone()),
                sha256: Some(plan.asset.checksum.clone()),
                size: Some(plan.size),
                file: Some(plan.asset.name.clone()),
                digest: Some(plan.digest.clone()),
                source: Some(plan.html_url.clone()),
                sums_url: plan.asset.checksum_link.clone(),
                format: Some("tar.gz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "java".into(),
        version,
        channel: channel.to_string(),
        kind: "upstream".into(),
        revision: 1,
        release_date: first
            .asset
            .updated_at
            .as_deref()
            .map(|stamp| truncate_chars(stamp, 10)),
        source: Some(json!({
            "release": first.asset.release_link,
            "temurinVersion": first.asset.openjdk_version,
        })),
        targets,
        ..PlanItem::default()
    }))
}

/// Stable GitHub releases of one repository, newest first.
fn github_stable_releases(repo: &str, token: Option<&str>) -> Result<Vec<Value>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let data = http_json(&url, token)?;
    let releases = data
        .as_array()
        .ok_or_else(|| format!("{url} did not return a JSON array"))?
        .iter()
        .filter(|release| {
            !truthy(release.get("prerelease"))
                && !truthy(release.get("draft"))
                && release.get("tag_name").and_then(Value::as_str).is_some()
        })
        .cloned()
        .collect();
    Ok(releases)
}

/// Bun stable releases: a self-contained zip per architecture, one `bun`
/// executable at the top-level directory.
fn discover_bun(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    let channels = channel_ids(package);
    if channels != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.bun.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let releases = github_stable_releases(BUN_REPO, token)?;
    bun_plan_from_releases(cfg, package, &releases, held)
}

/// Pure part of the Bun provider, unit-tested against recorded GitHub release
/// JSON.
fn bun_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        // Releases older than GitHub's asset-digest feature cannot be pinned;
        // skip them (with a note) instead of failing the whole channel.
        match bun_plan_from_release(cfg, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("bun@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "bun: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    let _ = package;
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn bun_plan_from_release(cfg: &Config, release: &Value) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let Some(version) = tag.strip_prefix("bun-v") else {
        return Ok(None);
    };
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("Bun release has no assets")?;
    let find_asset = |name: &str| {
        assets
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
    };
    let Some(sums_asset) = find_asset("SHASUMS256.txt") else {
        return Err(format!("Bun release {tag} publishes no SHASUMS256.txt"));
    };
    let sums_url = required_str(sums_asset, "browser_download_url")?.to_string();
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.bun_arch(target)?;
        let name = format!("bun-linux-{arch}.zip");
        let Some(asset) = find_asset(&name) else {
            log(format!(
                "bun {version}: release {tag} has no {name}; not published for {target}"
            ));
            continue;
        };
        let digest = asset
            .get("digest")
            .and_then(Value::as_str)
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .ok_or_else(|| format!("Bun asset {name} has no SHA-256 digest"))?
            .to_string();
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(required_str(asset, "browser_download_url")?.to_string()),
                sha256: Some(digest.clone()),
                size: Some(
                    asset
                        .get("size")
                        .and_then(Value::as_u64)
                        .ok_or("Bun asset has no size")?,
                ),
                file: Some(name),
                digest: Some(digest),
                source: Some(required_str(release, "html_url")?.to_string()),
                sums_url: Some(sums_url.clone()),
                format: Some("zip".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "bun".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release
            .get("published_at")
            .and_then(Value::as_str)
            .map(|stamp| truncate_chars(stamp, 10)),
        sums_url: Some(sums_url),
        targets,
        ..PlanItem::default()
    }))
}

fn discover_ai_agent(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.ai-agent.channels must be exactly [{id: \"stable\"}]"
                .into(),
        );
    }
    if package.app.is_none() {
        return Err("catalog.config.json packages.ai-agent needs its app section".into());
    }
    let mut releases = Vec::new();
    for repo in ai_agent_repos(package) {
        releases.extend(github_stable_releases(&repo, token)?);
    }
    ai_agent_plan_from_releases(cfg, package, &releases, held)
}

/// The repositories AI Agent's releases come from.
fn ai_agent_repos(package: &PackageConfig) -> Vec<String> {
    if package.release_repos.is_empty() {
        vec![RAST_REPO.to_string()]
    } else {
        package.release_repos.clone()
    }
}

/// `owner/name` of a GitHub release page (`https://github.com/owner/name/releases/…`).
fn github_repo_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(3, '/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name) && parts.next()?.starts_with("releases/"))
        .then(|| format!("{owner}/{name}"))
}

/// Pure part of the AI Agent provider: RAST's `ai-agent-v<version>` releases
/// with one `ai-agent-<version>-<target>.tar.gz` per target and SHA256SUMS.
fn ai_agent_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found: Vec<PlanItem> = Vec::new();
    for release in releases {
        match ai_agent_plan_from_release(cfg, package, release) {
            Ok(Some(item)) => {
                let known = found.iter().any(|f| f.version == item.version);
                if !known && !held.contains(&format!("ai-agent@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "ai-agent: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches.max(1) as usize);
    Ok(found)
}

fn ai_agent_plan_from_release(
    cfg: &Config,
    package: &PackageConfig,
    release: &Value,
) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let Some(version) = tag.strip_prefix("ai-agent-v") else {
        return Ok(None);
    };
    match Version::parse(version) {
        Some(parsed) if !parsed.is_prerelease() => {}
        _ => return Ok(None),
    }
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("the release has no assets")?;
    let find_asset = |name: &str| {
        assets
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
    };
    let sums_asset =
        find_asset("SHA256SUMS").ok_or_else(|| format!("release {tag} has no SHA256SUMS"))?;
    let sums_url = required_str(sums_asset, "browser_download_url")?.to_string();
    let mut targets = BTreeMap::new();
    for target in cfg.targets.entries.iter().map(|(target, _)| target) {
        let name = format!("ai-agent-{version}-{target}.tar.gz");
        let Some(asset) = find_asset(&name) else {
            log(format!("ai-agent {version}: release {tag} has no {name}"));
            continue;
        };
        let digest = asset
            .get("digest")
            .and_then(Value::as_str)
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .ok_or_else(|| format!("asset {name} has no SHA-256 digest"))?
            .to_string();
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(required_str(asset, "browser_download_url")?.to_string()),
                sha256: Some(digest.clone()),
                size: Some(
                    asset
                        .get("size")
                        .and_then(Value::as_u64)
                        .ok_or("the asset has no size")?,
                ),
                file: Some(name),
                digest: Some(digest),
                source: Some(required_str(release, "html_url")?.to_string()),
                sums_url: Some(sums_url.clone()),
                format: Some("tar.gz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "ai-agent".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release
            .get("published_at")
            .and_then(Value::as_str)
            .map(|stamp| truncate_chars(stamp, 10)),
        sums_url: Some(sums_url),
        // The verifier checks the files the app section names.
        source: Some(json!({ "app": package.app })),
        targets,
        attestations: package.github_attestations,
        ..PlanItem::default()
    }))
}

/// An app package's files are where its `app` section says, and its server
/// program reports the version (`<program> --version`).
fn check_app_files(
    root: &Path,
    app: &Value,
    version: &str,
    home: &Path,
) -> Result<Vec<String>, String> {
    let window = app.get("window").ok_or("the app section has no window")?;
    let mut files: Vec<&str> = vec![required_str(window, "entry")?];
    if let Some(styles) = window.get("styles").and_then(Value::as_array) {
        files.extend(styles.iter().filter_map(Value::as_str));
    }
    for file in files {
        let path = archive::file_inside(root, file)
            .map_err(|error| format!("window file {file}: {error}"))?;
        if !path.is_file() {
            return Err(format!("the archive has no window file {file}"));
        }
    }
    let mut checks = Vec::new();
    if let Some(server) = app.get("server") {
        let executable = required_str(server, "executable")?;
        if !is_executable_file(&root.join(executable)) {
            return Err(format!("the server program {executable} is missing"));
        }
        checks.push(run_check(
            root,
            executable,
            &["--version".to_string()],
            &format!(" {version}"),
            home,
            &[],
        )?);
    }
    Ok(checks)
}

fn deno_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        match deno_plan_from_release(cfg, package, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("deno@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "deno: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    let _ = package;
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn deno_plan_from_release(
    cfg: &Config,
    package: &PackageConfig,
    release: &Value,
) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let Some(version) = tag.strip_prefix('v') else {
        return Ok(None);
    };
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("Deno release has no assets")?;
    let find_asset = |name: &str| {
        assets
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
    };
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.deno_arch(target)?;
        let name = format!("deno-{arch}-unknown-linux-gnu.zip");
        let Some(asset) = find_asset(&name) else {
            log(format!(
                "deno {version}: release {tag} has no {name}; not published for {target}"
            ));
            continue;
        };
        let digest = asset
            .get("digest")
            .and_then(Value::as_str)
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .ok_or_else(|| format!("Deno asset {name} has no SHA-256 digest"))?
            .to_string();
        let sums_name = format!("{name}.sha256sum");
        let Some(sums_asset) = find_asset(&sums_name) else {
            return Err(format!("Deno release {tag} publishes no {sums_name}"));
        };
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(required_str(asset, "browser_download_url")?.to_string()),
                sha256: Some(digest.clone()),
                size: Some(
                    asset
                        .get("size")
                        .and_then(Value::as_u64)
                        .ok_or("Deno asset has no size")?,
                ),
                file: Some(name),
                digest: Some(digest),
                source: Some(required_str(release, "html_url")?.to_string()),
                sums_url: Some(required_str(sums_asset, "browser_download_url")?.to_string()),
                format: Some("zip".into()),
                strip_components: Some(0),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "deno".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        attestations: package.github_attestations,
        release_date: release
            .get("published_at")
            .and_then(Value::as_str)
            .map(|stamp| truncate_chars(stamp, 10)),
        targets,
        ..PlanItem::default()
    }))
}

/// Deno stable releases: a zip per target, a single `deno` executable at the
/// archive root, plus a per-asset `.sha256sum` file and GitHub build
/// provenance attestations.
fn discover_deno(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    let channels = channel_ids(package);
    if channels != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.deno.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let releases = github_stable_releases(DENO_REPO, token)?;
    deno_plan_from_releases(cfg, package, &releases, held)
}

fn discover_dotnet(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let index = http_json(DOTNET_RELEASES_INDEX, None)?;
    let channels = index
        .get("releases-index")
        .and_then(Value::as_array)
        .ok_or("the .NET releases index has no releases-index list")?;
    let mut items = Vec::new();
    for channel in channel_ids(package) {
        let Some(entry) = channels
            .iter()
            .find(|entry| entry.get("channel-version").and_then(Value::as_str) == Some(&channel))
        else {
            log(format!(
                ".NET channel {channel}: not in the releases index (EOL or removed); skipping"
            ));
            continue;
        };
        let support = entry
            .get("support-phase")
            .and_then(Value::as_str)
            .unwrap_or("");
        if support == "eol" {
            log(format!(
                ".NET channel {channel}: end of life upstream; not published"
            ));
            continue;
        }
        let releases_url = required_str(entry, "releases.json")?;
        let releases = http_json(releases_url, None)?;
        if let Some(item) =
            dotnet_plan_from_release(cfg, package, &channel, entry, &releases, held)?
        {
            items.push(item);
        }
    }
    Ok(items)
}

/// Pure part of the .NET provider: one plan item from a channel's release
/// metadata (unit-tested against recorded releases-index/releases JSON).
fn dotnet_plan_from_release(
    cfg: &Config,
    package: &PackageConfig,
    channel: &str,
    entry: &Value,
    releases: &Value,
    held: &BTreeSet<String>,
) -> Result<Option<PlanItem>, String> {
    let _ = package;
    let releases_url = required_str(entry, "releases.json")?;
    let list = releases
        .get("releases")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{releases_url} has no releases list"))?;
    let latest_sdk = entry
        .get("latest-sdk")
        .and_then(Value::as_str)
        .unwrap_or("");
    let release = list
        .iter()
        .find(|release| {
            release
                .get("sdk")
                .and_then(|sdk| sdk.get("version"))
                .and_then(Value::as_str)
                == Some(latest_sdk)
        })
        .or_else(|| list.first())
        .ok_or_else(|| format!("{releases_url} lists no releases"))?;
    let sdk = release
        .get("sdk")
        .ok_or("a .NET release has no sdk section")?;
    let version = required_str(sdk, "version")?.to_string();
    if held.contains(&format!("dotnet@{version}")) {
        return Ok(None);
    }
    let runtime_version = release
        .get("runtime")
        .and_then(|runtime| runtime.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let support = entry
        .get("support-phase")
        .and_then(Value::as_str)
        .unwrap_or("");
    let release_date = release
        .get("release-date")
        .and_then(Value::as_str)
        .or_else(|| entry.get("latest-release-date").and_then(Value::as_str))
        .map(|stamp| truncate_chars(stamp, 10));
    let files = sdk
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{releases_url} SDK release has no files"))?;
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let rid = target_config.dotnet_rid(target)?;
        let file = files.iter().find(|file| {
            file.get("rid").and_then(Value::as_str) == Some(rid)
                && file
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.ends_with(".tar.gz"))
        });
        let Some(file) = file else {
            log(format!(
                ".NET {version}: release metadata has no {rid} SDK tar.gz; not published for {target}"
            ));
            continue;
        };
        let hash = required_str(file, "hash")?;
        if !is_sha512(hash) {
            return Err(format!(
                ".NET {version} {rid}: release metadata hash is not a SHA-512 digest"
            ));
        }
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(required_str(file, "url")?.to_string()),
                sha512: Some(hash.to_string()),
                file: file.get("name").and_then(Value::as_str).map(str::to_string),
                source: Some(releases_url.to_string()),
                format: Some("tar.gz".into()),
                strip_components: Some(0),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "dotnet".into(),
        version,
        channel: channel.to_string(),
        kind: "upstream".into(),
        revision: 1,
        release_date,
        source: Some(json!({
            "url": releases_url,
            "runtime": runtime_version,
            "support": support,
        })),
        targets,
        ..PlanItem::default()
    }))
}

// ------------------------------------------- clang, flutter, composer, …

/// A GitHub release asset by exact name.
fn release_asset<'a>(assets: &'a [Value], name: &str) -> Option<&'a Value> {
    assets
        .iter()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
}

fn release_assets(release: &Value) -> Result<&[Value], String> {
    release
        .get("assets")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| "the GitHub release has no assets".to_string())
}

fn asset_url(asset: &Value) -> Result<String, String> {
    Ok(required_str(asset, "browser_download_url")?.to_string())
}

fn asset_size(asset: &Value, label: &str) -> Result<u64, String> {
    asset
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("GitHub asset {label} has no size"))
}

fn asset_sha256(asset: &Value, label: &str) -> Result<String, String> {
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or_else(|| format!("GitHub asset {label} has no SHA-256 digest"))?;
    if !is_sha256_hex(digest) {
        return Err(format!(
            "GitHub asset {label} digest is not a SHA-256 hex string"
        ));
    }
    Ok(digest.to_string())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn is_sha512_hex(value: &str) -> bool {
    value.len() == 128
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn release_published_date(release: &Value) -> Option<String> {
    release
        .get("published_at")
        .and_then(Value::as_str)
        .map(|stamp| truncate_chars(stamp, 10))
}

/// HTTP `Content-Length` of `url` (`HEAD`; redirects must stay https).
fn http_content_length(url: &str) -> Result<u64, String> {
    let agent = http_agent(Duration::from_secs(60));
    let response = agent
        .head(url)
        .call()
        .map_err(|error| format!("HEAD {url} failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered HTTP {status} to a HEAD request"));
    }
    ensure_final_https(url, &response)?;
    response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("{url} has no Content-Length"))
}

/// The verified phar file of a package with `format: "phar"` artifacts.
fn phar_path_for(package: &str) -> Option<&'static str> {
    match package {
        "composer" => Some("composer.phar"),
        _ => None,
    }
}

/// Why a configured target has no artifact for a package. Used in the
/// manifest notes so customers get an explicit compatibility reason.
fn unavailable_reason(package: &str, target: &str) -> Option<&'static str> {
    match (package, target) {
        ("flutter", "linux-aarch64") => Some(
            "Google publishes the Flutter Linux SDK for x86_64 only; there is no official linux-aarch64 host archive",
        ),
        _ => None,
    }
}

// ---------------------------------------------------------- clang / llvm

/// LLVM's official Linux release binaries: one `tar.xz` per architecture with
/// clang, clang++, lld, lldb and the libc++ headers/libraries, plus a
/// `<asset>.jsonl` SLSA build-provenance bundle.
fn discover_clang(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.clang.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let releases = github_stable_releases(CLANG_REPO, token)?;
    clang_plan_from_releases(cfg, package, &releases, held)
}

fn clang_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        match clang_plan_from_release(cfg, package, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("clang@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "clang: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn clang_plan_from_release(
    cfg: &Config,
    package: &PackageConfig,
    release: &Value,
) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let Some(version) = tag.strip_prefix("llvmorg-") else {
        return Ok(None);
    };
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release_assets(release)?;
    let page = required_str(release, "html_url")?.to_string();
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.clang_arch(target)?;
        let name = format!("LLVM-{version}-Linux-{arch}.tar.xz");
        let Some(asset) = release_asset(assets, &name) else {
            log(format!(
                "clang {version}: release {tag} has no {name}; not published for {target}"
            ));
            continue;
        };
        let sums_name = format!("{name}.jsonl");
        let Some(bundle) = release_asset(assets, &sums_name) else {
            return Err(format!("Clang release {tag} publishes no {sums_name}"));
        };
        let digest = asset_sha256(asset, &name)?;
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(asset_url(asset)?),
                sha256: Some(digest.clone()),
                size: Some(asset_size(asset, &name)?),
                file: Some(name),
                digest: Some(digest),
                source: Some(page.clone()),
                sums_url: Some(asset_url(bundle)?),
                format: Some("tar.xz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "clang".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        attestations: package.github_attestations,
        release_date: release_published_date(release),
        targets,
        ..PlanItem::default()
    }))
}

/// Validate the DSSE payload of LLVM's build-provenance bundle (the
/// `LLVM-<version>-Linux-<arch>.tar.xz.jsonl` release asset): it must be the
/// SLSA provenance v1 predicate and name this exact artifact digest. CI
/// additionally verifies the bundle signature with
/// `gh attestation verify --repo llvm/llvm-project`.
fn check_llvm_attestation_bundle(text: &str, digest: &str) -> Result<(), String> {
    for line in text.lines() {
        let Ok(bundle) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = bundle
            .get("dsseEnvelope")
            .and_then(|envelope| envelope.get("payload"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Ok(bytes) = B64.decode(payload) else {
            continue;
        };
        let Ok(statement) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if statement.get("predicateType").and_then(Value::as_str)
            != Some("https://slsa.dev/provenance/v1")
        {
            continue;
        }
        let subjects = statement
            .get("subject")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if subjects.iter().any(|subject| {
            subject
                .get("digest")
                .and_then(|digests| digests.get("sha256"))
                .and_then(Value::as_str)
                == Some(digest)
        }) {
            return Ok(());
        }
    }
    Err(format!(
        "LLVM's build-provenance bundle does not name the artifact digest {digest}"
    ))
}

// -------------------------------------------------------------- flutter

/// Google's Flutter Linux SDK archives for the stable channel, with the exact
/// SHA-256 published in `releases_linux.json`. Only x86_64 host archives are
/// published; linux-aarch64 is skipped with an explicit reason.
fn discover_flutter(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.flutter.channels must be exactly [{id: \"stable\"}]"
                .into(),
        );
    }
    let document = http_json(FLUTTER_RELEASES_LINUX, None)?;
    flutter_plan_from_releases(cfg, package, &document, held)
}

fn flutter_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    document: &Value,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let releases = document
        .get("releases")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{FLUTTER_RELEASES_LINUX} has no releases array"))?;
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for release in releases {
        if release.get("channel").and_then(Value::as_str) != Some("stable") {
            continue;
        }
        match flutter_plan_from_release(cfg, package, release) {
            Ok(Some(item)) => {
                if !seen.insert(item.version.clone()) {
                    continue;
                }
                if !held.contains(&format!("flutter@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "flutter: skipping release {:?}: {error}",
                release
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn flutter_plan_from_release(
    cfg: &Config,
    package: &PackageConfig,
    release: &Value,
) -> Result<Option<PlanItem>, String> {
    let version = required_str(release, "version")?;
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let Some(archive) = release
        .get("archive")
        .and_then(Value::as_str)
        .filter(|archive| !archive.is_empty())
    else {
        return Ok(None);
    };
    let Some(sha256) = release
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|digest| is_sha256_hex(digest))
    else {
        return Err(format!("Flutter release {version} has no SHA-256"));
    };
    let release_arch = release
        .get("dart_sdk_arch")
        .and_then(Value::as_str)
        .unwrap_or("x64");
    let url = format!("{FLUTTER_ARCHIVE_BASE}{archive}");
    let file = archive.rsplit('/').next().unwrap_or(archive).to_string();
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.flutter_arch(target)?;
        if arch != release_arch {
            log(format!(
                "flutter {version}: no official {arch} Linux archive in {FLUTTER_RELEASES_LINUX}; not published for {target}"
            ));
            continue;
        }
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(url.clone()),
                sha256: Some(sha256.to_string()),
                file: Some(file.clone()),
                source: Some(FLUTTER_RELEASES_LINUX.to_string()),
                format: Some("tar.xz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    let _ = package;
    Ok(Some(PlanItem {
        package: "flutter".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release
            .get("release_date")
            .and_then(Value::as_str)
            .map(|stamp| truncate_chars(stamp, 10)),
        source: release
            .get("dart_sdk_version")
            .and_then(Value::as_str)
            .map(|dart| json!({ "dart": dart })),
        targets,
        ..PlanItem::default()
    }))
}

/// `Dart SDK version: 3.14.0 (stable) (...) on "linux_x64"` (stderr) from
/// `dart --version`.
fn dart_sdk_version(output: &str) -> Option<String> {
    let rest = output.split("Dart SDK version: ").nth(1)?;
    rest.split_whitespace()
        .next()
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

// -------------------------------------------------------------- composer

/// Composer releases: one `composer.phar` per release (architecture
/// independent). The GitHub asset digest is cross-checked against
/// getcomposer.org's published SHA-256 and the phar is installed with a
/// generated `bin/composer` wrapper.
fn discover_composer(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.composer.channels must be exactly [{id: \"stable\"}]"
                .into(),
        );
    }
    let releases = github_stable_releases(COMPOSER_REPO, token)?;
    composer_plan_from_releases(cfg, package, &releases, held)
}

fn composer_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        match composer_plan_from_release(cfg, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("composer@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "composer: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn composer_plan_from_release(cfg: &Config, release: &Value) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release_assets(release)?;
    let name = "composer.phar";
    let Some(asset) = release_asset(assets, name) else {
        return Err(format!("Composer release {tag} publishes no {name}"));
    };
    let digest = asset_sha256(asset, name)?;
    let size = asset_size(asset, name)?;
    let url = asset_url(asset)?;
    // getcomposer.org publishes the same SHA-256 for every release.
    let sums_url = format!("https://getcomposer.org/download/{version}/composer.phar.sha256sum");
    let page = required_str(release, "html_url")?.to_string();
    let mut targets = BTreeMap::new();
    for (target, _target_config) in &cfg.targets.entries {
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(url.clone()),
                sha256: Some(digest.clone()),
                size: Some(size),
                file: Some(name.to_string()),
                digest: Some(digest.clone()),
                source: Some(page.clone()),
                sums_url: Some(sums_url.clone()),
                format: Some("phar".into()),
                strip_components: Some(0),
                ..TargetInfo::default()
            },
        );
    }
    Ok(Some(PlanItem {
        package: "composer".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release_published_date(release),
        targets,
        ..PlanItem::default()
    }))
}

// ------------------------------------------------------------------ zig

/// Zig's official download index: one `.tar.xz` per target, with the SHA-256
/// and size published in `index.json`.
fn discover_zig(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.zig.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let document = http_json(ZIG_INDEX, None)?;
    zig_plan_from_index(cfg, package, &document, held)
}

fn zig_plan_from_index(
    cfg: &Config,
    package: &PackageConfig,
    document: &Value,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let versions = document
        .as_object()
        .ok_or_else(|| format!("{ZIG_INDEX} is not a JSON object"))?;
    let mut found = Vec::new();
    for (version, entry) in versions {
        if version == "master" {
            continue;
        }
        if held.contains(&format!("zig@{version}")) {
            continue;
        }
        let Some(parsed) = Version::parse(version) else {
            continue;
        };
        if parsed.is_prerelease() {
            continue;
        }
        match zig_plan_from_version(cfg, version, entry) {
            Ok(Some(item)) => found.push(item),
            Ok(None) => {}
            Err(error) => log(format!("zig: skipping version {version}: {error}")),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn zig_plan_from_version(
    cfg: &Config,
    version: &str,
    entry: &Value,
) -> Result<Option<PlanItem>, String> {
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.zig_arch(target)?;
        let Some(binary) = entry.get(format!("{arch}-linux")) else {
            log(format!(
                "zig {version}: index.json has no {arch}-linux build; not published for {target}"
            ));
            continue;
        };
        let tarball = required_str(binary, "tarball")?;
        let shasum = required_str(binary, "shasum")?;
        if !is_sha256_hex(shasum) {
            return Err(format!(
                "zig {version} {arch}-linux shasum is not a SHA-256 hex digest"
            ));
        }
        let size = binary
            .get("size")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .ok_or_else(|| format!("zig {version} {arch}-linux has no size"))?;
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(tarball.to_string()),
                sha256: Some(shasum.to_string()),
                size: Some(size),
                file: tarball.rsplit('/').next().map(str::to_string),
                source: Some(ZIG_INDEX.to_string()),
                format: Some("tar.xz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "zig".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: entry
            .get("date")
            .and_then(Value::as_str)
            .map(|date| truncate_chars(date, 10)),
        targets,
        ..PlanItem::default()
    }))
}

// --------------------------------------------------------------- kotlin

/// The Kotlin command-line compiler: `kotlin-compiler-<version>.zip` per
/// release (architecture independent; it runs on a JVM), verified against the
/// GitHub asset digest and the published `.zip.sha256`.
fn discover_kotlin(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.kotlin.channels must be exactly [{id: \"stable\"}]"
                .into(),
        );
    }
    let releases = github_stable_releases(KOTLIN_REPO, token)?;
    kotlin_plan_from_releases(cfg, package, &releases, held)
}

fn kotlin_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        match kotlin_plan_from_release(cfg, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("kotlin@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "kotlin: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn kotlin_plan_from_release(cfg: &Config, release: &Value) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release_assets(release)?;
    let name = format!("kotlin-compiler-{version}.zip");
    let Some(asset) = release_asset(assets, &name) else {
        return Err(format!("Kotlin release {tag} publishes no {name}"));
    };
    let sums_name = format!("{name}.sha256");
    let Some(sums_asset) = release_asset(assets, &sums_name) else {
        return Err(format!("Kotlin release {tag} publishes no {sums_name}"));
    };
    let digest = asset_sha256(asset, &name)?;
    let size = asset_size(asset, &name)?;
    let url = asset_url(asset)?;
    let page = required_str(release, "html_url")?.to_string();
    let mut targets = BTreeMap::new();
    for (target, _target_config) in &cfg.targets.entries {
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(url.clone()),
                sha256: Some(digest.clone()),
                size: Some(size),
                file: Some(name.clone()),
                digest: Some(digest.clone()),
                source: Some(page.clone()),
                sums_url: Some(asset_url(sums_asset)?),
                format: Some("zip".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    Ok(Some(PlanItem {
        package: "kotlin".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release_published_date(release),
        targets,
        ..PlanItem::default()
    }))
}

// ---------------------------------------------------------------- cmake

/// CMake's official Linux archives, verified against the GitHub asset digest
/// and the release's `cmake-<version>-SHA-256.txt`.
fn discover_cmake(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
    token: Option<&str>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.cmake.channels must be exactly [{id: \"stable\"}]".into(),
        );
    }
    let releases = github_stable_releases(CMAKE_REPO, token)?;
    cmake_plan_from_releases(cfg, package, &releases, held)
}

fn cmake_plan_from_releases(
    cfg: &Config,
    package: &PackageConfig,
    releases: &[Value],
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut found = Vec::new();
    for release in releases {
        match cmake_plan_from_release(cfg, release) {
            Ok(Some(item)) => {
                if !held.contains(&format!("cmake@{}", item.version)) {
                    found.push(item);
                }
            }
            Ok(None) => {}
            Err(error) => log(format!(
                "cmake: skipping release {}: {error}",
                release
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )),
        }
    }
    sort_items_desc(&mut found);
    found.truncate(package.keep_patches as usize);
    Ok(found)
}

fn cmake_plan_from_release(cfg: &Config, release: &Value) -> Result<Option<PlanItem>, String> {
    if truthy(release.get("prerelease")) || truthy(release.get("draft")) {
        return Ok(None);
    }
    let tag = required_str(release, "tag_name")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let Some(parsed) = Version::parse(version) else {
        return Ok(None);
    };
    if parsed.is_prerelease() {
        return Ok(None);
    }
    let assets = release_assets(release)?;
    let sums_name = format!("cmake-{version}-SHA-256.txt");
    let Some(sums_asset) = release_asset(assets, &sums_name) else {
        return Err(format!("CMake release {tag} publishes no {sums_name}"));
    };
    let sums_url = asset_url(sums_asset)?;
    let page = required_str(release, "html_url")?.to_string();
    let mut targets = BTreeMap::new();
    for (target, target_config) in &cfg.targets.entries {
        let arch = target_config.cmake_arch(target)?;
        let name = format!("cmake-{version}-linux-{arch}.tar.gz");
        let Some(asset) = release_asset(assets, &name) else {
            log(format!(
                "cmake {version}: release {tag} has no {name}; not published for {target}"
            ));
            continue;
        };
        let digest = asset_sha256(asset, &name)?;
        targets.insert(
            target.clone(),
            TargetInfo {
                url: Some(asset_url(asset)?),
                sha256: Some(digest.clone()),
                size: Some(asset_size(asset, &name)?),
                file: Some(name),
                digest: Some(digest),
                source: Some(page.clone()),
                sums_url: Some(sums_url.clone()),
                format: Some("tar.gz".into()),
                strip_components: Some(1),
                ..TargetInfo::default()
            },
        );
    }
    if targets.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanItem {
        package: "cmake".into(),
        version: version.to_string(),
        channel: "stable".into(),
        kind: "upstream".into(),
        revision: 1,
        release_date: release_published_date(release),
        targets,
        ..PlanItem::default()
    }))
}

// --------------------------------------------------------------- gradle

/// Gradle distributions: architecture independent zips from
/// services.gradle.org, with the SHA-256 published in its version metadata
/// (and at `<zip>.sha256`).
fn discover_gradle(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    if channel_ids(package) != ["stable".to_string()] {
        return Err(
            "catalog.config.json packages.gradle.channels must be exactly [{id: \"stable\"}]"
                .into(),
        );
    }
    let document = http_json(GRADLE_VERSIONS, None)?;
    gradle_plan_from_versions(cfg, package, &document, held)
}

fn gradle_plan_from_versions(
    cfg: &Config,
    package: &PackageConfig,
    document: &Value,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let releases = document
        .as_array()
        .ok_or_else(|| format!("{GRADLE_VERSIONS} did not return a JSON array"))?;
    // Collect the final (non-snapshot/RC/broken) versions first; the size
    // probe below only runs for the versions that are kept.
    let mut candidates: Vec<(String, String, String, String, Option<String>)> = Vec::new();
    for release in releases {
        if truthy(release.get("snapshot"))
            || truthy(release.get("nightly"))
            || truthy(release.get("releaseNightly"))
            || truthy(release.get("broken"))
        {
            continue;
        }
        if !truthy(release.get("final")) {
            continue;
        }
        let version = required_str(release, "version")?;
        if Version::parse(version).is_none() || held.contains(&format!("gradle@{version}")) {
            continue;
        }
        let (Some(url), Some(sums_url), Some(checksum)) = (
            release.get("downloadUrl").and_then(Value::as_str),
            release.get("checksumUrl").and_then(Value::as_str),
            release.get("checksum").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !is_sha256_hex(checksum) {
            continue;
        }
        let release_date = release
            .get("buildTime")
            .and_then(Value::as_str)
            .and_then(gradle_release_date);
        candidates.push((
            version.to_string(),
            url.to_string(),
            sums_url.to_string(),
            checksum.to_string(),
            release_date,
        ));
    }
    candidates.sort_by(|a, b| version::compare(&b.0, &a.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    candidates.truncate(package.keep_patches as usize);
    let mut found = Vec::new();
    for (version, url, sums_url, checksum, release_date) in candidates {
        let file = url
            .rsplit('/')
            .next()
            .unwrap_or("gradle-bin.zip")
            .to_string();
        let mut targets = BTreeMap::new();
        for (target, _target_config) in &cfg.targets.entries {
            targets.insert(
                target.clone(),
                TargetInfo {
                    url: Some(url.clone()),
                    sha256: Some(checksum.clone()),
                    file: Some(file.clone()),
                    source: Some(GRADLE_VERSIONS.to_string()),
                    sums_url: Some(sums_url.clone()),
                    format: Some("zip".into()),
                    strip_components: Some(1),
                    ..TargetInfo::default()
                },
            );
        }
        found.push(PlanItem {
            package: "gradle".into(),
            version,
            channel: "stable".into(),
            kind: "upstream".into(),
            revision: 1,
            release_date,
            targets,
            ..PlanItem::default()
        });
    }
    Ok(found)
}

/// `20261110162215+0000` -> `2026-11-10`.
fn gradle_release_date(build_time: &str) -> Option<String> {
    let date = build_time.get(..8)?;
    if !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8]))
}

// ---------------------------------------------------------------- maven

/// Apache Maven binary archives from the official Apache archive directory.
/// The published `.sha512` is the upstream digest (the manifest pins the
/// computed SHA-256 of the downloaded bytes).
fn discover_maven(
    cfg: &Config,
    package: &PackageConfig,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    // Maven's update stream is <major>.<minor> (3.9, 3.10, …), so the
    // configured channels are informational and need not list every line.
    if channel_ids(package).is_empty() {
        return Err(
            "catalog.config.json packages.maven.channels must list at least one channel".into(),
        );
    }
    let listing = http_text(MAVEN_ARCHIVE)?;
    maven_plan_from_listing(cfg, package, &listing, held)
}

/// Maven version directories (`3.9.16/`) in Apache's archive listing.
fn maven_versions_from_listing(html: &str) -> Vec<String> {
    let mut versions = BTreeSet::new();
    for fragment in html.split("href=\"").skip(1) {
        let Some(end) = fragment.find('"') else {
            continue;
        };
        let href = &fragment[..end];
        let Some(version) = href.strip_suffix('/') else {
            continue;
        };
        // Only exact directory names like `3.9.16/` (no nested paths, no
        // parent-directory links).
        if version.is_empty() || version.contains('/') || version.starts_with('.') {
            continue;
        }
        if Version::parse(version).is_some() {
            versions.insert(version.to_string());
        }
    }
    versions.into_iter().collect()
}

fn maven_plan_from_listing(
    cfg: &Config,
    package: &PackageConfig,
    html: &str,
    held: &BTreeSet<String>,
) -> Result<Vec<PlanItem>, String> {
    let mut versions = maven_versions_from_listing(html);
    versions.sort_by(|a, b| version::compare(b, a));
    versions.truncate(package.keep_patches as usize);
    let mut found = Vec::new();
    for version in versions {
        if held.contains(&format!("maven@{version}")) {
            continue;
        }
        let name = format!("apache-maven-{version}-bin.tar.gz");
        let base = format!("{MAVEN_ARCHIVE}{version}/binaries/");
        let url = format!("{base}{name}");
        let mut targets = BTreeMap::new();
        for (target, _target_config) in &cfg.targets.entries {
            targets.insert(
                target.clone(),
                TargetInfo {
                    url: Some(url.clone()),
                    file: Some(name.clone()),
                    source: Some(MAVEN_ARCHIVE.to_string()),
                    sums_url: Some(format!("{base}{name}.sha512")),
                    format: Some("tar.gz".into()),
                    strip_components: Some(1),
                    ..TargetInfo::default()
                },
            );
        }
        let channel = default_channel(&version);
        found.push(PlanItem {
            package: "maven".into(),
            version,
            channel,
            kind: "upstream".into(),
            revision: 1,
            targets,
            ..PlanItem::default()
        });
    }
    Ok(found)
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
    for (id, package) in &cfg.packages.entries {
        let found = match id.as_str() {
            "python" => discover_python(&cfg, package, &held, token.as_deref())?,
            "php" => discover_php(&cfg, package, &held)?,
            "go" => discover_go(&cfg, package, &held)?,
            "nodejs" => discover_node(&cfg, package, &held)?,
            "rust" => discover_rust(&cfg, package, &held)?,
            "java" => discover_java(&cfg, package, &held, token.as_deref())?,
            "bun" => discover_bun(&cfg, package, &held, token.as_deref())?,
            "deno" => discover_deno(&cfg, package, &held, token.as_deref())?,
            "dotnet" => discover_dotnet(&cfg, package, &held)?,
            "clang" => discover_clang(&cfg, package, &held, token.as_deref())?,
            "flutter" => discover_flutter(&cfg, package, &held)?,
            "composer" => discover_composer(&cfg, package, &held, token.as_deref())?,
            "zig" => discover_zig(&cfg, package, &held)?,
            "kotlin" => discover_kotlin(&cfg, package, &held, token.as_deref())?,
            "cmake" => discover_cmake(&cfg, package, &held, token.as_deref())?,
            "gradle" => discover_gradle(&cfg, package, &held)?,
            "maven" => discover_maven(&cfg, package, &held)?,
            "ai-agent" => discover_ai_agent(&cfg, package, &held, token.as_deref())?,
            other => {
                return Err(format!(
                    "catalog.config.json packages.{other} has no catalog provider"
                ))
            }
        };
        items.extend(found);
    }

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
    hints: &[(String, String)],
) -> Result<String, String> {
    // RAST runs under MemoryDenyWriteExecute; check the same way.
    let environment = check_environment(root, home, hints);
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
        match parts.as_slice() {
            // `<digest>  <file>` (SHASUMS256.txt style).
            [digest, file] => {
                sums.insert((*file).to_string(), digest.to_ascii_lowercase());
            }
            // A bare digest (`composer.phar.sha256sum`, Apache `.sha512`,
            // Kotlin `.zip.sha256`, Gradle `.sha256`) is stored under "".
            [digest]
                if digest.len() == 64
                    && digest
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) =>
            {
                sums.insert(String::new(), (*digest).to_string());
            }
            [digest] if is_sha512_hex(digest) => {
                sums.insert(String::new(), (*digest).to_string());
            }
            _ => {}
        }
    }
    cache.insert(url.to_string(), sums.clone());
    Ok(sums)
}

/// One published digest from a parsed checksum document: `file` when named,
/// otherwise the bare digest stored under `""`.
fn published_digest(
    sums: &BTreeMap<String, String>,
    file: Option<&str>,
    length: usize,
) -> Result<String, String> {
    let label = file.unwrap_or("the artifact");
    let Some(value) = file
        .and_then(|file| sums.get(file))
        .or_else(|| sums.get(""))
    else {
        return Err(format!("no published checksum for {label}"));
    };
    let valid = value.len() == length
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if !valid {
        return Err(format!(
            "the published checksum for {label} is not a {length}-digit hex digest"
        ));
    }
    Ok(value.clone())
}

fn optional_executables(package: &str) -> &'static [(&'static str, &'static str)] {
    match package {
        "nodejs" => &[("corepack", "bin/corepack")],
        _ => &[],
    }
}

/// One archive as verified for one target (results file format). The first
/// entry of [`VerifiedItem::archives`] is the artifact's primary archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    sha256: String,
    size: u64,
    format: String,
    strip_components: u32,
}

struct VerifiedItem {
    /// Primary archive first; Rust adds its other components.
    archives: Vec<ArchiveResult>,
    bundled: BTreeMap<String, String>,
    optional: BTreeSet<String>,
    checks: Vec<String>,
    verification: String,
}

/// One archive of a target as planned (primary first).
struct PlannedArchive<'a> {
    name: Option<&'a str>,
    url: &'a str,
    sha256: Option<&'a str>,
    sha512: Option<&'a str>,
    size: Option<u64>,
    format: &'a str,
    strip_components: u32,
}

fn planned_archives<'a>(
    item: &PlanItem,
    target: &str,
    info: &'a TargetInfo,
) -> Result<Vec<PlannedArchive<'a>>, String> {
    let url = info.url.as_deref().ok_or_else(|| {
        format!(
            "{} {} {target} has no artifact URL in the plan",
            item.package, item.version
        )
    })?;
    let format = info.format.as_deref().unwrap_or("tar.gz");
    let strip_components = info.strip_components.unwrap_or(1);
    let mut archives = vec![PlannedArchive {
        name: None,
        url,
        sha256: info.sha256.as_deref(),
        sha512: info.sha512.as_deref(),
        size: info.size,
        format,
        strip_components,
    }];
    for archive in &info.archives {
        archives.push(PlannedArchive {
            name: archive.name.as_deref(),
            url: &archive.url,
            sha256: archive.sha256.as_deref(),
            sha512: None,
            size: archive.size,
            format,
            strip_components,
        });
    }
    Ok(archives)
}

fn sha512_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha512};
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn http_text(url: &str) -> Result<String, String> {
    let bytes = http_get(url, None, None)?;
    String::from_utf8(bytes).map_err(|_| format!("{url} is not UTF-8"))
}

/// Environment for the version checks: no inherited state, a writable home,
/// and the JIT-free Node.js settings RAST itself runs with.
fn check_environment(
    root: &Path,
    home: &Path,
    hints: &[(String, String)],
) -> Vec<(String, String)> {
    let mut environment = vec![
        (
            "PATH".to_string(),
            format!("{}/bin:/usr/local/bin:/usr/bin:/bin", root.display()),
        ),
        ("HOME".to_string(), home.display().to_string()),
        ("TMPDIR".to_string(), home.display().to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("NODE_OPTIONS".to_string(), "--jitless".to_string()),
        (
            "npm_config_cache".to_string(),
            home.join(".npm").display().to_string(),
        ),
        (
            "npm_config_update_notifier".to_string(),
            "false".to_string(),
        ),
        // .NET's first-run experience must not write outside the temp home.
        ("DOTNET_CLI_TELEMETRY_OPTOUT".to_string(), "1".to_string()),
        ("DOTNET_NOLOGO".to_string(), "1".to_string()),
        // Flutter/Gradle/Maven and other CI-aware tools: non-interactive,
        // no analytics, caches under the writable temp home.
        ("CI".to_string(), "true".to_string()),
        ("FLUTTER_SUPPRESS_ANALYTICS".to_string(), "true".to_string()),
    ];
    // Runtime env hints (e.g. JAVA_HOME, DOTNET_ROOT, LD_LIBRARY_PATH) apply
    // to the checks too, exactly as in the installer; `{root}` is the
    // extracted prefix. Fixed entries win, so a hint can never replace
    // PATH/HOME/TMPDIR.
    for (name, value) in hints {
        if !environment.iter().any(|(existing, _)| existing == name) {
            environment.push((
                name.clone(),
                value.replace("{root}", &root.display().to_string()),
            ));
        }
    }
    environment
}

/// Decode an ASCII-armored OpenPGP block (`-----BEGIN PGP PUBLIC KEY BLOCK-----`)
/// into the binary keyring `gpgv --keyring` expects.
fn pgp_dearmor(text: &str, label: &str) -> Result<Vec<u8>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text
        .find(&begin)
        .ok_or_else(|| format!("no {label} block"))?
        + begin.len();
    let rest = &text[start..];
    let stop = rest.find(&end).ok_or_else(|| format!("no {label} end"))?;
    let body = &rest[..stop];
    // Armor headers ("Comment: …") are terminated by an empty line.
    let body = match body.find("\n\n") {
        Some(index) => &body[index + 2..],
        None => body,
    };
    let encoded: String = body
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('='))
        .collect();
    B64.decode(encoded)
        .map_err(|_| format!("{label} is not valid base64"))
}

/// Check the OpenPGP signature of the Rust stable channel manifest against
/// the Rust release signing key, whose fingerprint is pinned above. This ties
/// every component SHA-256 to a signature made with the Rust project's key
/// (the same trust model as rustup when it verifies a channel manifest).
fn verify_rust_manifest_signature(work: &Path) -> Result<(), String> {
    let (doc, manifest_sha256, text) = rust_channel_manifest()?;
    let _ = doc;
    let manifest_path = work.join("channel-rust-stable.toml");
    fs::write(&manifest_path, &text)
        .map_err(|error| format!("cannot write {}: {error}", manifest_path.display()))?;
    let sums_path = work.join("channel-rust-stable.toml.sha256");
    fs::write(
        &sums_path,
        http_get(&format!("{RUST_CHANNEL_TOML}.sha256"), None, None)?,
    )
    .map_err(|error| format!("cannot write {}: {error}", sums_path.display()))?;
    let signature_path = work.join("channel-rust-stable.toml.asc");
    fs::write(
        &signature_path,
        http_get(&format!("{RUST_CHANNEL_TOML}.asc"), None, None)?,
    )
    .map_err(|error| format!("cannot write {}: {error}", signature_path.display()))?;
    let key_path = work.join("rust-release-key.gpg");
    if !key_path.exists() {
        let armored = http_text(RUST_RELEASE_KEY)?;
        fs::write(&key_path, pgp_dearmor(&armored, "PGP PUBLIC KEY BLOCK")?)
            .map_err(|error| format!("cannot write {}: {error}", key_path.display()))?;
        // Pin the key: a different primary key must not be trusted silently.
        let fingerprint = run_capture(
            Path::new("gpg"),
            &[
                "--show-keys".to_string(),
                "--with-colons".to_string(),
                key_path.display().to_string(),
            ],
            None,
            None,
            Duration::from_secs(60),
        )?;
        let expected = format!("fpr:::::::::{RUST_RELEASE_KEY_FINGERPRINT}:");
        if fingerprint.exit_code != 0 || !fingerprint.stdout.lines().any(|line| line == expected) {
            return Err(format!(
                "the Rust release key at {RUST_RELEASE_KEY} does not have the pinned fingerprint {RUST_RELEASE_KEY_FINGERPRINT}"
            ));
        }
    }
    let captured = run_capture(
        Path::new("gpgv"),
        &[
            "--keyring".to_string(),
            key_path.display().to_string(),
            signature_path.display().to_string(),
            manifest_path.display().to_string(),
        ],
        None,
        None,
        Duration::from_secs(60),
    )?;
    if captured.exit_code != 0 {
        return Err(format!(
            "the Rust channel manifest signature does not verify: {}",
            truncate_chars(captured.stderr.trim(), 300)
        ));
    }
    let _ = manifest_sha256;
    Ok(())
}

/// Version from a rustc archive name: `rustc-1.100.0-x86_64-unknown-linux-gnu.tar.gz`.
fn rust_triple_from_file(file: &str, version: &str) -> Result<String, String> {
    file.strip_prefix(&format!("rustc-{version}-"))
        .and_then(|rest| rest.strip_suffix(".tar.gz"))
        .filter(|triple| !triple.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Rust artifact {file:?} does not name the {version} rustc archive"))
}

/// SHA-256 of every component archive for `triple`, from the signed channel
/// manifest.
fn rust_component_hashes(
    doc: &toml::Value,
    triple: &str,
) -> Result<BTreeMap<String, String>, String> {
    let pkgs = toml_table(doc, "pkg").ok_or("Rust channel manifest has no [pkg] tables")?;
    let mut hashes = BTreeMap::new();
    for (component, _) in RUST_COMPONENTS {
        let hash = pkgs
            .get(*component)
            .and_then(|pkg| pkg.get("target"))
            .and_then(toml::Value::as_table)
            .and_then(|targets| targets.get(triple))
            .and_then(toml::Value::as_table)
            .and_then(|entry| entry.get("hash"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                format!("the Rust channel manifest has no {component} hash for {triple}")
            })?;
        hashes.insert((*component).to_string(), hash.to_string());
    }
    Ok(hashes)
}

/// `gh attestation verify` argument list.
///
/// `gh` enforces the SLSA provenance v1 predicate by default; repositories
/// that publish a different predicate (Deno uses the in-toto release
/// predicate) must name it explicitly.
/// `gh attestation verify <file> --repo <repo>` (GitHub build provenance).
fn verify_attestation(archive: &Path, repo: &str) -> Result<(), String> {
    let captured = run_capture(
        Path::new("gh"),
        &[
            "attestation".to_string(),
            "verify".to_string(),
            archive.display().to_string(),
            "--repo".to_string(),
            repo.to_string(),
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
    Ok(())
}

/// URL-encode the characters used by a predicate-type URL.
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Confirm a Deno release attestation for one exact artifact digest.
///
/// Deno attests every release asset with the in-toto release predicate
/// (`https://in-toto.io/attestation/release/v0.2`). `gh attestation verify`
/// does not accept this release predicate (it reports "no attestations found"
/// with gh 2.104), while the GitHub attestations API returns the signed bundle
/// for the same digest. We therefore inspect that API response for *this
/// digest*: it must carry the expected predicate type and name the digest in
/// its subjects. Integrity is still anchored by the upstream .sha256sum and
/// the GitHub asset digest.
fn verify_deno_release_attestation(digest: &str, token: Option<&str>) -> Result<(), String> {
    let url = format!(
        "https://api.github.com/repos/{DENO_REPO}/attestations/sha256:{digest}?per_page=30&predicate_type={}",
        encode_query_value(DENO_RELEASE_PREDICATE)
    );
    let document = http_json(&url, token)?;
    check_deno_attestation_document(&document, digest)
}

/// Validate the GitHub attestations API response for one Deno artifact.
fn check_deno_attestation_document(document: &Value, digest: &str) -> Result<(), String> {
    let attestations = document
        .get("attestations")
        .and_then(Value::as_array)
        .ok_or_else(|| "the Deno attestations response has no attestations array".to_string())?;
    for attestation in attestations {
        let Some(payload) = attestation
            .get("bundle")
            .and_then(|bundle| bundle.get("dsseEnvelope"))
            .and_then(|envelope| envelope.get("payload"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Ok(bytes) = B64.decode(payload) else {
            continue;
        };
        let Ok(statement) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if statement.get("predicateType").and_then(Value::as_str) != Some(DENO_RELEASE_PREDICATE) {
            continue;
        }
        let subjects = statement
            .get("subject")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if subjects.iter().any(|subject| {
            subject
                .get("digest")
                .and_then(|digests| digests.get("sha256"))
                .and_then(Value::as_str)
                == Some(digest)
        }) {
            return Ok(());
        }
    }
    Err(format!(
        "no Deno release attestation names this artifact digest ({digest})"
    ))
}

fn verify_item(
    item: &PlanItem,
    target: &str,
    info: &TargetInfo,
    work: &Path,
    sums_cache: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> Result<VerifiedItem, String> {
    let planned = planned_archives(item, target, info)?;
    let mut expected_sha256: Vec<Option<String>> = planned
        .iter()
        .map(|part| part.sha256.map(str::to_string))
        .collect();
    let mut expected_sha512: Vec<Option<String>> = planned
        .iter()
        .map(|part| part.sha512.map(str::to_string))
        .collect();
    let mut expected_sizes: Vec<Option<u64>> = planned.iter().map(|part| part.size).collect();
    #[allow(clippy::needless_late_init)]
    let verification: String;
    match item.package.as_str() {
        "go" => {
            verification =
                "SHA-256 published in the go.dev/dl release index; downloaded and re-hashed in CI"
                    .into();
        }
        "nodejs" => {
            let sums = node_shasums(&item.version, work)?;
            let file = info.file.as_deref().ok_or_else(|| {
                format!("nodejs {} has no artifact file in the plan", item.version)
            })?;
            let expected = sums
                .get(file)
                .cloned()
                .ok_or_else(|| format!("{file} is not listed in SHASUMS256.txt"))?;
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from SHASUMS256.txt, whose signature was verified with gpgv against the nodejs/release-keys keyring".into();
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
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the release SHA256SUMS, equal to the GitHub asset digest; GitHub build-provenance attestation verified with gh attestation verify".into();
        }
        "rust" => {
            verify_rust_manifest_signature(work)?;
            let triple = rust_triple_from_file(
                info.file.as_deref().ok_or_else(|| {
                    format!("rust {} has no rustc file in the plan", item.version)
                })?,
                &item.version,
            )?;
            let (doc, _manifest_sha256, _text) = rust_channel_manifest()?;
            let hashes = rust_component_hashes(&doc, &triple)?;
            for (index, part) in planned.iter().enumerate() {
                let component = part.name.unwrap_or("rustc");
                let Some(expected) = hashes.get(component) else {
                    return Err(format!("the channel manifest has no {component} hash"));
                };
                if part.sha256 != Some(expected.as_str()) {
                    return Err(format!(
                        "rust {component} SHA-256 in the plan does not match the signed channel manifest"
                    ));
                }
                expected_sha256[index] = Some(expected.clone());
            }
            verification = "Every component SHA-256 comes from the stable channel manifest, whose SHA-256 matches the dist .sha256 file and whose OpenPGP signature was verified with gpgv against the pinned Rust release key".into();
        }
        "java" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("java {} has no sha256 in the plan", item.version))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("Adoptium API checksum differs from the GitHub asset digest".into());
            }
            if let Some(url) = &info.sums_url {
                let sums = pbs_sums(url, sums_cache)?;
                let file = info.file.as_deref().ok_or_else(|| {
                    format!("java {} has no artifact file in the plan", item.version)
                })?;
                let published = sums
                    .get(file)
                    .ok_or_else(|| format!("{file} is not listed in the Adoptium checksum file"))?;
                if published != &expected {
                    return Err(
                        "the Adoptium .sha256.txt checksum differs from the API and GitHub digest"
                            .into(),
                    );
                }
            }
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the Adoptium API equals the GitHub release asset digest and the published .sha256.txt; downloaded and re-hashed in CI; java/javac/jar/jshell version checks run on the native runner".into();
        }
        "bun" | "deno" | "ai-agent" => {
            let url = info.sums_url.as_deref().ok_or_else(|| {
                format!(
                    "{} {} has no checksums URL in the plan",
                    item.package, item.version
                )
            })?;
            let sums = pbs_sums(url, sums_cache)?;
            let file = info.file.as_deref().ok_or_else(|| {
                format!(
                    "{} {} has no artifact file in the plan",
                    item.package, item.version
                )
            })?;
            let expected = sums
                .get(file)
                .cloned()
                .ok_or_else(|| format!("{file} is not listed in the published checksums"))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("GitHub asset digest differs from the published checksums".into());
            }
            expected_sha256[0] = Some(expected);
            verification = if item.package == "deno" {
                "SHA-256 from the release .sha256sum, equal to the GitHub asset digest; the GitHub release attestation (in-toto release predicate) is confirmed for this exact digest through the GitHub attestations API".into()
            } else if item.package == "ai-agent" {
                "Built by RAST's ai-agent workflow; SHA-256 from the release SHA256SUMS, equal to the GitHub asset digest; build-provenance attestation verified with gh attestation verify; window files and the server's --version checked on the native runner".into()
            } else {
                "SHA-256 from the release SHASUMS256.txt, equal to the GitHub asset digest; downloaded and re-hashed in CI".into()
            };
        }
        "dotnet" => {
            let sha512 = info
                .sha512
                .clone()
                .ok_or_else(|| format!("dotnet {} has no SHA-512 in the plan", item.version))?;
            if !is_sha512(&sha512) {
                return Err("dotnet SHA-512 in the plan is not a SHA-512 digest".into());
            }
            expected_sha512[0] = Some(sha512);
            // The manifest pin is computed from the downloaded bytes; the
            // release metadata's SHA-512 is what proves those bytes.
            expected_sha256[0] = None;
            expected_sizes[0] = None;
            verification = ".NET release metadata SHA-512 verified against the downloaded SDK archive; the manifest pins the computed SHA-256; dotnet --version, --list-sdks and --list-runtimes run on the native runner".into();
        }
        "clang" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("clang {} has no SHA-256 in the plan", item.version))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("the GitHub asset digest differs from the planned SHA-256".into());
            }
            let bundle = info.sums_url.as_deref().ok_or_else(|| {
                format!(
                    "clang {} has no provenance bundle in the plan",
                    item.version
                )
            })?;
            let text = http_text(bundle)?;
            check_llvm_attestation_bundle(&text, &expected)?;
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the GitHub asset digest, named by LLVM's SLSA provenance bundle and confirmed with gh attestation verify; bytes re-hashed; clang, clang++ and ld.lld run on the native runner (lldb is shipped but not executed: the official build needs a host libpython)".into();
        }
        "flutter" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("flutter {} has no SHA-256 in the plan", item.version))?;
            if !is_sha256_hex(&expected) {
                return Err("flutter SHA-256 in the plan is not a SHA-256 digest".into());
            }
            expected_sha256[0] = Some(expected);
            expected_sizes[0] = Some(http_content_length(planned[0].url)?);
            verification = "SHA-256 from Google's official releases_linux.json metadata; the size comes from the storage object; bytes re-hashed in CI; flutter --version and dart --version run on the native runner".into();
        }
        "composer" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("composer {} has no SHA-256 in the plan", item.version))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("the GitHub asset digest differs from the planned SHA-256".into());
            }
            let sums_url = info.sums_url.as_deref().ok_or_else(|| {
                format!("composer {} has no checksum URL in the plan", item.version)
            })?;
            let sums = pbs_sums(sums_url, sums_cache)?;
            let published = published_digest(&sums, info.file.as_deref(), 64)?;
            if published != expected {
                return Err(
                    "the getcomposer.org SHA-256 differs from the GitHub asset digest".into(),
                );
            }
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the GitHub release asset digest, equal to getcomposer.org's published composer.phar.sha256sum; bytes re-hashed in CI; the generated bin/composer wrapper runs composer --version with the PHP CLI on PATH".into();
        }
        "zig" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("zig {} has no SHA-256 in the plan", item.version))?;
            if !is_sha256_hex(&expected) {
                return Err("zig SHA-256 in the plan is not a SHA-256 digest".into());
            }
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 and size from ziglang.org's download index.json; bytes re-hashed in CI; zig version runs on the native runner".into();
        }
        "kotlin" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("kotlin {} has no SHA-256 in the plan", item.version))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("the GitHub asset digest differs from the planned SHA-256".into());
            }
            let sums_url = info.sums_url.as_deref().ok_or_else(|| {
                format!("kotlin {} has no checksum URL in the plan", item.version)
            })?;
            let sums = pbs_sums(sums_url, sums_cache)?;
            let published = published_digest(&sums, info.file.as_deref(), 64)?;
            if published != expected {
                return Err(
                    "the published .zip.sha256 differs from the GitHub asset digest".into(),
                );
            }
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the GitHub release asset digest, equal to the published kotlin-compiler .zip.sha256; bytes re-hashed in CI; kotlinc -version runs with the JDK on PATH".into();
        }
        "cmake" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("cmake {} has no SHA-256 in the plan", item.version))?;
            if info.digest.as_deref() != Some(expected.as_str()) {
                return Err("the GitHub asset digest differs from the planned SHA-256".into());
            }
            let sums_url = info
                .sums_url
                .as_deref()
                .ok_or_else(|| format!("cmake {} has no checksum URL in the plan", item.version))?;
            let sums = pbs_sums(sums_url, sums_cache)?;
            let published = published_digest(&sums, info.file.as_deref(), 64)?;
            if published != expected {
                return Err("the release SHA-256.txt differs from the GitHub asset digest".into());
            }
            expected_sha256[0] = Some(expected);
            verification = "SHA-256 from the GitHub release asset digest, equal to the release SHA-256.txt; bytes re-hashed in CI; cmake --version and ctest --version run on the native runner".into();
        }
        "gradle" => {
            let expected = info
                .sha256
                .clone()
                .ok_or_else(|| format!("gradle {} has no SHA-256 in the plan", item.version))?;
            let sums_url = info.sums_url.as_deref().ok_or_else(|| {
                format!("gradle {} has no checksum URL in the plan", item.version)
            })?;
            let sums = pbs_sums(sums_url, sums_cache)?;
            let published = published_digest(&sums, info.file.as_deref(), 64)?;
            if published != expected {
                return Err("the published .sha256 differs from the planned SHA-256".into());
            }
            expected_sha256[0] = Some(expected);
            expected_sizes[0] = Some(http_content_length(planned[0].url)?);
            verification = "SHA-256 from Gradle's version metadata, equal to the published .sha256; bytes re-hashed in CI; gradle --version runs with the JDK on PATH".into();
        }
        "maven" => {
            let sums_url = info
                .sums_url
                .as_deref()
                .ok_or_else(|| format!("maven {} has no checksum URL in the plan", item.version))?;
            let sums = pbs_sums(sums_url, sums_cache)?;
            let published = published_digest(&sums, info.file.as_deref(), 128)?;
            if !is_sha512_hex(&published) {
                return Err("Apache's .sha512 file is not a SHA-512 digest".into());
            }
            expected_sha512[0] = Some(published);
            expected_sizes[0] = Some(http_content_length(planned[0].url)?);
            // The manifest pins the SHA-256 computed from the verified bytes.
            expected_sha256[0] = None;
            verification = "SHA-512 from Apache's published .sha512 file; bytes re-hashed in CI; mvn --version runs with the JDK on PATH".into();
        }
        other => return Err(format!("not an upstream package: {other}")),
    }
    if verification.len() > 300 {
        return Err(format!(
            "verification note for {} is longer than the manifest allows",
            item.package
        ));
    }
    for (index, part) in planned.iter().enumerate() {
        if expected_sha256[index].is_none() && expected_sha512[index].is_none() {
            return Err(format!(
                "{} {}: no pinned SHA-256/SHA-512 for {}",
                item.package, item.version, part.url
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
    let mut archives = Vec::new();
    for (index, part) in planned.iter().enumerate() {
        let archive_path = work.join(format!("artifact-{index}.{}", part.format));
        let (sha256, size) = download(part.url, &archive_path)?;
        if let Some(expected) = &expected_sha256[index] {
            if &sha256 != expected {
                return Err(format!(
                    "SHA-256 mismatch for {}: expected {expected}, got {sha256}",
                    part.name.unwrap_or(target)
                ));
            }
        }
        if let Some(expected) = &expected_sha512[index] {
            let actual = sha512_of(&archive_path)?;
            if &actual != expected {
                return Err(format!(
                    "SHA-512 mismatch for {}: expected {expected}, got {actual}",
                    part.name.unwrap_or(target)
                ));
            }
        }
        if let Some(expected_size) = expected_sizes[index] {
            if expected_size != size {
                return Err(format!(
                    "size mismatch for {}: expected {expected_size}, got {size}",
                    part.name.unwrap_or(target)
                ));
            }
        }
        if index == 0 {
            // `gh attestation verify` defaults to the SLSA provenance v1
            // predicate; Deno attests with the in-toto release predicate.
            match item.package.as_str() {
                "python" => verify_attestation(&archive_path, PBS_REPO)?,
                "clang" if item.attestations => verify_attestation(&archive_path, CLANG_REPO)?,
                "ai-agent" if item.attestations => {
                    let repo =
                        info.source.as_deref().and_then(github_repo_of).ok_or(
                            "ai-agent: the plan does not say which repository released it",
                        )?;
                    verify_attestation(&archive_path, &repo)?
                }
                "deno" if item.attestations => {
                    verify_deno_release_attestation(&sha256, gh_token().as_deref())?
                }
                _ => {}
            }
        }
        archives.push(ArchiveResult {
            name: part.name.map(str::to_string),
            sha256,
            size,
            format: part.format.to_string(),
            strip_components: part.strip_components,
        });
        if part.format == "phar" {
            // A verified single file (Composer): write it read-only into the
            // prefix; the wrapper is generated once the runtime spec exists.
            let path = phar_path_for(&item.package)
                .ok_or_else(|| format!("{} is not a package with phar artifacts", item.package))?;
            install::install_phar(&root, &archive_path, path)?;
        } else {
            let room = available_space(work)
                .map(|free| free.saturating_sub(1024 * 1024 * 1024))
                .unwrap_or(archive::MAX_TOTAL_BYTES);
            archive::extract_archive(
                &archive_path,
                &root,
                part.format,
                part.strip_components,
                room,
                &mut progress,
                &|| false,
            )?;
            let _ = fs::remove_file(&archive_path);
        }
    }
    let home = work.join("home");
    fs::create_dir_all(&home)
        .map_err(|error| format!("cannot create {}: {error}", home.display()))?;
    if let Some(app) = item.source.as_ref().and_then(|source| source.get("app")) {
        let checks = check_app_files(&root, app, &item.version, &home)?;
        return Ok(VerifiedItem {
            archives,
            bundled: BTreeMap::new(),
            optional: BTreeSet::new(),
            checks,
            verification,
        });
    }
    let mut bundled = BTreeMap::new();
    match item.package.as_str() {
        "nodejs" => {
            let npm = run_check(&root, "bin/npm", &["--version".to_string()], "", &home, &[])?;
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
        "python" => {
            let pip = run_check(
                &root,
                "bin/pip3",
                &["--version".to_string()],
                "pip ",
                &home,
                &[],
            )?;
            let version = pip
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| format!("bin/pip3 --version printed {pip:?}"))?
                .to_string();
            bundled.insert("pip".to_string(), version);
        }
        "dotnet" => {
            let captured = run_capture(
                &root.join("dotnet"),
                &["--list-runtimes".to_string()],
                Some(&home),
                Some(&check_environment(&root, &home, &[])),
                Duration::from_secs(120),
            )?;
            let text = format!("{}{}", captured.stdout, captured.stderr);
            if captured.exit_code != 0 {
                return Err(format!(
                    "dotnet --list-runtimes exited {}: {}",
                    captured.exit_code,
                    truncate_chars(text.trim(), 300)
                ));
            }
            let runtime = text
                .lines()
                .find_map(|line| line.strip_prefix("Microsoft.NETCore.App "))
                .and_then(|rest| rest.split_whitespace().next())
                .ok_or_else(|| {
                    format!(
                        "dotnet --list-runtimes lists no Microsoft.NETCore.App: {}",
                        truncate_chars(text.trim(), 200)
                    )
                })?
                .to_string();
            if let Some(expected) = item
                .source
                .as_ref()
                .and_then(|source| source.get("runtime"))
                .and_then(Value::as_str)
                .filter(|runtime| !runtime.is_empty())
            {
                if expected != runtime {
                    return Err(format!(
                        ".NET release metadata says runtime {expected}, the SDK archive reports {runtime}"
                    ));
                }
            }
            bundled.insert("runtime".to_string(), runtime);
        }
        "flutter" => {
            let captured = run_capture(
                &root.join("bin/dart"),
                &["--version".to_string()],
                Some(&home),
                Some(&check_environment(&root, &home, &[])),
                Duration::from_secs(120),
            )?;
            let text = format!("{}{}", captured.stdout, captured.stderr);
            if captured.exit_code != 0 {
                return Err(format!(
                    "dart --version exited {}: {}",
                    captured.exit_code,
                    truncate_chars(text.trim(), 300)
                ));
            }
            let dart = dart_sdk_version(&text).ok_or_else(|| {
                format!(
                    "dart --version printed no Dart SDK version: {}",
                    truncate_chars(text.trim(), 200)
                )
            })?;
            if let Some(expected) = item
                .source
                .as_ref()
                .and_then(|source| source.get("dart"))
                .and_then(Value::as_str)
                .filter(|dart| !dart.is_empty())
            {
                if expected != dart {
                    return Err(format!(
                        "Flutter's release metadata says Dart {expected}, the archive ships {dart}"
                    ));
                }
            }
            bundled.insert("dart".to_string(), dart);
        }
        _ => {}
    }
    let mut optional = BTreeSet::new();
    for (name, relative) in optional_executables(&item.package) {
        if root.join(relative).exists() {
            optional.insert((*name).to_string());
        }
    }
    let spec = runtime_spec(&item.package, &item.version, &bundled, &optional)?;
    if let Some(phar) = phar_path_for(&item.package) {
        // Executables of a phar package are wrappers around the verified phar.
        install::write_phar_wrappers(&root, &spec.executables, phar)?;
    }
    if let Some(mirror) = &spec.mirror {
        // Checks run through the per-user mirror launchers, as on a server.
        install::write_mirror_launchers(&root, &spec.executables, mirror)?;
    }
    for executable in &spec.executables {
        if !is_executable_file(&root.join(&executable.path)) {
            return Err(format!("executable {} is missing", executable.path));
        }
    }
    // The manifest's runtime env hints apply to every check (JAVA_HOME,
    // DOTNET_ROOT, LD_LIBRARY_PATH, …); install.rs does the same.
    let hints: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut checks = Vec::new();
    for check in &spec.checks {
        let executable = spec
            .executables
            .iter()
            .find(|executable| executable.name == check.executable)
            .ok_or_else(|| format!("check uses unknown executable {:?}", check.executable))?;
        let launch = if spec.mirror.is_some() {
            format!("{}/{}", manifest::MIRROR_DIR, executable.name)
        } else {
            executable.path.clone()
        };
        checks.push(run_check(
            &root,
            &launch,
            &check.args,
            &check.expect,
            &home,
            &hints,
        )?);
    }
    Ok(VerifiedItem {
        archives,
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
                let primary = &verified.archives[0];
                result.insert("ok".into(), json!(true));
                result.insert("sha256".into(), json!(primary.sha256));
                result.insert("size".into(), json!(primary.size));
                result.insert("format".into(), json!(primary.format));
                result.insert("stripComponents".into(), json!(primary.strip_components));
                if verified.archives.len() > 1 {
                    result.insert(
                        "archives".into(),
                        serde_json::to_value(&verified.archives[1..])
                            .map_err(|error| error.to_string())?,
                    );
                }
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
        &[],
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
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut path_dirs = vec!["bin".to_string()];
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
        "rust" => {
            // A standalone rust-installer toolchain (not rustup): rustc,
            // cargo, rust-std, rustfmt and clippy share one prefix. Cargo
            // caches stay per user, so no CARGO_HOME/RUSTUP_HOME is set.
            for (name, path) in [
                ("rustc", "bin/rustc"),
                ("cargo", "bin/cargo"),
                ("rustfmt", "bin/rustfmt"),
                ("cargo-fmt", "bin/cargo-fmt"),
                ("clippy-driver", "bin/clippy-driver"),
                ("cargo-clippy", "bin/cargo-clippy"),
            ] {
                executables.push(manifest::Executable {
                    name: name.into(),
                    path: path.into(),
                });
            }
            checks.push(manifest::VersionCheck {
                executable: "rustc".into(),
                args: vec!["--version".into()],
                expect: version.to_string(),
                label: None,
            });
            checks.push(manifest::VersionCheck {
                executable: "cargo".into(),
                args: vec!["--version".into()],
                expect: version.to_string(),
                label: Some("cargo".into()),
            });
            // Cargo normally searches PATH for rustc; pin it so the managed
            // toolchain keeps working from a shim or an absolute path too.
            env.insert("RUSTC".into(), "{root}/bin/rustc".into());
        }
        "java" => {
            let display = java_display_version(version);
            let legacy = version
                .split('+')
                .next()
                .unwrap_or(version)
                .starts_with("8.");
            executables.push(manifest::Executable {
                name: "java".into(),
                path: "bin/java".into(),
            });
            executables.push(manifest::Executable {
                name: "javac".into(),
                path: "bin/javac".into(),
            });
            executables.push(manifest::Executable {
                name: "jar".into(),
                path: "bin/jar".into(),
            });
            if !legacy {
                executables.push(manifest::Executable {
                    name: "jshell".into(),
                    path: "bin/jshell".into(),
                });
            }
            checks.push(manifest::VersionCheck {
                executable: "java".into(),
                // Java 8 only has the single-dash form, and prints it to
                // stderr; the check runner reads both streams.
                args: vec![if legacy {
                    "-version".into()
                } else {
                    "--version".into()
                }],
                expect: display.clone(),
                label: None,
            });
            checks.push(manifest::VersionCheck {
                executable: "javac".into(),
                args: vec!["-version".into()],
                expect: format!("javac {display}"),
                label: Some("javac".into()),
            });
            if !legacy {
                checks.push(manifest::VersionCheck {
                    executable: "jar".into(),
                    args: vec!["--version".into()],
                    expect: format!("jar {display}"),
                    label: Some("jar".into()),
                });
                checks.push(manifest::VersionCheck {
                    executable: "jshell".into(),
                    args: vec!["--version".into()],
                    expect: format!("jshell {display}"),
                    label: Some("jshell".into()),
                });
            }
            env.insert("JAVA_HOME".into(), "{root}".into());
        }
        "bun" => {
            executables.push(manifest::Executable {
                name: "bun".into(),
                path: "bun".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "bun".into(),
                args: vec!["--version".into()],
                expect: version.to_string(),
                label: None,
            });
            // Bun's zip has no bin/ directory: the executable is the root.
            path_dirs = vec![".".to_string()];
        }
        "deno" => {
            executables.push(manifest::Executable {
                name: "deno".into(),
                path: "deno".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "deno".into(),
                args: vec!["--version".into()],
                expect: format!("deno {version}"),
                label: None,
            });
            path_dirs = vec![".".to_string()];
        }
        "dotnet" => {
            executables.push(manifest::Executable {
                name: "dotnet".into(),
                path: "dotnet".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "dotnet".into(),
                args: vec!["--version".into()],
                expect: version.to_string(),
                label: None,
            });
            checks.push(manifest::VersionCheck {
                executable: "dotnet".into(),
                args: vec!["--list-sdks".into()],
                expect: version.to_string(),
                label: Some("SDKs".into()),
            });
            if let Some(runtime) = bundled.get("runtime") {
                checks.push(manifest::VersionCheck {
                    executable: "dotnet".into(),
                    args: vec!["--list-runtimes".into()],
                    expect: format!("Microsoft.NETCore.App {runtime}"),
                    label: Some("runtime".into()),
                });
            }
            env.insert("DOTNET_ROOT".into(), "{root}".into());
            path_dirs = vec![".".to_string()];
        }
        "clang" => {
            // lld's generic `bin/lld` driver refuses to link; the drivers are
            // `ld.lld` (ELF), `ld64.lld` and `lld-link`, so expose/check
            // `ld.lld`. lldb needs liblldb.so from the prefix's lib/.
            for (name, path) in [
                ("clang", "bin/clang"),
                ("clang++", "bin/clang++"),
                ("ld.lld", "bin/ld.lld"),
                ("lldb", "bin/lldb"),
            ] {
                executables.push(manifest::Executable {
                    name: name.into(),
                    path: path.into(),
                });
            }
            checks.push(manifest::VersionCheck {
                executable: "clang".into(),
                args: vec!["--version".into()],
                expect: format!("clang version {version}"),
                label: None,
            });
            checks.push(manifest::VersionCheck {
                executable: "clang++".into(),
                args: vec!["--version".into()],
                expect: format!("clang version {version}"),
                label: Some("clang++".into()),
            });
            checks.push(manifest::VersionCheck {
                executable: "ld.lld".into(),
                args: vec!["--version".into()],
                expect: format!("LLD {version}"),
                label: Some("ld.lld".into()),
            });
            // lldb is shipped but not executed as a version check: the
            // official LLVM Linux build links it against a specific libpython
            // shared object most hosts do not have (e.g.
            // libpython3.14.so.1.0), so executing it in CI would fail even
            // though clang/clang++/ld.lld work. lldb stays in bin/ and finds
            // liblldb.so via LD_LIBRARY_PATH; users who need it install the
            // matching libpython package.
            env.insert("LD_LIBRARY_PATH".into(), "{root}/lib".into());
        }
        "flutter" => {
            executables.push(manifest::Executable {
                name: "flutter".into(),
                path: "bin/flutter".into(),
            });
            executables.push(manifest::Executable {
                name: "dart".into(),
                path: "bin/dart".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "flutter".into(),
                args: vec!["--version".into()],
                expect: format!("Flutter {version}"),
                label: None,
            });
            if let Some(dart) = bundled.get("dart") {
                checks.push(manifest::VersionCheck {
                    executable: "dart".into(),
                    args: vec!["--version".into()],
                    expect: format!("Dart SDK version: {dart}"),
                    label: Some("Dart".into()),
                });
            }
        }
        "composer" => {
            executables.push(manifest::Executable {
                name: "composer".into(),
                path: "bin/composer".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "composer".into(),
                args: vec!["--version".into()],
                expect: format!("Composer version {version}"),
                label: None,
            });
        }
        "zig" => {
            executables.push(manifest::Executable {
                name: "zig".into(),
                path: "zig".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "zig".into(),
                args: vec!["version".into()],
                expect: version.to_string(),
                label: None,
            });
            // Zig's archive has no bin/: the executable is at the root.
            path_dirs = vec![".".to_string()];
        }
        "kotlin" => {
            executables.push(manifest::Executable {
                name: "kotlinc".into(),
                path: "bin/kotlinc".into(),
            });
            executables.push(manifest::Executable {
                name: "kotlin".into(),
                path: "bin/kotlin".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "kotlinc".into(),
                args: vec!["-version".into()],
                expect: version.to_string(),
                label: None,
            });
        }
        "cmake" => {
            for name in ["cmake", "ctest", "cpack"] {
                executables.push(manifest::Executable {
                    name: name.into(),
                    path: format!("bin/{name}"),
                });
            }
            checks.push(manifest::VersionCheck {
                executable: "cmake".into(),
                args: vec!["--version".into()],
                expect: format!("cmake version {version}"),
                label: None,
            });
            checks.push(manifest::VersionCheck {
                executable: "ctest".into(),
                args: vec!["--version".into()],
                expect: format!("ctest version {version}"),
                label: Some("ctest".into()),
            });
        }
        "gradle" => {
            executables.push(manifest::Executable {
                name: "gradle".into(),
                path: "bin/gradle".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "gradle".into(),
                args: vec!["--version".into()],
                expect: format!("Gradle {version}"),
                label: None,
            });
        }
        "maven" => {
            executables.push(manifest::Executable {
                name: "mvn".into(),
                path: "bin/mvn".into(),
            });
            checks.push(manifest::VersionCheck {
                executable: "mvn".into(),
                args: vec!["--version".into()],
                expect: format!("Apache Maven {version}"),
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
        env,
        path_dirs,
        mirror: mirror_for(package),
    })
}

/// SDKs that write into their own folder while they run get a per-user
/// mirror (see [`manifest::Mirror`]). Flutter rewrites `bin/cache/engine.*`
/// on every start and runs Git (also `git fetch`) on its own checkout.
fn mirror_for(package: &str) -> Option<manifest::Mirror> {
    match package {
        "flutter" => Some(manifest::Mirror {
            copy: vec!["bin".into(), "bin/internal".into(), "bin/cache".into()],
            // Flutter's tool rewrites its own pub state on every start.
            copy_tree: vec![
                ".git".into(),
                ".dart_tool".into(),
                "packages/flutter_tools/.dart_tool".into(),
            ],
        }),
        _ => None,
    }
}

fn base_manifest(cfg: &Config, item: &PlanItem) -> Result<Value, String> {
    let package = package_config(cfg, &item.package)?;
    let mut manifest = Map::new();
    manifest.insert("schemaVersion".into(), json!(1));
    manifest.insert("kind".into(), json!("rast-package"));
    manifest.insert("id".into(), json!(item.package));
    manifest.insert("type".into(), json!(package.package_type()));
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
    manifest.insert(
        "minRastVersion".into(),
        json!(package
            .min_rast_version
            .as_deref()
            .unwrap_or(&cfg.min_rast_version)),
    );
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
    let app = package_config(cfg, &item.package)?.app.clone();
    if let Some(app) = &app {
        manifest.insert("app".into(), app.clone());
    } else {
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
    }

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
            // An app package is built by RAST's own workflow.
            let kind = if app.is_some() { "built" } else { "upstream" };
            provenance.insert("kind".into(), json!(kind));
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
                "rust" => json!({
                    "glibc": "2.17",
                    "notes": "Standalone rust-installer toolchain (rustc, cargo, rust-std, rustfmt, clippy); rustup is not involved. Cargo/rustup state stays in the user's home, not in this prefix."
                }),
                "java" => json!({
                    "glibc": "2.17",
                    "notes": "Eclipse Temurin JDK for glibc Linux; set JAVA_HOME to the prefix. Alpine/musl builds are not published."
                }),
                "bun" => json!({
                    "glibc": "2.17",
                    "notes": "Bun's glibc build; Alpine/musl builds are not published. BUN_INSTALL and caches stay per user."
                }),
                "deno" => json!({
                    "glibc": "2.27",
                    "notes": "Deno's glibc Linux build; DENO_DIR and caches stay per user, not in the shared prefix."
                }),
                "dotnet" => json!({
                    "glibc": "2.27",
                    "notes": ".NET SDK (includes the runtime); DOTNET_ROOT points at the prefix. NUGET_PACKAGES and DOTNET_CLI_HOME stay per user."
                }),
                "clang" => json!({
                    "glibc": "2.34",
                    "notes": "LLVM's official Linux release binaries (built on Ubuntu 22.04): clang and clang++ compilers, ld.lld linker and lldb (shipped; needs a matching host libpython shared object to run); LLVM libraries are in lib/ and the libc++ headers in include/c++/v1."
                }),
                "flutter" => json!({
                    "glibc": "2.31",
                    "notes": "Google's official Flutter Linux SDK (includes the Dart SDK). Flutter keeps its cache and downloaded artifacts inside <prefix>/bin/cache, so it needs write access there; use a writable home for its per-user state."
                }),
                "composer" => json!({
                    "notes": "composer.phar is installed read-only at the prefix root; bin/composer is a generated wrapper that runs it with the PHP interpreter found on PATH. A PHP CLI (for example the PHP runtime in this catalog) must be on PATH."
                }),
                "zig" => json!({
                    "notes": "Statically linked Zig compiler; Zig caches (ZIG_GLOBAL_CACHE_DIR/ZIG_LOCAL_CACHE_DIR) stay per user."
                }),
                "kotlin" => json!({
                    "notes": "Runs on the JVM: kotlinc and kotlin need a JDK (java on PATH, e.g. the Temurin JDK from this catalog)."
                }),
                "cmake" => json!({
                    "glibc": "2.31",
                    "notes": "CMake's official Linux binaries (cmake, ctest, cpack); no extra build toolchain is required for CMake itself."
                }),
                "gradle" => json!({
                    "notes": "Architecture-independent distribution; gradle needs a JDK (java on PATH, e.g. the Temurin JDK from this catalog). Gradle's user home stays at ~/.gradle."
                }),
                "maven" => json!({
                    "notes": "Architecture-independent distribution; mvn needs a JDK (java on PATH, e.g. the Temurin JDK from this catalog). Maven's local repository stays at ~/.m2."
                }),
                "ai-agent" => json!({
                    "glibc": "2.35",
                    "notes": "RAST runs the server program (bin/rast-ai-agent) and opens the window (ui/) with its window SDK."
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
        let format = result
            .format
            .clone()
            .unwrap_or_else(|| "tar.gz".to_string());
        let strip_components = result.strip_components.unwrap_or(1);
        let mut extra_archives = Vec::new();
        if item.package != "php" {
            let info = item.targets.get(&result.target).ok_or_else(|| {
                format!(
                    "{} {} has no plan target {}",
                    item.package, item.version, result.target
                )
            })?;
            if !info.archives.is_empty() {
                if info.archives.len() != result.archives.len() {
                    return Err(format!(
                        "{} {} {}: the plan lists {} component archives but {} were verified",
                        item.package,
                        item.version,
                        result.target,
                        info.archives.len(),
                        result.archives.len()
                    ));
                }
                for (planned, verified) in info.archives.iter().zip(&result.archives) {
                    extra_archives.push(json!({
                        "name": planned.name,
                        "url": planned.url,
                        "sha256": verified.sha256,
                        "size": verified.size,
                        "format": verified.format,
                        "stripComponents": verified.strip_components,
                    }));
                }
            }
        }
        let mut artifact = Map::new();
        artifact.insert("target".into(), json!(result.target));
        artifact.insert("url".into(), json!(url));
        artifact.insert("sha256".into(), json!(sha256));
        artifact.insert("size".into(), json!(size));
        artifact.insert("format".into(), json!(format));
        artifact.insert("stripComponents".into(), json!(strip_components));
        if format == "phar" {
            let path = phar_path_for(&item.package)
                .ok_or_else(|| format!("{} is not a package with phar artifacts", item.package))?;
            artifact.insert("phar".into(), json!({ "path": path }));
        }
        artifact.insert("provenance".into(), provenance);
        if requires
            .as_object()
            .is_some_and(|object| !object.is_empty())
        {
            artifact.insert("requires".into(), requires);
        }
        if !extra_archives.is_empty() {
            artifact.insert("archives".into(), Value::Array(extra_archives));
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
    match item.package.as_str() {
        "rust" => notes.push(
            "Standalone rust-installer toolchain: rustup is not involved, so rust-toolchain.toml is not honored (it is a rustup feature); pin a version with RAST instead. CARGO_HOME/RUSTUP_HOME stay in each user's home, never in this read-only prefix."
                .into(),
        ),
        "java" => notes.push(
            "JAVA_HOME is the installed prefix (runtime.env); java, javac, jar and jshell are in bin/."
                .into(),
        ),
        "bun" => notes.push(
            "The bun executable is at the prefix root; BUN_INSTALL and caches must stay in each user's home."
                .into(),
        ),
        "deno" => notes.push(
            "The deno executable is at the prefix root; DENO_DIR must stay in each user's home."
                .into(),
        ),
        "dotnet" => notes.push(
            "DOTNET_ROOT is the installed prefix; the SDK includes the runtime. NUGET_PACKAGES and DOTNET_CLI_HOME must stay in each user's home."
                .into(),
        ),
        "clang" => notes.push(
            "clang and clang++ are the C/C++ compilers; the linker is ld.lld. lldb is shipped and finds liblldb.so via LD_LIBRARY_PATH=<prefix>/lib, but the official build needs a matching host libpython shared object (install the matching python3 library to use lldb; clang/clang++/ld.lld work without it). The C++ standard library ships as libc++: headers in include/c++/v1, libraries in lib/ (link with -stdlib=libc++)."
                .into(),
        ),
        "flutter" => notes.push(
            "flutter and dart are in bin/. The SDK cache and downloaded artifacts live inside the prefix (bin/cache), so Flutter needs a writable prefix or a prewarmed cache; Linux host SDK archives are published for x86_64 only."
                .into(),
        ),
        "composer" => notes.push(
            "composer.phar is never executable and is only run through the generated bin/composer wrapper, which uses the php found on PATH. Install a PHP runtime first."
                .into(),
        ),
        "zig" => notes.push(
            "zig is a single statically linked executable at the prefix root (pathDirs \".\"); its caches stay in each user's home."
                .into(),
        ),
        "kotlin" => notes.push(
            "kotlinc and kotlin are in bin/ and run on a JVM: a JDK must be on PATH when they are used."
                .into(),
        ),
        "cmake" => notes.push("cmake, ctest and cpack are in bin/.".into()),
        "ai-agent" => notes.push(
            "On its first start AI Agent takes the chats, providers, agents, skills, limits and chat files RAST kept for it (RAST keeps its copy). Its data then lives in the app's own folder."
                .into(),
        ),
        "gradle" => notes.push(
            "gradle is in bin/ and runs on a JVM: a JDK must be on PATH; Gradle keeps its user home in ~/.gradle."
                .into(),
        ),
        "maven" => notes.push(
            "mvn is in bin/ and runs on a JVM: a JDK must be on PATH; Maven keeps its local repository in ~/.m2."
                .into(),
        ),
        _ => {}
    }
    // Explicit compatibility reason when a configured target has no artifact.
    for (target, _) in &cfg.targets.entries {
        if item.targets.contains_key(target) {
            continue;
        }
        if let Some(reason) = unavailable_reason(&item.package, target) {
            notes.push(format!("Not available for {target}: {reason}."));
        }
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

/// The catalog package entries (one per configured package, no versions yet).
///
/// Package-level metadata comes from `catalog.config.json`; a configured
/// `icon` file is loaded, validated and embedded as base64 in the entry (the
/// same [`manifest::PackageIcon`] shape as a manifest icon). Package entries
/// are regenerated from the config by every command, so interim and final
/// catalogs always carry the current icons.
fn package_entries(cfg: &Config) -> Result<Vec<(String, Value)>, String> {
    let mut entries = Vec::with_capacity(cfg.packages.entries.len());
    for (id, package) in &cfg.packages.entries {
        let mut entry = json!({
            "id": id,
            "type": package.package_type(),
            "name": package.name,
            "description": package.description,
            "homepage": package.homepage,
            "license": package.license,
            "channels": package.channels,
            "versions": [],
            "detected": [],
        });
        if let Some(path) = package.icon_path(&cfg.config_dir) {
            let icon = manifest::package_icon_from_file(&path)
                .map_err(|error| format!("catalog.config.json packages.{id}.icon: {error}"))?;
            let icon = serde_json::to_value(&icon)
                .map_err(|error| format!("cannot serialize the {id} icon: {error}"))?;
            entry
                .as_object_mut()
                .expect("a package entry is a JSON object")
                .insert("icon".into(), icon);
        }
        entries.push((id.clone(), entry));
    }
    Ok(entries)
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
    let mut packages = package_entries(cfg)?;
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
    let mut packages = package_entries(cfg)?;
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
        if let Some(icon) = &package.icon {
            if let Err(error) = manifest::validate_icon(icon) {
                problems.push(format!("{} icon: {error}", package.id));
            }
        }
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
                        // A target can have several archives (Rust components);
                        // every one of them is re-downloaded and re-hashed.
                        let artifact: manifest::Artifact = serde_json::from_value(artifact.clone())
                            .map_err(|error| format!("invalid artifact: {error}"))?;
                        for archive in artifact.archives() {
                            let url = archive.url;
                            if mode == DownloadMode::New && !url.starts_with(&cfg.repository) {
                                continue;
                            }
                            let final_url = head_final_url(url)?;
                            final_host_allowed(url, &final_url, &hosts)?;
                            let (sha256, size) = download(url, &work.path().join("a"))?;
                            if sha256 != archive.sha256 || size != archive.size {
                                return Err(format!(
                                    "{url}: published bytes differ from the manifest"
                                ));
                            }
                            log(format!("ok {url}"));
                        }
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
    /// Primary archive format/strip; absent in results written before
    /// multi-archive support (default tar.gz / strip 1).
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    strip_components: Option<u32>,
    /// Additional component archives (Rust), in plan order.
    #[serde(default)]
    archives: Vec<ArchiveResult>,
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
    // The plan artifact (plan.json + previous/). With several artifacts GitHub
    // unpacks each into `<path>/<name>`; with only the plan artifact (e.g. a
    // `republish` run where verify/php are skipped) it may unpack the files
    // directly into `<path>`. Support both layouts.
    let plan_dir = artifacts.join("plan");
    if plan_dir.is_dir() {
        copy_tree(&plan_dir, work)?;
    } else if artifacts.join("plan.json").is_file() {
        fs::copy(artifacts.join("plan.json"), work.join("plan.json"))
            .map_err(|error| format!("cannot copy plan.json: {error}"))?;
        let previous = artifacts.join("previous");
        if previous.is_dir() {
            copy_tree(&previous, &work.join("previous"))?;
        }
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

/// `<out>/<package>-<target>.tar.gz`: both targets of a version are uploaded
/// to the same release, so the asset name must carry the target.
fn write_php_archive(
    stage: &Path,
    package: &str,
    target: &str,
    out: &Path,
) -> Result<PathBuf, String> {
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
    let archive = out.join(format!("{package}-{target}.tar.gz"));
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
    // static-php-cli lists upstream releases through the GitHub API, which
    // rate-limits anonymous calls (HTTP 403). The workflow provides GH_TOKEN;
    // hand it to the container as GITHUB_TOKEN (by name, so it stays out of
    // the process arguments).
    if let Some(token) = gh_token() {
        docker
            .env("GITHUB_TOKEN", token)
            .args(["-e", "GITHUB_TOKEN"]);
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
    let archive = write_php_archive(&stage, &package, target, out)?;
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

    #[test]
    fn php_archives_are_named_per_target() {
        // Both targets of a version are uploaded to one release; a shared
        // name made one build overwrite the other ("published bytes differ").
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        fs::create_dir_all(stage.join("bin")).unwrap();
        fs::write(stage.join("bin/php"), b"#!/bin/sh\n").unwrap();
        let out = dir.path().join("dist");
        let x86 = write_php_archive(&stage, "php-8.4.28", "linux-x86_64", &out).unwrap();
        let arm = write_php_archive(&stage, "php-8.4.28", "linux-aarch64", &out).unwrap();
        assert_eq!(x86.file_name().unwrap(), "php-8.4.28-linux-x86_64.tar.gz");
        assert_eq!(arm.file_name().unwrap(), "php-8.4.28-linux-aarch64.tar.gz");
        assert!(x86.is_file() && arm.is_file());
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        archive::extract_tar_gz(&x86, &root, 1, u64::MAX, &mut |_| {}, &|| false).unwrap();
        assert!(
            root.join("bin/php").is_file(),
            "the archive holds php-8.4.28/…"
        );
    }

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

    const TINY_NODE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 2 2 22h20z"/></svg>"#;
    const TINY_PYTHON_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/></svg>"#;

    /// `test_config` with two package icons on disk: relative paths resolved
    /// against `dir` as if it were the directory of catalog.config.json.
    fn icon_config(dir: &Path) -> Config {
        fs::create_dir_all(dir.join("icons")).unwrap();
        fs::write(dir.join("icons/nodejs.svg"), TINY_NODE_SVG).unwrap();
        fs::write(dir.join("icons/python.svg"), TINY_PYTHON_SVG).unwrap();
        let mut cfg = test_config();
        cfg.config_dir = dir.to_path_buf();
        cfg.packages.get_mut("nodejs").unwrap().icon = Some("icons/nodejs.svg".into());
        cfg.packages.get_mut("python").unwrap().icon = Some("icons/python.svg".into());
        cfg
    }

    fn assert_package_icons(document: &Value) {
        for (id, svg) in [("nodejs", TINY_NODE_SVG), ("python", TINY_PYTHON_SVG)] {
            let package = document["packages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|package| package["id"] == id)
                .unwrap_or_else(|| panic!("catalog has no {id} package"));
            assert_eq!(package["icon"]["mediaType"], "image/svg+xml", "{id}");
            let data = package["icon"]["data"].as_str().unwrap();
            assert_eq!(B64.decode(data).unwrap(), svg.as_bytes(), "{id}");
            assert!(package["icon"].get("attribution").is_none(), "{id}");
        }
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
        // The label is passed in so secret scanners do not take this
        // throwaway test key for a committed one.
        let label = "PRIVATE KEY";
        let private_pem = format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
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
        let packages = package_entries(&cfg).unwrap();
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
        assert!(
            packages[0].1.get("icon").is_none(),
            "no configured icon means no icon key"
        );
    }

    #[test]
    fn package_icons_are_loaded_relative_to_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = icon_config(dir.path());
        let packages = package_entries(&cfg).unwrap();
        let node = &packages[0].1;
        assert_eq!(node["icon"]["mediaType"], "image/svg+xml");
        assert_eq!(
            B64.decode(node["icon"]["data"].as_str().unwrap()).unwrap(),
            TINY_NODE_SVG.as_bytes()
        );
        assert!(node["icon"].get("attribution").is_none());
        let python = &packages[1].1;
        assert_eq!(
            B64.decode(python["icon"]["data"].as_str().unwrap())
                .unwrap(),
            TINY_PYTHON_SVG.as_bytes()
        );

        // Absolute icon paths are accepted too.
        let mut cfg = icon_config(dir.path());
        cfg.packages.get_mut("python").unwrap().icon = Some(
            dir.path()
                .join("icons/python.svg")
                .to_string_lossy()
                .into_owned(),
        );
        let packages = package_entries(&cfg).unwrap();
        assert!(packages[1].1.get("icon").is_some());

        // A missing file is a clear error naming the package and the path.
        cfg.packages.get_mut("nodejs").unwrap().icon = Some("icons/missing.svg".into());
        let error = package_entries(&cfg).unwrap_err();
        assert!(error.contains("packages.nodejs.icon"), "{error}");
        assert!(error.contains("missing.svg"), "{error}");

        // The icon goes through the same validation as a manifest icon.
        std::fs::write(dir.path().join("icons/bad.svg"), "<svg onload=\"x\"></svg>").unwrap();
        let mut cfg = icon_config(dir.path());
        cfg.packages.get_mut("python").unwrap().icon = Some("icons/bad.svg".into());
        let error = package_entries(&cfg).unwrap_err();
        assert!(error.contains("event handler"), "{error}");
    }

    #[test]
    fn package_icons_reach_interim_and_final_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = icon_config(dir.path());
        let previous_dir = dir.path().join("previous");
        let old = previous_manifest(&cfg, "3.12.1", &["linux-x86_64"]);
        write_previous_catalog(&previous_dir, 4, vec![old]);
        let mut carry = python_item_for("3.12.1", &["linux-x86_64"], 1);
        carry.status = "carry".into();
        let plan = Plan {
            generated_at: "2026-01-01T00:00:00Z".into(),
            previous_sequence: 4,
            withheld: Vec::new(),
            items: vec![carry],
            dropped: Vec::new(),
        };
        let plan_path = dir.path().join("plan.json");
        write_json(&plan_path, &serde_json::to_value(&plan).unwrap()).unwrap();

        let interim = dir.path().join("interim/catalog.json");
        interim_with_config(&cfg, &plan_path, &previous_dir, &interim).unwrap();
        let document: Value = serde_json::from_slice(&fs::read(&interim).unwrap()).unwrap();
        assert_package_icons(&document);
        assert!(catalog_problems(&document).unwrap().is_empty());

        let results_dir = dir.path().join("results");
        fs::create_dir_all(&results_dir).unwrap();
        let final_out = dir.path().join("out/catalog.json");
        assemble_with_config(
            &cfg,
            &plan_path,
            &previous_dir,
            &results_dir,
            None,
            &final_out,
        )
        .unwrap();
        let document: Value = serde_json::from_slice(&fs::read(&final_out).unwrap()).unwrap();
        assert_package_icons(&document);
        assert!(catalog_problems(&document).unwrap().is_empty());
    }

    #[test]
    fn catalog_problems_reject_a_bad_package_icon() {
        let cfg = test_config();
        let mut packages = package_entries(&cfg).unwrap();
        let document = catalog_doc(&cfg, 1, &mut packages).unwrap();
        assert!(catalog_problems(&document).unwrap().is_empty());

        let mut unknown = document.clone();
        unknown["packages"][0]["icon"] = json!({"mediaType": "image/gif", "data": "AAAA"});
        let problems = catalog_problems(&unknown).unwrap();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("nodejs icon")
                    && problem.contains("not supported")),
            "{problems:?}"
        );

        let mut script = document.clone();
        script["packages"][0]["icon"] = json!({
            "mediaType": "image/svg+xml",
            "data": B64.encode("<svg><script>alert(1)</script></svg>"),
        });
        let problems = catalog_problems(&script).unwrap();
        assert!(
            problems.iter().any(|problem| problem.contains("script")),
            "{problems:?}"
        );

        // A malformed icon envelope is a parse error, not a list of problems.
        let mut malformed = document;
        malformed["packages"][0]["icon"] = json!({"mediaType": "image/svg+xml"});
        assert!(catalog_problems(&malformed).is_err());
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
        let mut packages = package_entries(&cfg).unwrap();
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
        let mut packages = package_entries(&cfg).unwrap();
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
    // ------------------------------------------------- managed runtime providers

    const ADOPTIUM_AVAILABLE_FIXTURE: &str = r#"{"available_lts_releases":[8,11,17,21,25],"available_releases":[8,11,16,17,18,19,20,21,22,23,24,25,26,27],"most_recent_feature_release":27,"most_recent_feature_version":28,"most_recent_lts":25,"tip_version":28}"#;
    const ADOPTIUM_25_X64: &str = r#"[{"binary":{"architecture":"x64","download_count":1543676,"heap_size":"normal","image_type":"jdk","jvm_impl":"hotspot","os":"linux","package":{"checksum":"68982529869257d0ee6b824ec5aa270ff8b118bb438670da0b97ebdab178d7fb","checksum_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz.sha256.txt","download_count":1543676,"link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz","metadata_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz.json","name":"OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz","signature_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz.sig","size":141341188},"project":"jdk","scm_ref":"jdk-25.0.5+8_adopt","updated_at":"2026-10-22T12:28:52Z"},"release_link":"https://github.com/adoptium/temurin25-binaries/releases/tag/jdk-25.0.5%2B8","release_name":"jdk-25.0.5+8","vendor":"eclipse","version":{"build":8,"major":25,"minor":0,"openjdk_version":"25.0.5+8-LTS","optional":"LTS","security":5,"semver":"25.0.5+8.0.LTS"}}]"#;
    const ADOPTIUM_25_ARM64: &str = r#"[{"binary":{"architecture":"aarch64","download_count":326660,"heap_size":"normal","image_type":"jdk","jvm_impl":"hotspot","os":"linux","package":{"checksum":"bb5b2ded252f21c6090cf780b0a408ad0d83bbb557e702955643557f13231880","checksum_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz.sha256.txt","download_count":326660,"link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz","metadata_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz.json","name":"OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz","signature_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz.sig","size":140178170},"project":"jdk","scm_ref":"jdk-25.0.5+8_adopt","updated_at":"2026-10-23T08:50:58Z"},"release_link":"https://github.com/adoptium/temurin25-binaries/releases/tag/jdk-25.0.5%2B8","release_name":"jdk-25.0.5+8","vendor":"eclipse","version":{"build":8,"major":25,"minor":0,"openjdk_version":"25.0.5+8-LTS","optional":"LTS","security":5,"semver":"25.0.5+8.0.LTS"}}]"#;
    const RUST_CHANNEL_TOML_FIXTURE: &str = r#"manifest-version = "2"
date = "2026-11-12"

[pkg.rustc]
version = "1.100.0 (0ef1bc386 2026-11-09)"

[pkg.rustc.target.x86_64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rustc-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
hash = "67b0e85ff4b3eee83c48b8ae1da10dd0bcd60b43d02cd0e7df78c59d19f6b960"

[pkg.rustc.target.aarch64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rustc-1.100.0-aarch64-unknown-linux-gnu.tar.gz"
hash = "7ccc960846f81e016086f1931dedadd837a3317da80e0114df68651145c0f43b"

[pkg.cargo]
version = "0.101.0 (3acc79c0c 2026-11-05)"

[pkg.cargo.target.x86_64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/cargo-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
hash = "4e7fc3cbe98e23c88a3c6ce37f834e3ffbc91536789782c4c4a9670df6b16a49"

[pkg.cargo.target.aarch64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/cargo-1.100.0-aarch64-unknown-linux-gnu.tar.gz"
hash = "b3acccbba065da7e97c20592442f104bd9554d54bf87a323bba8a1d6e61282eb"

[pkg.rust-std]
version = "1.100.0 (0ef1bc386 2026-11-09)"

[pkg.rust-std.target.x86_64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
hash = "61eb09d68b338e230d698716927bd4623049c9910ef4a4972e66d763d66ed9e1"

[pkg.rust-std.target.aarch64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-aarch64-unknown-linux-gnu.tar.gz"
hash = "89b01323a6611c9382a401282fb66eb80f1f3241df3e139d3df41afbd2da314b"

[pkg.rustfmt-preview]
version = "1.11.0"

[pkg.rustfmt-preview.target.x86_64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rustfmt-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
hash = "b62ba6e046a5580c52f24a817101734b73e76ea0724fc6a21d57fba4db345ced"

[pkg.rustfmt-preview.target.aarch64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rustfmt-1.100.0-aarch64-unknown-linux-gnu.tar.gz"
hash = "e555f81eccd5e2f741732be3f6784d073946eadbf2e00f4112dfdabc6c36c13e"

[pkg.clippy-preview]
version = "0.1.100"

[pkg.clippy-preview.target.x86_64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/clippy-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
hash = "277a9abd9378561d11fb67a9eb5c08895e207d07dfd985a67197aa47e030856f"

[pkg.clippy-preview.target.aarch64-unknown-linux-gnu]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/clippy-1.100.0-aarch64-unknown-linux-gnu.tar.gz"
hash = "41920025e43c858e777177468698a88fc270bcceb6ba61059aac1dfee4da1d37"
"#;
    const BUN_RELEASE_FIXTURE: &str = r#"{"tag_name":"bun-v1.4.4","prerelease":false,"draft":false,"published_at":"2026-10-22T10:11:32Z","html_url":"https://github.com/oven-sh/bun/releases/tag/bun-v1.4.4","assets":[{"name":"SHASUMS256.txt","size":3431,"digest":"sha256:86629f14fe256b487ceba5170217e13787c49d0cc0fe4bb6397ce5767d3b8f65","browser_download_url":"https://github.com/oven-sh/bun/releases/download/bun-v1.4.4/SHASUMS256.txt"},{"name":"bun-linux-aarch64.zip","size":51644673,"digest":"sha256:baa6bb36f51d6e58561e02bbea825038a9df454f673d6d776e8bbf86f8c7850e","browser_download_url":"https://github.com/oven-sh/bun/releases/download/bun-v1.4.4/bun-linux-aarch64.zip"},{"name":"bun-linux-x64.zip","size":51794595,"digest":"sha256:bf8ccbfb2b0b80159e0f9a6d15c55317add8077534dbc6130a3edb52fccb395c","browser_download_url":"https://github.com/oven-sh/bun/releases/download/bun-v1.4.4/bun-linux-x64.zip"}]}"#;
    const DENO_RELEASE_FIXTURE: &str = r#"{"tag_name":"v2.9.8","prerelease":false,"draft":false,"published_at":"2026-10-21T15:28:53Z","html_url":"https://github.com/denoland/deno/releases/tag/v2.9.8","assets":[{"name":"deno-aarch64-unknown-linux-gnu.zip","size":40120596,"digest":"sha256:d49596c20b9788cf8f5d8ddf644103d4bde56ed239753f0bdb92ba9361d51905","browser_download_url":"https://github.com/denoland/deno/releases/download/v2.9.8/deno-aarch64-unknown-linux-gnu.zip"},{"name":"deno-aarch64-unknown-linux-gnu.zip.sha256sum","size":101,"digest":"sha256:8eb865b23b330c730e109461eb3c1e185049ace2cd20a2002e7be1c023e284d0","browser_download_url":"https://github.com/denoland/deno/releases/download/v2.9.8/deno-aarch64-unknown-linux-gnu.zip.sha256sum"},{"name":"deno-x86_64-unknown-linux-gnu.zip","size":41963064,"digest":"sha256:e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609","browser_download_url":"https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip"},{"name":"deno-x86_64-unknown-linux-gnu.zip.sha256sum","size":100,"digest":"sha256:f41fbb4278955d03b735c37f957470a1432bdae643d673866e913eea6e4a7855","browser_download_url":"https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip.sha256sum"}]}"#;
    const DOTNET_INDEX_ENTRY_FIXTURE: &str = r#"{"channel-version":"10.0","latest-release":"10.0.15","latest-release-date":"2026-11-10","latest-sdk":"10.0.404","latest-runtime":"10.0.15","support-phase":"active","release-type":"lts","releases.json":"https://builds.dotnet.microsoft.com/dotnet/release-metadata/10.0/releases.json","eol-date":"2028-11-14"}"#;
    const DOTNET_RELEASES_FIXTURE: &str = r#"{"channel-version":"10.0","latest-sdk":"10.0.404","releases":[{"release-date":"2026-11-10","release-version":"10.0.15","runtime":{"version":"10.0.15"},"sdk":{"version":"10.0.404","files":[{"name":"dotnet-sdk-linux-arm64.tar.gz","rid":"linux-arm64","hash":"3533a3869da219cac45c567aadac3d8e96cdf1c6a04fe1c1d5fdeefea5d45af8424488f0144730d11b4a751f7ad546fb11e4c2b2725cd8401efee726711f8f11","url":"https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.404/dotnet-sdk-10.0.404-linux-arm64.tar.gz"},{"name":"dotnet-sdk-linux-x64.tar.gz","rid":"linux-x64","hash":"577d0eb17fec33d029104c5c63a1ab0accc3e28776274abef2218dbac5af18d2109b5406d26b31d76f5c245215e3ba0926a94c20246c672c41847c9e1eb1dc50","url":"https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.404/dotnet-sdk-10.0.404-linux-x64.tar.gz"}]}}]}"#;

    const CLANG_RELEASE_FIXTURE: &str = r#"{"tag_name":"llvmorg-23.1.5","prerelease":false,"draft":false,"published_at":"2026-11-03T21:35:47Z","html_url":"https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.5","assets":[{"name":"LLVM-23.1.5-Linux-X64.tar.xz","size":2014093084,"digest":"sha256:f72edb0b36580ece7f52e23858f00c18fad4e87395ea7ad05d7e55c39f8fc78a","browser_download_url":"https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.5/LLVM-23.1.5-Linux-X64.tar.xz"},{"name":"LLVM-23.1.5-Linux-X64.tar.xz.jsonl","size":10990,"digest":"sha256:b2bb5d229cc071b00858b4ab0e414f4ba7218b7cdfa858b2cac281687769ed81","browser_download_url":"https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.5/LLVM-23.1.5-Linux-X64.tar.xz.jsonl"},{"name":"LLVM-23.1.5-Linux-ARM64.tar.xz","size":1828141344,"digest":"sha256:44f1e4b7a125b2763d56386a199928dd57ec32e151cc6042dce2f7b0da7bb651","browser_download_url":"https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.5/LLVM-23.1.5-Linux-ARM64.tar.xz"},{"name":"LLVM-23.1.5-Linux-ARM64.tar.xz.jsonl","size":11229,"digest":"sha256:e2cb38e1559d8da56705a4d4f6d4e148b4155e9700d4dcedfffd3b389015b808","browser_download_url":"https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.5/LLVM-23.1.5-Linux-ARM64.tar.xz.jsonl"}]}"#;
    const FLUTTER_RELEASES_FIXTURE: &str = r#"{"current_release":{"beta":"d040ef5c5c4c8be0948207fadd35e87305920146","stable":"ab2912035e160d9cad6306df307576da6128ebdb"},"releases":[{"hash":"ab2912035e160d9cad6306df307576da6128ebdb","channel":"stable","version":"3.50.0","dart_sdk_version":"3.14.0","dart_sdk_arch":"x64","release_date":"2026-11-11T19:05:47.758707Z","archive":"stable/linux/flutter_linux_3.50.0-stable.tar.xz","sha256":"6ea2d806fd57269ae783e6788e2a81fc26eaad782f1e0d0ff38623bb765ff9f5"},{"hash":"c197ca5199e9ee5661e865596c9c07294dfab51c","channel":"stable","version":"3.47.9","dart_sdk_version":"3.13.7","dart_sdk_arch":"x64","release_date":"2026-10-29T19:22:44.517205Z","archive":"stable/linux/flutter_linux_3.47.9-stable.tar.xz","sha256":"25ed8c3c29d51a7353f2a944310b478af8214920639524b19c50a19e6c044c5a"},{"hash":"d040ef5c5c4c8be0948207fadd35e87305920146","channel":"beta","version":"3.51.0","dart_sdk_version":"3.15.0","dart_sdk_arch":"x64","release_date":"2026-11-12T19:00:00Z","archive":"beta/linux/flutter_linux_3.51.0-beta.tar.xz","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#;
    const COMPOSER_RELEASE_FIXTURE: &str = r#"{"tag_name":"2.11.2","prerelease":false,"draft":false,"published_at":"2026-11-10T09:24:59Z","html_url":"https://github.com/composer/composer/releases/tag/2.11.2","assets":[{"name":"composer.phar.asc","size":589,"digest":"sha256:9a471a170440cb453d7f13e808cd05753704e866baea082967311acee62ce507","browser_download_url":"https://github.com/composer/composer/releases/download/2.11.2/composer.phar.asc"},{"name":"composer.phar","size":3815650,"digest":"sha256:dd158c0afd41d15fac89e1b4e47b5d42911fe316c3687cd5edac98319b25d028","browser_download_url":"https://github.com/composer/composer/releases/download/2.11.2/composer.phar"}]}"#;
    const ZIG_INDEX_FIXTURE: &str = r#"{"master":{"version":"0.18.0-dev.1","date":"2026-11-14","x86_64-linux":{"tarball":"https://ziglang.org/builds/zig-x86_64-linux-0.18.0-dev.1.tar.xz","shasum":"1111111111111111111111111111111111111111111111111111111111111111","size":"58000000"}},"0.17.1":{"version":"0.17.1","date":"2026-10-02","x86_64-linux":{"tarball":"https://ziglang.org/download/0.17.1/zig-x86_64-linux-0.17.1.tar.xz","shasum":"3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a","size":"57411036"},"aarch64-linux":{"tarball":"https://ziglang.org/download/0.17.1/zig-aarch64-linux-0.17.1.tar.xz","shasum":"958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f","size":"49471996"}}}"#;
    const KOTLIN_RELEASE_FIXTURE: &str = r#"{"tag_name":"v2.4.21","prerelease":false,"draft":false,"published_at":"2026-10-08T09:25:15Z","html_url":"https://github.com/JetBrains/kotlin/releases/tag/v2.4.21","assets":[{"name":"kotlin-compiler-2.4.21.zip","size":89733618,"digest":"sha256:7cc140e76daf416a0424a5557f99afd7ca89ebd463ea1101261e3e01c33e75cd","browser_download_url":"https://github.com/JetBrains/kotlin/releases/download/v2.4.21/kotlin-compiler-2.4.21.zip"},{"name":"kotlin-compiler-2.4.21.zip.sha256","size":64,"digest":"sha256:69012ce92719f8fa0524ca69954e2c36800edd76c8cd7f74965550e53650ac05","browser_download_url":"https://github.com/JetBrains/kotlin/releases/download/v2.4.21/kotlin-compiler-2.4.21.zip.sha256"}]}"#;
    const CMAKE_RELEASE_FIXTURE: &str = r#"{"tag_name":"v4.5.0","prerelease":false,"draft":false,"published_at":"2026-11-12T18:53:08Z","html_url":"https://github.com/Kitware/CMake/releases/tag/v4.5.0","assets":[{"name":"cmake-4.5.0-linux-x86_64.tar.gz","size":67002501,"digest":"sha256:81f0d5b0960fb6adc8197030ffc827fba937640d51e73e39e0e9e40d88c66f6f","browser_download_url":"https://github.com/Kitware/CMake/releases/download/v4.5.0/cmake-4.5.0-linux-x86_64.tar.gz"},{"name":"cmake-4.5.0-linux-aarch64.tar.gz","size":53719606,"digest":"sha256:0c8447e7bba9701467b5ac6155d157aa9e309e71af4981f78bd4b9176107f553","browser_download_url":"https://github.com/Kitware/CMake/releases/download/v4.5.0/cmake-4.5.0-linux-aarch64.tar.gz"},{"name":"cmake-4.5.0-SHA-256.txt","size":2015,"digest":"sha256:32f7aaffd4f866c926835dbb95a8b08fd4c2f5a0b5899e97b2f29f6be3c0b020","browser_download_url":"https://github.com/Kitware/CMake/releases/download/v4.5.0/cmake-4.5.0-SHA-256.txt"}]}"#;
    const GRADLE_VERSIONS_FIXTURE: &str = r#"[{"version":"9.9.1","buildTime":"20261110162215+0000","snapshot":false,"nightly":false,"releaseNightly":false,"activeRc":false,"rcFor":"","milestoneFor":"","broken":false,"downloadUrl":"https://services.gradle.org/distributions/gradle-9.9.1-bin.zip","checksumUrl":"https://services.gradle.org/distributions/gradle-9.9.1-bin.zip.sha256","checksum":"d8a18a7a841dd5df825d35a685fad6f2d7f12ac0c280e57b67c27b4a520ba228","released":true,"final":true},{"version":"9.9.0","buildTime":"20261028120000+0000","snapshot":false,"nightly":false,"releaseNightly":false,"activeRc":false,"rcFor":"","milestoneFor":"","broken":false,"downloadUrl":"https://services.gradle.org/distributions/gradle-9.9.0-bin.zip","checksumUrl":"https://services.gradle.org/distributions/gradle-9.9.0-bin.zip.sha256","checksum":"2222222222222222222222222222222222222222222222222222222222222222","released":true,"final":true},{"version":"9.10.0-rc1","buildTime":"20261113120000+0000","snapshot":false,"nightly":false,"releaseNightly":false,"activeRc":true,"rcFor":"9.10","milestoneFor":"","broken":false,"downloadUrl":"https://services.gradle.org/distributions/gradle-9.10.0-rc1-bin.zip","checksumUrl":"https://services.gradle.org/distributions/gradle-9.10.0-rc1-bin.zip.sha256","checksum":"3333333333333333333333333333333333333333333333333333333333333333","released":true,"final":false},{"version":"9.8.2","buildTime":"20260901000000+0000","snapshot":true,"nightly":false,"releaseNightly":false,"activeRc":false,"rcFor":"","milestoneFor":"","broken":false,"downloadUrl":"https://services.gradle.org/distributions/gradle-9.8.2-bin.zip","checksumUrl":"https://services.gradle.org/distributions/gradle-9.8.2-bin.zip.sha256","checksum":"4444444444444444444444444444444444444444444444444444444444444444","released":false,"final":true}]"#;
    const MAVEN_LISTING_FIXTURE: &str = r#"<html><body><table><tr><td><a href="../">../</a></td></tr><tr><td><a href="3.10.1/">3.10.1/</a></td><td>2026-11-05 12:00</td><td>0</td></tr><tr><td><a href="3.10.0/">3.10.0/</a></td><td>2026-10-01 12:00</td><td>0</td></tr><tr><td><a href="3.9.16/">3.9.16/</a></td><td>2026-09-01 12:00</td><td>0</td></tr><tr><td><a href="README.txt">README.txt</a></td></tr></table></body></html>"#;

    fn providers_config() -> Config {
        serde_json::from_str(r#"{"name": "RAST official catalog", "publisher": {"id": "rast", "name": "RAST", "url": "https://github.com/Enricko/rast-packages"}, "repository": "https://github.com/Enricko/rast-packages", "keyId": "rast-official-2026-11", "publicKey": "keys/rast-official-2026-11.pub.pem", "minRastVersion": "0.5.0", "expiresDays": 30, "refreshBeforeDays": 10, "allowedHosts": ["github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com", "static.rust-lang.org", "builds.dotnet.microsoft.com"], "targets": {"linux-x86_64": {"runner": "ubuntu-24.04", "go": "amd64", "node": "x64", "python": "x86_64", "rustTriple": "x86_64-unknown-linux-gnu", "javaArch": "x64", "bunArch": "x64", "denoArch": "x86_64", "dotnetRid": "linux-x64", "clangArch": "X64", "flutterArch": "x64", "cmakeArch": "x86_64", "zigArch": "x86_64"}, "linux-aarch64": {"runner": "ubuntu-24.04-arm", "go": "arm64", "node": "arm64", "python": "aarch64", "rustTriple": "aarch64-unknown-linux-gnu", "javaArch": "aarch64", "bunArch": "aarch64", "denoArch": "aarch64", "dotnetRid": "linux-arm64", "clangArch": "ARM64", "flutterArch": "arm64", "cmakeArch": "aarch64", "zigArch": "aarch64"}}, "withheld": [], "revisions": {}, "packages": {"rust": {"name": "Rust", "description": "Rust toolchain", "homepage": "https://www.rust-lang.org", "license": "MIT OR Apache-2.0", "keepPatches": 1, "channels": [{"id": "stable", "label": "Rust stable", "status": "stable"}]}, "java": {"name": "Eclipse Temurin JDK", "description": "Temurin JDK", "homepage": "https://adoptium.net", "license": "GPL-2.0 WITH Classpath-exception-2.0", "keepPatches": 1, "channels": [{"id": "25", "label": "Temurin 25", "status": "LTS"}]}, "bun": {"name": "Bun", "description": "Bun runtime", "homepage": "https://bun.com", "license": "MIT", "keepPatches": 2, "channels": [{"id": "stable", "label": "Bun stable", "status": "stable"}]}, "deno": {"name": "Deno", "description": "Deno runtime", "homepage": "https://deno.com", "license": "MIT", "keepPatches": 2, "githubAttestations": true, "channels": [{"id": "stable", "label": "Deno stable", "status": "stable"}]}, "dotnet": {"name": ".NET SDK", "description": ".NET SDK and runtime", "homepage": "https://dotnet.microsoft.com", "license": "MIT", "keepPatches": 1, "channels": [{"id": "10.0", "label": ".NET 10", "status": "LTS"}]}, "clang": {"name": "Clang/LLVM", "description": "LLVM C/C++ toolchain", "homepage": "https://llvm.org", "license": "Apache-2.0 WITH LLVM-exception", "keepPatches": 1, "githubAttestations": true, "channels": [{"id": "stable", "label": "LLVM stable", "status": "stable"}]}, "flutter": {"name": "Flutter", "description": "Flutter SDK and Dart", "homepage": "https://flutter.dev", "license": "BSD-3-Clause", "keepPatches": 2, "channels": [{"id": "stable", "label": "Flutter stable", "status": "stable"}]}, "composer": {"name": "Composer", "description": "Composer PHP dependency manager", "homepage": "https://getcomposer.org", "license": "MIT", "keepPatches": 2, "channels": [{"id": "stable", "label": "Composer stable", "status": "stable"}]}, "zig": {"name": "Zig", "description": "Zig compiler", "homepage": "https://ziglang.org", "license": "MIT", "keepPatches": 2, "channels": [{"id": "stable", "label": "Zig stable", "status": "stable"}]}, "kotlin": {"name": "Kotlin", "description": "Kotlin command-line compiler", "homepage": "https://kotlinlang.org", "license": "Apache-2.0", "keepPatches": 1, "channels": [{"id": "stable", "label": "Kotlin stable", "status": "stable"}]}, "cmake": {"name": "CMake", "description": "CMake build system", "homepage": "https://cmake.org", "license": "BSD-3-Clause", "keepPatches": 2, "channels": [{"id": "stable", "label": "CMake stable", "status": "stable"}]}, "gradle": {"name": "Gradle", "description": "Gradle build tool", "homepage": "https://gradle.org", "license": "Apache-2.0", "keepPatches": 2, "channels": [{"id": "stable", "label": "Gradle stable", "status": "stable"}]}, "maven": {"name": "Apache Maven", "description": "Maven build tool", "homepage": "https://maven.apache.org", "license": "Apache-2.0", "keepPatches": 2, "channels": [{"id": "3.10", "label": "Maven 3.10", "status": "stable"}]}}}"#).unwrap()
    }

    fn rust_doc() -> toml::Value {
        toml::from_str(RUST_CHANNEL_TOML_FIXTURE).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn ok_outcome(
        package: &str,
        version: &str,
        target: &str,
        sha256: &str,
        size: u64,
        format: &str,
        strip: u32,
        archives: Vec<ArchiveResult>,
    ) -> Outcome {
        Outcome {
            package: package.into(),
            version: version.into(),
            target: target.into(),
            ok: true,
            sha256: Some(sha256.into()),
            size: Some(size),
            format: Some(format.into()),
            strip_components: Some(strip),
            archives,
            verification: Some("verification recorded by the workflow".into()),
            ..Outcome::default()
        }
    }

    fn host_for(target: &str) -> crate::packages::host::HostInfo {
        crate::packages::host::HostInfo {
            os: "linux".into(),
            arch: target.trim_start_matches("linux-").into(),
            target: target.into(),
            libc: "glibc".into(),
            glibc_version: Some("2.39".into()),
            kernel: None,
            rast_version: "0.14.0".into(),
        }
    }

    #[test]
    fn rust_discovery_builds_a_multi_component_toolchain() {
        let cfg = providers_config();
        let package = package_config(&cfg, "rust").unwrap();
        let doc = rust_doc();
        let items =
            rust_plan_from_channel(&cfg, package, &doc, &"f".repeat(64), &BTreeSet::new()).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.version, "1.100.0");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.kind, "upstream");
        assert_eq!(item.release_date.as_deref(), Some("2026-11-12"));
        assert_eq!(item.targets.len(), 2, "both tier-1 Linux targets");
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(
            x86.url.as_deref().unwrap(),
            "https://static.rust-lang.org/dist/2026-11-12/rustc-1.100.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(x86.format.as_deref(), Some("tar.gz"));
        assert_eq!(x86.strip_components, Some(2));
        assert_eq!(
            x86.archives
                .iter()
                .map(|archive| archive.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["rust-std", "cargo", "rustfmt-preview", "clippy-preview"]
        );
        assert_eq!(
            x86.archives[0].sha256.as_deref(),
            Some("61eb09d68b338e230d698716927bd4623049c9910ef4a4972e66d763d66ed9e1")
        );
        assert_eq!(
            item.sums_url.as_deref(),
            Some("https://static.rust-lang.org/dist/channel-rust-stable.toml.sha256")
        );

        // withheld versions are skipped
        let held = BTreeSet::from(["rust@1.100.0".to_string()]);
        assert!(rust_plan_from_channel(&cfg, package, &doc, "f", &held)
            .unwrap()
            .is_empty());

        // A component missing for one target only drops that target.
        let mut incomplete = doc.clone();
        let clippy = incomplete
            .get_mut("pkg")
            .and_then(|pkgs| pkgs.get_mut("clippy-preview"))
            .and_then(|pkg| pkg.get_mut("target"))
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        clippy.remove("aarch64-unknown-linux-gnu");
        let items =
            rust_plan_from_channel(&cfg, package, &incomplete, "f", &BTreeSet::new()).unwrap();
        assert_eq!(items[0].targets.len(), 1);
        assert!(items[0].targets.contains_key("linux-x86_64"));
        assert!(!items[0].targets.contains_key("linux-aarch64"));
    }

    #[test]
    fn rust_component_metadata_is_cross_checked() {
        let doc = rust_doc();
        assert_eq!(
            rust_version("1.100.0 (0ef1bc386 2026-11-09)").as_deref(),
            Some("1.100.0")
        );
        assert!(rust_version("not a version").is_none());
        let hashes = rust_component_hashes(&doc, "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(hashes.len(), 5);
        assert_eq!(
            hashes["clippy-preview"],
            "277a9abd9378561d11fb67a9eb5c08895e207d07dfd985a67197aa47e030856f"
        );
        assert_eq!(
            rust_triple_from_file("rustc-1.100.0-x86_64-unknown-linux-gnu.tar.gz", "1.100.0")
                .unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert!(
            rust_triple_from_file("cargo-1.100.0-x86_64-unknown-linux-gnu.tar.gz", "1.100.0")
                .is_err()
        );
        assert!(rust_component_hashes(&doc, "x86_64-pc-windows-msvc").is_err());
    }

    fn rust_plan() -> (Config, PlanItem) {
        let cfg = providers_config();
        let package = package_config(&cfg, "rust").unwrap().clone();
        let doc = rust_doc();
        let items = rust_plan_from_channel(&cfg, &package, &doc, &"f".repeat(64), &BTreeSet::new())
            .unwrap();
        (cfg, items.into_iter().next().unwrap())
    }

    fn outcomes_for(item: &PlanItem) -> Vec<Outcome> {
        item.targets
            .iter()
            .map(|(target, info)| {
                let archives = info
                    .archives
                    .iter()
                    .map(|archive| ArchiveResult {
                        name: archive.name.clone(),
                        sha256: archive.sha256.clone().unwrap(),
                        size: 1000,
                        format: "tar.gz".into(),
                        strip_components: info.strip_components.unwrap_or(1),
                    })
                    .collect();
                // .NET publishes SHA-512 metadata; the manifest pin is the
                // SHA-256 of the downloaded bytes (simulated here).
                let pinned = info
                    .sha256
                    .clone()
                    .filter(|sha| sha.len() == 64)
                    .unwrap_or_else(|| "a".repeat(64));
                ok_outcome(
                    &item.package,
                    &item.version,
                    target,
                    &pinned,
                    4096,
                    info.format.as_deref().unwrap_or("tar.gz"),
                    info.strip_components.unwrap_or(1),
                    archives,
                )
            })
            .collect()
    }

    #[test]
    fn rust_manifest_merges_components_and_documents_coexistence() {
        let (cfg, item) = rust_plan();
        let (manifest, detail) = build_manifest(&cfg, &item, &outcomes_for(&item), None).unwrap();
        assert_eq!(detail, "");
        let manifest = manifest.expect("a verified target must produce a manifest");
        let artifact = manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["target"] == "linux-x86_64")
            .unwrap();
        assert_eq!(artifact["format"], "tar.gz");
        assert_eq!(artifact["stripComponents"], 2);
        assert_eq!(artifact["archives"].as_array().unwrap().len(), 4);
        assert_eq!(artifact["archives"][0]["name"], "rust-std");
        assert_eq!(artifact["archives"][0]["stripComponents"], 2);
        assert_eq!(artifact["requires"]["glibc"], "2.17");
        assert_eq!(manifest["channel"], "stable");
        assert_eq!(manifest["runtime"]["pathDirs"][0], "bin");
        assert_eq!(manifest["runtime"]["env"]["RUSTC"], "{root}/bin/rustc");
        assert!(
            manifest["runtime"]["env"].get("CARGO_HOME").is_none(),
            "CARGO_HOME must stay per user"
        );
        let notes = manifest["notes"].as_str().unwrap();
        assert!(notes.contains("rustup is not involved"), "{notes}");
        assert!(notes.contains("rust-toolchain.toml"), "{notes}");
        assert!(notes.contains("CARGO_HOME"), "{notes}");
        // The generated manifest must be valid for RAST itself.
        let parsed = manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(parsed.version, "1.100.0");
        assert_eq!(parsed.artifacts[0].archives().len(), 5);
        assert_eq!(parsed.artifacts[0].total_size(), 4096 + 4 * 1000);

        // A result with fewer verified components than the plan is refused.
        let mut outcomes = outcomes_for(&item);
        outcomes[0].archives.pop();
        let err = build_manifest(&cfg, &item, &outcomes, None).unwrap_err();
        assert!(err.contains("component archives"), "{err}");
    }

    #[test]
    fn java_versions_are_normalised_per_feature_version() {
        assert_eq!(java_manifest_version("25.0.5+8.0.LTS").unwrap(), "25.0.5+8");
        assert_eq!(
            java_manifest_version("21.0.13+9.0.LTS").unwrap(),
            "21.0.13+9"
        );
        assert_eq!(java_manifest_version("8.0.512+9").unwrap(), "8.0.512+9");
        assert_eq!(java_manifest_version("27.0.1+0").unwrap(), "27.0.1+0");
        assert!(java_manifest_version("LTS").is_err());
        assert_eq!(java_display_version("25.0.5+8"), "25.0.5");
        assert_eq!(java_display_version("8.0.512+9"), "1.8.0_512");
        assert_eq!(java_display_version("21.0.13+9"), "21.0.13");

        // JDK 8 has no jshell and speaks the legacy single-dash version flag.
        let legacy = runtime_spec("java", "8.0.512+9", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert!(legacy.executables.iter().all(|exe| exe.name != "jshell"));
        assert_eq!(legacy.checks[0].args, vec!["-version".to_string()]);
        assert_eq!(legacy.checks[0].expect, "1.8.0_512");
        assert_eq!(legacy.env["JAVA_HOME"], "{root}");

        let modern = runtime_spec("java", "25.0.5+8", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            modern
                .executables
                .iter()
                .map(|exe| exe.name.as_str())
                .collect::<Vec<_>>(),
            vec!["java", "javac", "jar", "jshell"]
        );
        assert_eq!(modern.checks.len(), 4);
        assert_eq!(modern.checks[0].args, vec!["--version".to_string()]);
        assert_eq!(modern.checks[1].expect, "javac 25.0.5");
        assert_eq!(modern.checks[2].expect, "jar 25.0.5");
        assert_eq!(modern.checks[3].expect, "jshell 25.0.5");
    }

    fn adoptium_plan(fixture: &str, arch: &str, target: &str) -> JavaAssetPlan {
        let assets: Value = serde_json::from_str(fixture).unwrap();
        let asset = adoptium_asset(assets.as_array().unwrap(), arch).unwrap();
        JavaAssetPlan {
            target: target.into(),
            digest: asset.checksum.clone(),
            size: asset.size,
            html_url: asset.release_link.clone(),
            asset,
        }
    }

    #[test]
    fn java_discovery_uses_adoptium_assets_and_github_digests() {
        let cfg = providers_config();
        let package = package_config(&cfg, "java").unwrap().clone();
        let found = vec![
            adoptium_plan(ADOPTIUM_25_X64, "x64", "linux-x86_64"),
            adoptium_plan(ADOPTIUM_25_ARM64, "aarch64", "linux-aarch64"),
        ];
        assert_eq!(
            found[0].asset.checksum,
            "68982529869257d0ee6b824ec5aa270ff8b118bb438670da0b97ebdab178d7fb"
        );
        assert_eq!(found[0].asset.release_name, "jdk-25.0.5+8");
        let item = java_plan_from_assets(&package, "25", &found, &BTreeSet::new())
            .unwrap()
            .expect("both targets have assets");
        assert_eq!(item.version, "25.0.5+8");
        assert_eq!(item.channel, "25");
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.gz"));
        assert_eq!(x86.strip_components, Some(1));
        assert!(x86.sums_url.as_deref().unwrap().ends_with(".sha256.txt"));
        assert_eq!(
            item.source.as_ref().unwrap()["temurinVersion"],
            "25.0.5+8-LTS"
        );

        // Different upstream versions per target: only the matching target.
        let mut mismatched = found.clone();
        mismatched[1].asset.semver = "25.0.6+1.0.LTS".into();
        let item = java_plan_from_assets(&package, "25", &mismatched, &BTreeSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(item.targets.len(), 1);
        assert!(item.targets.contains_key("linux-x86_64"));

        // withheld
        let held = BTreeSet::from(["java@25.0.5+8".to_string()]);
        assert!(java_plan_from_assets(&package, "25", &found, &held)
            .unwrap()
            .is_none());

        let (manifest, _) = build_manifest(&cfg, &item, &outcomes_for(&item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["runtime"]["env"]["JAVA_HOME"], "{root}");
        assert_eq!(manifest["runtime"]["pathDirs"][0], "bin");
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.17");
        assert!(manifest["notes"].as_str().unwrap().contains("JAVA_HOME"));
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn adoptium_assets_must_match_arch_and_os() {
        let assets: Value = serde_json::from_str(ADOPTIUM_25_X64).unwrap();
        assert!(adoptium_asset(assets.as_array().unwrap(), "aarch64").is_err());
        let available: Value = serde_json::from_str(ADOPTIUM_AVAILABLE_FIXTURE).unwrap();
        let versions: Vec<u64> = available["available_releases"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        assert!(versions.contains(&8) && versions.contains(&25) && versions.contains(&27));
        assert_eq!(available["most_recent_lts"], 25);
    }

    fn release_fixture(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn clang_discovery_uses_llvm_release_binaries_and_attestation() {
        let cfg = providers_config();
        let package = package_config(&cfg, "clang").unwrap().clone();
        assert!(package.github_attestations);
        let items = clang_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(CLANG_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.version, "23.1.5");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.kind, "upstream");
        assert!(item.attestations);
        assert_eq!(item.release_date.as_deref(), Some("2026-11-03"));
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.xz"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("f72edb0b36580ece7f52e23858f00c18fad4e87395ea7ad05d7e55c39f8fc78a")
        );
        assert_eq!(x86.digest, x86.sha256);
        assert_eq!(x86.size, Some(2_014_093_084));
        assert!(x86.sums_url.as_deref().unwrap().ends_with(".tar.xz.jsonl"));
        assert_eq!(
            item.targets["linux-aarch64"].file.as_deref(),
            Some("LLVM-23.1.5-Linux-ARM64.tar.xz")
        );

        // The version check commands and manifest shape.
        let spec = runtime_spec("clang", "23.1.5", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            spec.executables
                .iter()
                .map(|exe| exe.name.as_str())
                .collect::<Vec<_>>(),
            vec!["clang", "clang++", "ld.lld", "lldb"]
        );
        assert_eq!(spec.checks.len(), 3);
        assert_eq!(spec.checks[0].expect, "clang version 23.1.5");
        assert_eq!(spec.checks[2].expect, "LLD 23.1.5");
        assert!(spec.checks.iter().all(|check| check.executable != "lldb"));
        assert_eq!(spec.env["LD_LIBRARY_PATH"], "{root}/lib");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["channel"], "stable");
        assert_eq!(manifest["artifacts"][0]["format"], "tar.xz");
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.34");
        assert_eq!(manifest["runtime"]["env"]["LD_LIBRARY_PATH"], "{root}/lib");
        assert_eq!(manifest["artifacts"][0]["provenance"]["kind"], "upstream");
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();

        // Withheld and pre-release releases are not planned.
        let held = BTreeSet::from(["clang@23.1.5".to_string()]);
        assert!(clang_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(CLANG_RELEASE_FIXTURE)],
            &held
        )
        .unwrap()
        .is_empty());
        let mut prerelease = release_fixture(CLANG_RELEASE_FIXTURE);
        prerelease["prerelease"] = Value::Bool(true);
        assert!(
            clang_plan_from_releases(&cfg, &package, &[prerelease], &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn llvm_attestation_bundle_must_name_the_artifact_digest() {
        let digest = "f72edb0b36580ece7f52e23858f00c18fad4e87395ea7ad05d7e55c39f8fc78a";
        let statement = json!({
            "predicateType": "https://slsa.dev/provenance/v1",
            "subject": [
                {"name": "LLVM-23.1.5-Linux-X64.tar.xz", "digest": {"sha256": digest}},
                {"name": "LLVM-23.1.5-Linux-X64.tar.zst", "digest": {"sha256": "0000"}}
            ]
        });
        let payload = B64.encode(serde_json::to_vec(&statement).unwrap());
        let bundle = json!({
            "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
            "dsseEnvelope": {"payloadType": "application/vnd.in-toto+json", "payload": payload}
        });
        let good = format!("{}\n", serde_json::to_string(&bundle).unwrap());
        check_llvm_attestation_bundle(&good, digest).expect("matching digest");

        let wrong = json!({
            "predicateType": "https://slsa.dev/provenance/v1",
            "subject": [{"digest": {"sha256": "ab".repeat(32).to_string()}}]
        });
        let payload = B64.encode(serde_json::to_vec(&wrong).unwrap());
        let bundle = json!({"dsseEnvelope": {"payload": payload}});
        let bad = format!("{}\nnot json\n", serde_json::to_string(&bundle).unwrap());
        assert!(check_llvm_attestation_bundle(&bad, digest).is_err());
        // A different predicate is not LLVM's build provenance.
        let slsa_wrong = json!({
            "predicateType": "https://in-toto.io/attestation/release/v0.2",
            "subject": [{"digest": {"sha256": digest}}]
        });
        let payload = B64.encode(serde_json::to_vec(&slsa_wrong).unwrap());
        let bundle = json!({"dsseEnvelope": {"payload": payload}});
        let text = format!("{}\n", serde_json::to_string(&bundle).unwrap());
        assert!(check_llvm_attestation_bundle(&text, digest).is_err());
    }

    #[test]
    fn flutter_discovery_uses_google_metadata_and_skips_aarch64() {
        let cfg = providers_config();
        let package = package_config(&cfg, "flutter").unwrap().clone();
        let document = release_fixture(FLUTTER_RELEASES_FIXTURE);
        let items =
            flutter_plan_from_releases(&cfg, &package, &document, &BTreeSet::new()).unwrap();
        assert_eq!(
            items.len(),
            2,
            "only the newest stable releases are planned"
        );
        assert_eq!(items[1].version, "3.47.9");
        let item = &items[0];
        assert_eq!(item.version, "3.50.0");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.release_date.as_deref(), Some("2026-11-11"));
        assert_eq!(item.source.as_ref().unwrap()["dart"], "3.14.0");
        assert_eq!(
            item.targets.keys().cloned().collect::<Vec<_>>(),
            vec!["linux-x86_64"]
        );
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.xz"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("6ea2d806fd57269ae783e6788e2a81fc26eaad782f1e0d0ff38623bb765ff9f5")
        );
        assert_eq!(
            x86.file.as_deref(),
            Some("flutter_linux_3.50.0-stable.tar.xz")
        );
        assert_eq!(
            x86.url.as_deref(),
            Some("https://storage.googleapis.com/flutter_infra_release/releases/stable/linux/flutter_linux_3.50.0-stable.tar.xz")
        );

        let bundled = BTreeMap::from([("dart".to_string(), "3.14.0".to_string())]);
        let spec = runtime_spec("flutter", "3.50.0", &bundled, &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "bin/flutter");
        assert_eq!(spec.executables[1].path, "bin/dart");
        assert_eq!(spec.checks[0].expect, "Flutter 3.50.0");
        assert_eq!(spec.checks[1].expect, "Dart SDK version: 3.14.0");

        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.31");
        let notes = manifest["notes"].as_str().unwrap();
        assert!(notes.contains("Not available for linux-aarch64"), "{notes}");
        assert!(notes.contains("x86_64 only"), "{notes}");
        let parsed = manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        let arm = crate::packages::host::incompatibility(&parsed, &host_for("linux-aarch64"))
            .expect("aarch64 is not published");
        assert!(arm.contains("No build for this server"), "{arm}");
        assert!(
            crate::packages::host::incompatibility(&parsed, &host_for("linux-x86_64")).is_none()
        );

        assert_eq!(
            dart_sdk_version("Dart SDK version: 3.14.0 (stable) (Tue) on \"linux_x64\""),
            Some("3.14.0".to_string())
        );
        assert_eq!(dart_sdk_version("no version here"), None);
    }

    #[test]
    fn composer_discovery_uses_the_github_phar_and_getcomposer_sha256() {
        let cfg = providers_config();
        let package = package_config(&cfg, "composer").unwrap().clone();
        let items = composer_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(COMPOSER_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        let item = &items[0];
        assert_eq!(item.version, "2.11.2");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.targets.len(), 2);
        for info in item.targets.values() {
            assert_eq!(info.format.as_deref(), Some("phar"));
            assert_eq!(info.strip_components, Some(0));
            assert_eq!(
                info.sha256.as_deref(),
                Some("dd158c0afd41d15fac89e1b4e47b5d42911fe316c3687cd5edac98319b25d028")
            );
            assert_eq!(
                info.sums_url.as_deref(),
                Some("https://getcomposer.org/download/2.11.2/composer.phar.sha256sum")
            );
            assert_eq!(info.size, Some(3_815_650));
        }
        let spec = runtime_spec("composer", "2.11.2", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].name, "composer");
        assert_eq!(spec.executables[0].path, "bin/composer");
        assert_eq!(spec.checks[0].expect, "Composer version 2.11.2");
        assert_eq!(phar_path_for("composer"), Some("composer.phar"));

        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["format"], "phar");
        assert_eq!(manifest["artifacts"][0]["phar"]["path"], "composer.phar");
        assert_eq!(manifest["artifacts"][0]["provenance"]["kind"], "upstream");
        let parsed = manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(
            parsed.artifacts[0].phar.as_ref().unwrap().path,
            "composer.phar"
        );
        // The phar's wrapper path is generated by the installer, so the
        // manifest only lists bin/composer.
        assert_eq!(parsed.runtime.executables[0].path, "bin/composer");
    }

    fn ai_agent_config() -> Config {
        let mut cfg = providers_config();
        let package: PackageConfig = serde_json::from_value(json!({
            "name": "AI Agent", "description": "Chat with AI agents", "homepage": "https://github.com/Enricko/rast",
            "license": "MIT", "keepPatches": 2, "githubAttestations": true, "minRastVersion": "0.9.17",
            "app": {
                "id": "agent",
                "window": {"entry": "ui/app.js", "styles": ["ui/app.css"], "sdk": 1, "singleton": true, "defaultSize": {"w": 1080, "h": 720}},
                "server": {"executable": "bin/rast-ai-agent", "hostApi": 1, "apiPrefix": "/api/ai", "eventPrefix": "ai."}
            },
            "channels": [{"id": "stable", "label": "AI Agent", "status": "stable"}]
        }))
        .unwrap();
        cfg.packages.entries.push(("ai-agent".into(), package));
        cfg
    }

    fn ai_agent_release(tag: &str, version: &str) -> Value {
        let asset = |name: &str, digest: &str| {
            json!({
                "name": name, "size": 4096, "digest": format!("sha256:{digest}"),
                "browser_download_url": format!("https://github.com/Enricko/rast/releases/download/{tag}/{name}"),
            })
        };
        json!({
            "tag_name": tag, "prerelease": false, "draft": false,
            "published_at": "2026-11-28T10:00:00Z",
            "html_url": format!("https://github.com/Enricko/rast/releases/tag/{tag}"),
            "assets": [
                asset(&format!("ai-agent-{version}-linux-x86_64.tar.gz"), &"a".repeat(64)),
                asset(&format!("ai-agent-{version}-linux-aarch64.tar.gz"), &"b".repeat(64)),
                asset("SHA256SUMS", &"c".repeat(64)),
            ],
        })
    }

    #[test]
    fn ai_agent_is_an_app_package_from_rast_releases() {
        let cfg = ai_agent_config();
        let package = package_config(&cfg, "ai-agent").unwrap().clone();
        let releases = [
            ai_agent_release("ai-agent-v1.0.0", "1.0.0"),
            // RAST's own releases are not AI Agent's.
            ai_agent_release("v0.9.17", "0.9.17"),
            ai_agent_release("ai-agent-v1.1.0-rc.1", "1.1.0-rc.1"),
        ];
        let items =
            ai_agent_plan_from_releases(&cfg, &package, &releases, &BTreeSet::new()).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.version, "1.0.0");
        assert_eq!(item.channel, "stable");
        assert!(item.attestations);
        assert_eq!(item.source.as_ref().unwrap()["app"]["id"], "agent");
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.sha256.as_deref(), Some("a".repeat(64).as_str()));
        assert!(x86.sums_url.as_deref().unwrap().ends_with("/SHA256SUMS"));
        assert_eq!(x86.strip_components, Some(1));

        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["type"], "app");
        assert_eq!(manifest["minRastVersion"], "0.9.17");
        assert!(manifest.get("runtime").is_none());
        assert_eq!(manifest["app"]["server"]["apiPrefix"], "/api/ai");
        assert_eq!(manifest["artifacts"][0]["provenance"]["kind"], "built");
        let parsed = manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        manifest::validate(&parsed).unwrap();
        let entries = package_entries(&cfg).unwrap();
        let entry = &entries.iter().find(|(id, _)| id == "ai-agent").unwrap().1;
        assert_eq!(entry["type"], "app");
        assert_eq!(
            entries.iter().find(|(id, _)| id == "bun").unwrap().1["type"],
            "runtime"
        );
    }

    #[test]
    fn ai_agent_releases_come_from_its_repositories_newest_home_first() {
        let mut cfg = ai_agent_config();
        // Older catalogs' config: RAST's repository.
        let package = package_config(&cfg, "ai-agent").unwrap().clone();
        assert_eq!(ai_agent_repos(&package), ["Enricko/rast"]);
        let entry = cfg
            .packages
            .entries
            .iter_mut()
            .find(|(id, _)| id == "ai-agent")
            .unwrap();
        entry.1.release_repos = vec!["Enricko/rast-packages".into(), "Enricko/rast".into()];
        let package = package_config(&cfg, "ai-agent").unwrap().clone();
        assert_eq!(
            ai_agent_repos(&package),
            ["Enricko/rast-packages", "Enricko/rast"]
        );
        let moved = |tag: &str, version: &str| {
            let text = ai_agent_release(tag, version)
                .to_string()
                .replace("Enricko/rast/", "Enricko/rast-packages/");
            serde_json::from_str::<Value>(&text).unwrap()
        };
        // 1.3.0 in both: the package repository's is taken.
        let releases = [
            moved("ai-agent-v1.4.0", "1.4.0"),
            moved("ai-agent-v1.3.0", "1.3.0"),
            ai_agent_release("ai-agent-v1.3.0", "1.3.0"),
            ai_agent_release("ai-agent-v1.2.0", "1.2.0"),
        ];
        let items =
            ai_agent_plan_from_releases(&cfg, &package, &releases, &BTreeSet::new()).unwrap();
        let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
        assert_eq!(versions, ["1.4.0", "1.3.0"]);
        let source = items[1].targets["linux-x86_64"].source.as_deref().unwrap();
        assert_eq!(
            github_repo_of(source).as_deref(),
            Some("Enricko/rast-packages")
        );
        assert_eq!(
            github_repo_of("https://github.com/Enricko/rast/releases/tag/ai-agent-v1.2.0")
                .as_deref(),
            Some("Enricko/rast")
        );
        for url in [
            "https://github.com/Enricko/rast",
            "https://github.com/Enricko/rast/tree/main",
            "https://example.com/Enricko/rast/releases/tag/x",
            "https://github.com/../rast/releases/tag/x",
        ] {
            assert_eq!(github_repo_of(url), None, "{url}");
        }
    }

    #[test]
    fn app_archives_are_checked_for_their_files_and_version() {
        use std::os::unix::fs::PermissionsExt;
        let cfg = ai_agent_config();
        let app = package_config(&cfg, "ai-agent")
            .unwrap()
            .app
            .clone()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("ui")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let program = root.join("bin/rast-ai-agent");
        std::fs::write(
            &program,
            "#!/bin/sh\necho 'rast-ai-agent 1.0.0 (RAST 0.9.17)'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("ui/app.js"), "export default {};").unwrap();
        let missing = check_app_files(&root, &app, "1.0.0", &home).unwrap_err();
        assert!(missing.contains("ui/app.css"), "{missing}");
        std::fs::write(root.join("ui/app.css"), "").unwrap();
        let checks = check_app_files(&root, &app, "1.0.0", &home).unwrap();
        assert_eq!(
            checks,
            vec!["rast-ai-agent 1.0.0 (RAST 0.9.17)".to_string()]
        );
        let wrong = check_app_files(&root, &app, "1.0.1", &home).unwrap_err();
        assert!(wrong.contains("expected"), "{wrong}");
    }

    #[test]
    fn zig_discovery_uses_the_official_download_index() {
        let cfg = providers_config();
        let package = package_config(&cfg, "zig").unwrap().clone();
        let document = release_fixture(ZIG_INDEX_FIXTURE);
        let items = zig_plan_from_index(&cfg, &package, &document, &BTreeSet::new()).unwrap();
        assert_eq!(items.len(), 1, "master/development builds are not planned");
        let item = &items[0];
        assert_eq!(item.version, "0.17.1");
        assert_eq!(item.release_date.as_deref(), Some("2026-10-02"));
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.xz"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(x86.size, Some(57_411_036));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a")
        );
        let spec = runtime_spec("zig", "0.17.1", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "zig");
        assert_eq!(spec.path_dirs, vec![".".to_string()]);
        assert_eq!(spec.checks[0].args, vec!["version".to_string()]);
        assert_eq!(spec.checks[0].expect, "0.17.1");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["format"], "tar.xz");
        assert_eq!(manifest["runtime"]["pathDirs"][0], ".");
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn kotlin_discovery_uses_github_zip_and_sha256_file() {
        let cfg = providers_config();
        let package = package_config(&cfg, "kotlin").unwrap().clone();
        let items = kotlin_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(KOTLIN_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        let item = &items[0];
        assert_eq!(item.version, "2.4.21", "the v prefix is normalised away");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.targets.len(), 2);
        assert_eq!(
            item.targets["linux-aarch64"].url, item.targets["linux-x86_64"].url,
            "the compiler zip is architecture independent"
        );
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("zip"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("7cc140e76daf416a0424a5557f99afd7ca89ebd463ea1101261e3e01c33e75cd")
        );
        assert!(x86.sums_url.as_deref().unwrap().ends_with(".zip.sha256"));
        let spec = runtime_spec("kotlin", "2.4.21", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            spec.executables
                .iter()
                .map(|exe| exe.path.as_str())
                .collect::<Vec<_>>(),
            vec!["bin/kotlinc", "bin/kotlin"]
        );
        assert_eq!(spec.checks[0].expect, "2.4.21");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        manifest::parse(&serde_json::to_vec(&manifest.unwrap()).unwrap()).unwrap();

        // Pre-release tags are filtered before version normalisation.
        let mut prerelease = release_fixture(KOTLIN_RELEASE_FIXTURE);
        prerelease["prerelease"] = Value::Bool(true);
        assert!(
            kotlin_plan_from_releases(&cfg, &package, &[prerelease], &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cmake_discovery_uses_linux_archives_and_the_sha256_file() {
        let cfg = providers_config();
        let package = package_config(&cfg, "cmake").unwrap().clone();
        let items = cmake_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(CMAKE_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        let item = &items[0];
        assert_eq!(item.version, "4.5.0");
        assert_eq!(item.targets.len(), 2);
        assert_eq!(
            item.targets["linux-x86_64"].file.as_deref(),
            Some("cmake-4.5.0-linux-x86_64.tar.gz")
        );
        assert_eq!(
            item.targets["linux-aarch64"].file.as_deref(),
            Some("cmake-4.5.0-linux-aarch64.tar.gz")
        );
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.gz"));
        assert!(x86.sums_url.as_deref().unwrap().ends_with("SHA-256.txt"));
        let spec = runtime_spec("cmake", "4.5.0", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            spec.executables
                .iter()
                .map(|exe| exe.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cmake", "ctest", "cpack"]
        );
        assert_eq!(spec.checks[0].expect, "cmake version 4.5.0");
        assert_eq!(spec.checks[1].expect, "ctest version 4.5.0");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.31");
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn gradle_discovery_uses_version_metadata_and_filters_unreleased_versions() {
        let cfg = providers_config();
        let package = package_config(&cfg, "gradle").unwrap().clone();
        let document = release_fixture(GRADLE_VERSIONS_FIXTURE);
        let items = gradle_plan_from_versions(&cfg, &package, &document, &BTreeSet::new()).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.version.as_str())
                .collect::<Vec<_>>(),
            vec!["9.9.1", "9.9.0"],
            "RC/snapshot/broken versions are filtered"
        );
        let item = &items[0];
        assert_eq!(item.release_date.as_deref(), Some("2026-11-10"));
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("zip"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("d8a18a7a841dd5df825d35a685fad6f2d7f12ac0c280e57b67c27b4a520ba228")
        );
        assert!(x86
            .url
            .as_deref()
            .unwrap()
            .ends_with("gradle-9.9.1-bin.zip"));
        let spec = runtime_spec("gradle", "9.9.1", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "bin/gradle");
        assert_eq!(spec.checks[0].expect, "Gradle 9.9.1");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        manifest::parse(&serde_json::to_vec(&manifest.unwrap()).unwrap()).unwrap();
    }

    #[test]
    fn maven_discovery_parses_the_apache_archive_listing() {
        let cfg = providers_config();
        let package = package_config(&cfg, "maven").unwrap().clone();
        assert_eq!(
            maven_versions_from_listing(MAVEN_LISTING_FIXTURE),
            vec![
                "3.10.0".to_string(),
                "3.10.1".to_string(),
                "3.9.16".to_string()
            ]
        );
        let items =
            maven_plan_from_listing(&cfg, &package, MAVEN_LISTING_FIXTURE, &BTreeSet::new())
                .unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.version.as_str())
                .collect::<Vec<_>>(),
            vec!["3.10.1", "3.10.0"]
        );
        let item = &items[0];
        assert_eq!(item.channel, "3.10");
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.gz"));
        assert_eq!(x86.strip_components, Some(1));
        assert!(x86
            .url
            .as_deref()
            .unwrap()
            .ends_with("maven-3/3.10.1/binaries/apache-maven-3.10.1-bin.tar.gz"));
        assert!(x86.sums_url.as_deref().unwrap().ends_with(".tar.gz.sha512"));
        assert!(
            x86.sha256.is_none(),
            "the SHA-512 is fetched at verify time"
        );
        let spec = runtime_spec("maven", "3.10.1", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "bin/mvn");
        assert_eq!(spec.checks[0].expect, "Apache Maven 3.10.1");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        manifest::parse(&serde_json::to_vec(&manifest.unwrap()).unwrap()).unwrap();
    }

    #[test]
    fn new_provider_target_architectures_are_configured() {
        let cfg = providers_config();
        let x86 = cfg.targets.get("linux-x86_64").unwrap();
        assert_eq!(x86.clang_arch("linux-x86_64").unwrap(), "X64");
        assert_eq!(x86.flutter_arch("linux-x86_64").unwrap(), "x64");
        assert_eq!(x86.cmake_arch("linux-x86_64").unwrap(), "x86_64");
        assert_eq!(x86.zig_arch("linux-x86_64").unwrap(), "x86_64");
        let arm = cfg.targets.get("linux-aarch64").unwrap();
        assert_eq!(arm.clang_arch("linux-aarch64").unwrap(), "ARM64");
        assert_eq!(arm.flutter_arch("linux-aarch64").unwrap(), "arm64");
        assert_eq!(arm.cmake_arch("linux-aarch64").unwrap(), "aarch64");
        assert_eq!(arm.zig_arch("linux-aarch64").unwrap(), "aarch64");
        assert!(unavailable_reason("flutter", "linux-aarch64")
            .unwrap()
            .contains("x86_64 only"));
        assert!(unavailable_reason("bun", "linux-aarch64").is_none());
    }

    #[test]
    fn bun_discovery_uses_github_releases_and_shasums() {
        let cfg = providers_config();
        let package = package_config(&cfg, "bun").unwrap().clone();
        let items = bun_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(BUN_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.version, "1.4.4");
        assert_eq!(item.channel, "stable");
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("zip"));
        assert_eq!(x86.strip_components, Some(1));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("bf8ccbfb2b0b80159e0f9a6d15c55317add8077534dbc6130a3edb52fccb395c")
        );
        assert!(item
            .sums_url
            .as_deref()
            .unwrap()
            .ends_with("/SHASUMS256.txt"));
        let spec = runtime_spec("bun", "1.4.4", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "bun");
        assert_eq!(spec.path_dirs, vec![".".to_string()]);
        assert_eq!(spec.checks[0].expect, "1.4.4");

        // Pre-release builds are not published.
        let mut prerelease = release_fixture(BUN_RELEASE_FIXTURE);
        prerelease["prerelease"] = Value::Bool(true);
        assert!(
            bun_plan_from_releases(&cfg, &package, &[prerelease], &BTreeSet::new())
                .unwrap()
                .is_empty()
        );

        // A release without the arm64 asset only publishes x86_64, and a
        // host without a build gets the explicit compatibility reason.
        let mut partial = release_fixture(BUN_RELEASE_FIXTURE);
        partial["assets"]
            .as_array_mut()
            .unwrap()
            .retain(|asset| asset["name"] != "bun-linux-aarch64.zip");
        let items = bun_plan_from_releases(&cfg, &package, &[partial], &BTreeSet::new()).unwrap();
        assert_eq!(items[0].targets.len(), 1);
        let (manifest, _) =
            build_manifest(&cfg, &items[0], &outcomes_for(&items[0]), None).unwrap();
        let manifest = manifest.expect("the x86_64 build still verifies");
        assert_eq!(manifest["artifacts"][0]["format"], "zip");
        assert_eq!(manifest["runtime"]["pathDirs"][0], ".");
        let parsed = manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        let reason = crate::packages::host::incompatibility(&parsed, &host_for("linux-aarch64"))
            .expect("aarch64 is not published");
        assert!(reason.contains("No build for this server"), "{reason}");
        assert!(reason.contains("linux-x86_64"), "{reason}");
        assert!(
            crate::packages::host::incompatibility(&parsed, &host_for("linux-x86_64")).is_none()
        );
    }

    #[test]
    fn collect_accepts_a_flat_plan_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(artifacts.join("plan.json"), b"{\"sequence\":1}").unwrap();
        std::fs::create_dir_all(artifacts.join("previous")).unwrap();
        std::fs::write(artifacts.join("previous/catalog.json"), b"{}").unwrap();
        let work = temp.path().join("work");
        cmd_collect(&artifacts, &work, "skipped").unwrap();
        assert!(work.join("plan.json").is_file());
        assert!(work.join("previous/catalog.json").is_file());
    }

    #[test]
    fn deno_attestation_document_must_name_the_release_predicate_and_digest() {
        let digest = "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609";
        let statement = json!({
            "predicateType": DENO_RELEASE_PREDICATE,
            "subject": [
                {"name": "deno-x86_64-unknown-linux-gnu.zip", "digest": {"sha256": digest}},
                {"name": "deno-aarch64-unknown-linux-gnu.zip", "digest": {"sha256": "0000"}}
            ]
        });
        let payload = B64.encode(serde_json::to_vec(&statement).unwrap());
        let good = json!({"attestations": [{"bundle": {"dsseEnvelope": {"payload": payload}}}]});
        check_deno_attestation_document(&good, digest).expect("matching digest");

        let slsa = json!({
            "predicateType": "https://slsa.dev/provenance/v1",
            "subject": [{"digest": {"sha256": digest}}]
        });
        let payload = B64.encode(serde_json::to_vec(&slsa).unwrap());
        let wrong_predicate =
            json!({"attestations": [{"bundle": {"dsseEnvelope": {"payload": payload}}}]});
        assert!(check_deno_attestation_document(&wrong_predicate, digest).is_err());
        assert!(check_deno_attestation_document(&json!({"attestations": []}), digest).is_err());
        assert!(encode_query_value(DENO_RELEASE_PREDICATE).starts_with("https%3A%2F%2Fin-toto.io"));
    }

    #[test]
    fn deno_discovery_uses_github_releases_sums_and_attestations() {
        let cfg = providers_config();
        let package = package_config(&cfg, "deno").unwrap().clone();
        assert!(package.github_attestations);
        let items = deno_plan_from_releases(
            &cfg,
            &package,
            &[release_fixture(DENO_RELEASE_FIXTURE)],
            &BTreeSet::new(),
        )
        .unwrap();
        let item = &items[0];
        assert_eq!(item.version, "2.9.8");
        assert_eq!(item.channel, "stable");
        assert!(item.attestations);
        assert_eq!(item.targets.len(), 2);
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("zip"));
        assert_eq!(x86.strip_components, Some(0));
        assert!(x86
            .sums_url
            .as_deref()
            .unwrap()
            .ends_with("deno-x86_64-unknown-linux-gnu.zip.sha256sum"));
        assert_eq!(
            x86.sha256.as_deref(),
            Some("e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609")
        );
        let spec = runtime_spec("deno", "2.9.8", &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "deno");
        assert_eq!(spec.path_dirs, vec![".".to_string()]);
        assert_eq!(spec.checks[0].expect, "deno 2.9.8");
        let (manifest, _) = build_manifest(&cfg, item, &outcomes_for(item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["format"], "zip");
        assert_eq!(manifest["artifacts"][0]["requires"]["glibc"], "2.27");
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn dotnet_discovery_uses_release_metadata_and_sha512() {
        let cfg = providers_config();
        let package = package_config(&cfg, "dotnet").unwrap().clone();
        let entry = release_fixture(DOTNET_INDEX_ENTRY_FIXTURE);
        let releases = release_fixture(DOTNET_RELEASES_FIXTURE);
        let item =
            dotnet_plan_from_release(&cfg, &package, "10.0", &entry, &releases, &BTreeSet::new())
                .unwrap()
                .expect("channel is active");
        assert_eq!(item.version, "10.0.404");
        assert_eq!(item.channel, "10.0");
        assert_eq!(item.release_date.as_deref(), Some("2026-11-10"));
        assert_eq!(item.source.as_ref().unwrap()["runtime"], "10.0.15");
        let x86 = &item.targets["linux-x86_64"];
        assert_eq!(x86.format.as_deref(), Some("tar.gz"));
        assert_eq!(x86.strip_components, Some(0));
        assert_eq!(
            x86.sha512.as_deref(),
            Some("577d0eb17fec33d029104c5c63a1ab0accc3e28776274abef2218dbac5af18d2109b5406d26b31d76f5c245215e3ba0926a94c20246c672c41847c9e1eb1dc50")
        );
        assert!(x86.sha256.is_none(), "the metadata hash is SHA-512");
        let bundled = BTreeMap::from([("runtime".to_string(), "10.0.15".to_string())]);
        let spec = runtime_spec("dotnet", "10.0.404", &bundled, &BTreeSet::new()).unwrap();
        assert_eq!(spec.executables[0].path, "dotnet");
        assert_eq!(spec.env["DOTNET_ROOT"], "{root}");
        assert_eq!(spec.path_dirs, vec![".".to_string()]);
        assert_eq!(spec.checks.len(), 3);
        assert_eq!(spec.checks[1].expect, "10.0.404");
        assert_eq!(spec.checks[2].expect, "Microsoft.NETCore.App 10.0.15");
        let (manifest, _) = build_manifest(&cfg, &item, &outcomes_for(&item), None).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest["artifacts"][0]["stripComponents"], 0);
        assert_eq!(manifest["runtime"]["env"]["DOTNET_ROOT"], "{root}");
        manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        // withheld
        let held = BTreeSet::from(["dotnet@10.0.404".to_string()]);
        assert!(
            dotnet_plan_from_release(&cfg, &package, "10.0", &entry, &releases, &held)
                .unwrap()
                .is_none()
        );
        // End-of-life channels are filtered by discovery from support-phase,
        // not by the pure plan builder; a shifted latest-sdk falls back to
        // the newest listed release instead of failing.
        let mut older = releases.clone();
        older["releases"][0]["sdk"]["version"] = json!("10.0.999");
        assert!(
            dotnet_plan_from_release(&cfg, &package, "10.0", &entry, &older, &BTreeSet::new())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn target_architecture_mappings_are_configured() {
        let cfg = providers_config();
        let x86 = cfg.targets.get("linux-x86_64").unwrap();
        assert_eq!(
            x86.rust_triple("linux-x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(x86.java_arch("linux-x86_64").unwrap(), "x64");
        assert_eq!(x86.bun_arch("linux-x86_64").unwrap(), "x64");
        assert_eq!(x86.deno_arch("linux-x86_64").unwrap(), "x86_64");
        assert_eq!(x86.dotnet_rid("linux-x86_64").unwrap(), "linux-x64");
        let arm = cfg.targets.get("linux-aarch64").unwrap();
        assert_eq!(arm.java_arch("linux-aarch64").unwrap(), "aarch64");
        assert_eq!(arm.dotnet_rid("linux-aarch64").unwrap(), "linux-arm64");
        // An old-style target block without the new mappings still parses,
        // but the provider reports exactly what the config is missing.
        let legacy: TargetConfig = serde_json::from_value(json!({
            "runner": "ubuntu-24.04", "go": "amd64", "node": "x64", "python": "x86_64"
        }))
        .unwrap();
        for error in [
            legacy.rust_triple("linux-x86_64").unwrap_err(),
            legacy.java_arch("linux-x86_64").unwrap_err(),
            legacy.dotnet_rid("linux-x86_64").unwrap_err(),
        ] {
            assert!(
                error.contains("catalog.config.json has no targets.linux-x86_64"),
                "{error}"
            );
        }
    }

    #[test]
    fn new_provider_channels_and_result_archives() {
        assert_eq!(channel_of("rust", "1.100.0"), Some("stable".into()));
        assert_eq!(channel_of("bun", "1.4.4"), Some("stable".into()));
        assert_eq!(channel_of("deno", "2.9.8"), Some("stable".into()));
        assert_eq!(channel_of("java", "25.0.5+8"), Some("25".into()));
        assert_eq!(channel_of("java", "8.0.512+9"), Some("8".into()));
        assert_eq!(channel_of("dotnet", "10.0.404"), Some("10.0".into()));
        for package in [
            "clang", "flutter", "composer", "zig", "kotlin", "cmake", "gradle",
        ] {
            assert_eq!(
                channel_of(package, "1.2.3"),
                Some("stable".to_string()),
                "{package}"
            );
        }
        assert_eq!(channel_of("maven", "3.10.1"), Some("3.10".to_string()));

        let value = json!({
            "package": "rust",
            "version": "1.100.0",
            "target": "linux-x86_64",
            "ok": true,
            "sha256": "a".repeat(64),
            "size": 4096,
            "format": "tar.gz",
            "stripComponents": 2,
            "archives": [
                {"name": "rust-std", "sha256": "b".repeat(64), "size": 2048,
                 "format": "tar.gz", "stripComponents": 2}
            ],
            "verification": "signed channel manifest",
            "checks": ["rustc 1.100.0"],
        });
        let outcome: Outcome = serde_json::from_value(value).unwrap();
        assert_eq!(outcome.format.as_deref(), Some("tar.gz"));
        assert_eq!(outcome.strip_components, Some(2));
        assert_eq!(outcome.archives.len(), 1);
        assert_eq!(outcome.archives[0].name.as_deref(), Some("rust-std"));
        // Results written before multi-archive support still parse.
        let old: Outcome = serde_json::from_value(json!({
            "package": "go", "version": "1.27.1", "target": "linux-x86_64",
            "ok": true, "sha256": "a".repeat(64), "size": 4096
        }))
        .unwrap();
        assert!(old.format.is_none());
        assert!(old.archives.is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "runs a shell script")]
    fn version_checks_read_stdout_and_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("bin")).unwrap();
        let javac = root.join("bin/javac");
        fs::write(&javac, "#!/bin/sh\necho 'javac 25.0.5' >&2\n").unwrap();
        fs::set_permissions(&javac, fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // javac prints its version on stderr; the check must see it anyway.
        run_check(
            &root,
            "bin/javac",
            &["-version".to_string()],
            "javac 25.0.5",
            &home,
            &[],
        )
        .unwrap();
        assert!(run_check(
            &root,
            "bin/javac",
            &["-version".to_string()],
            "javac 1.8.0_512",
            &home,
            &[]
        )
        .is_err());

        // Runtime env hints reach the checks with {root} resolved; the fixed
        // entries (PATH/HOME/TMPDIR) can never be replaced by a hint.
        let environment = check_environment(
            &root,
            &home,
            &[
                ("DEMO_HOME".to_string(), "{root}/share".to_string()),
                ("HOME".to_string(), "{root}".to_string()),
            ],
        );
        let lookup = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(
            lookup("DEMO_HOME"),
            Some(format!("{}/share", root.display()).as_str())
        );
        assert_eq!(lookup("HOME"), Some(home.display().to_string().as_str()));
    }
}
