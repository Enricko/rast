//! The RAST package manifest (`rast-package.json`, schema version 1).
//!
//! One manifest describes one package version: identity, publisher, upstream
//! version and package revision, the host targets it supports, the artifact
//! for each target (URL, SHA-256, size, archive format, provenance) and the
//! executables it provides as relative paths. The official catalog embeds the
//! same documents, so official and imported packages go through exactly the
//! same validation and installation operations.
//!
//! A manifest has no install scripts or hooks. The known operations are:
//! download the artifact for the host target, verify its size and SHA-256,
//! extract the archive (with path/link validation), check that every declared
//! executable exists inside the package directory, and run the declared
//! version checks. The checks do execute the package's own executables — for
//! an imported package that is third-party code — as the RAST service
//! account, confined by [`super::sandbox`] (own process group, no sockets,
//! Landlock-limited file access). Future package types (apps, services) add
//! their own typed sections and operations; `type` values this RAST does not
//! support are rejected with a clear message.
//!
//! An `app` package (official catalog only) is a desktop app: a window
//! module the desktop loads, and optionally a server program RAST runs and
//! forwards the app's API path to (see [`AppSpec`] and `crate::apps`).
//!
//! The JSON Schema for authors lives in `docs/rast-package.schema.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};

use super::version::{self, Version};

pub const SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_KIND: &str = "rast-package";
pub const MANIFEST_FILE: &str = "rast-package.json";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// Media types a package icon may use: an SVG document or a PNG image.
pub const ICON_MEDIA_TYPES: &[&str] = &["image/svg+xml", "image/png"];
/// Icons are embedded base64 in manifests and in the signed catalog; the
/// decoded image is bounded so a catalog stays small.
pub const MAX_ICON_BYTES: usize = 64 * 1024;
/// Maximum length of an icon's optional attribution text.
pub const MAX_ICON_ATTRIBUTION_CHARS: usize = 300;
/// The 8 bytes every PNG file starts with.
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Archive formats the installer can extract safely (and, for `phar`, write
/// as a verified single file).
pub const ARCHIVE_FORMATS: &[&str] = &["tar.gz", "tar.xz", "zip", "phar"];
/// Formats that are unpacked as archives. Every format except `phar` is
/// extractable; component archives of a multi-archive artifact must be one of
/// these.
pub const EXTRACTABLE_FORMATS: &[&str] = &["tar.gz", "tar.xz", "zip"];
/// Most archives one host target may need (Rust ships five components).
pub const MAX_ARCHIVES_PER_TARGET: usize = 8;
/// Host targets RAST knows about (`<os>-<arch>`).
pub const KNOWN_TARGETS: &[&str] = &["linux-x86_64", "linux-aarch64"];
/// Package types this RAST can install.
pub const SUPPORTED_TYPES: &[&str] = &["runtime", "app"];
/// Package types reserved for future releases (rejected with a specific note).
pub const PLANNED_TYPES: &[&str] = &["service"];
/// The host interface versions this RAST offers app packages: the server
/// API ([`AppServer`]) and the window SDK ([`AppWindow::sdk`]).
pub const HOST_API_VERSIONS: &[u32] = &[1];
pub const WINDOW_SDK_VERSIONS: &[u32] = &[1];
/// First path segments under `/api/` that belong to RAST itself; an app's
/// `apiPrefix` must be another one.
pub const HOST_API_SEGMENTS: &[&str] = &[
    "auth",
    "users",
    "groups",
    "files",
    "shares",
    "storage",
    "packages",
    "apps",
    "system",
    "settings",
    "desktop",
    "events",
    "jobs",
    "audit",
    "logs",
    "backup",
    "docker",
    "terminal",
    "network",
    "ws",
    "health",
    "update",
    "catalog",
    "access",
    "me",
    "notifications",
];
/// Ids that collide with API path segments (`/api/packages/<id>`).
pub const RESERVED_IDS: &[&str] = &["check", "consumers", "github", "host", "catalog", "bin"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Publisher {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Executable {
    /// Command name, e.g. `python3`, `php`, `go`, `node`.
    pub name: String,
    /// Path relative to the package root, e.g. `bin/python3`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionCheck {
    /// One of the declared executable names.
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Text that must appear in the command's output.
    pub expect: String,
    /// Optional label, e.g. `npm` for Node's bundled package manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bundled {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeSpec {
    pub executables: Vec<Executable>,
    /// The first check is the primary version check.
    pub checks: Vec<VersionCheck>,
    /// Tools shipped inside the artifact (npm, pip, …) as found in it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundled: Vec<Bundled>,
    /// Bundled extensions/modules (e.g. PHP extensions), informational.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// Environment hints for a shell that runs the runtime. Values may use
    /// the `{root}` placeholder, which consumers replace with the installed
    /// version prefix (e.g. `JAVA_HOME: "{root}"`). These are hints only: the
    /// installer never sets them, and writable per-user caches must stay in
    /// the user's home, not in the shared read-only prefix.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Directories (relative to the installed prefix, `"."` is the prefix
    /// itself) a shell should prepend to `PATH`. Absent means `["bin"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_dirs: Vec<String>,
    /// For an SDK that writes into its own folder while it runs (Flutter
    /// rewrites stamp files in `bin/cache` on every start): its commands run
    /// from a per-user mirror instead of the shared, read-only prefix. See
    /// [`Mirror`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<Mirror>,
}

/// A per-user mirror of an installed prefix, kept in the user's
/// `~/.cache/rast-sdk/<id>-<version>`: every folder is a real (writable)
/// folder, every file a link to the shared read-only file, except small
/// files (under 1 MiB)
///
/// * directly inside each `copy` folder (the SDK's own scripts and the stamp
///   files it rewrites in place), and
/// * anywhere below each `copyTree` folder (Flutter's `.git`: Git needs real
///   `HEAD`/ref files, and a `git fetch` then writes into the user's copy;
///   the large pack files stay links),
///
/// which are copied.
///
/// The installer writes a launcher per executable in [`MIRROR_DIR`] that
/// builds the mirror when needed and runs the executable inside it; command
/// links and checks use the launcher ([`Manifest::launch_path`]). The paths
/// are restricted like a phar path because the launcher is a shell script.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mirror {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_tree: Vec<String>,
}

/// Where the installer writes the launchers of a [`Mirror`] package.
pub const MIRROR_DIR: &str = ".rast-mirror";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Requirements {
    /// Minimum glibc (`2.28`). Absent = no glibc requirement (static builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glibc: Option<String>,
    /// Free-text host dependency notes shown to administrators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    /// `upstream` (unmodified upstream archive) or `built` (catalog/publisher CI).
    pub kind: String,
    /// Where the bytes (or the source they were built from) come from.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    /// How the publisher verified the upstream bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// CI run that produced or verified the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_url: Option<String>,
    /// Build-provenance attestation location, when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_url: Option<String>,
}

/// An icon shown for a package in the Package Center (runtimes) or App
/// Center (apps). The image is embedded as base64 in the manifest (and in the
/// catalog package entry), never fetched from the network, so an icon can not
/// track or leak anything. Published catalogs use icons to identify official
/// runtimes with their brand mark; see [`decode_icon`] for the exact rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageIcon {
    /// `image/svg+xml` or `image/png`
    pub media_type: String,
    /// Standard base64 (no data: prefix, no whitespace)
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

/// One additional archive of a host target. Rust is installed from five
/// component archives (`rustc`, `rust-std`, `cargo`, `rustfmt`, `clippy`)
/// extracted into the same prefix; the artifact's own `url`/`sha256`/`size`/
/// `format`/`stripComponents` describe the first (primary) archive, and
/// `archives` lists the others in extraction order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchivePart {
    /// Optional component name (`rust-std`), informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub format: String,
    #[serde(default)]
    pub strip_components: u32,
}

/// A single-file PHP phar artifact (`format: "phar"`).
///
/// The verified bytes are written to [`PharSpec::path`] inside the installed
/// prefix with mode 0644 — never executable, and never run at install time.
/// The artifact's `runtime.executables` (which must live in `bin/`) are
/// generated by the installer as small `#!/bin/sh` wrappers that run the phar
/// with the `php` interpreter found on `PATH`:
///
/// ```text
/// #!/bin/sh
/// self=$(readlink -f -- "$0" 2>/dev/null) || self=$0
/// root=$(CDPATH= cd -- "$(dirname -- "$self")/.." && pwd) || exit 1
/// exec php "$root/<path>" "$@"
/// ```
///
/// The wrapper is generated from fixed text plus a path restricted to
/// `[A-Za-z0-9._-]` components, so no manifest value is ever interpreted by
/// the shell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PharSpec {
    /// Destination of the phar inside the package prefix, e.g. `composer.phar`.
    pub path: String,
}

impl Artifact {
    /// Every archive of this target, the primary one first.
    pub fn archives(&self) -> Vec<ArchiveRef<'_>> {
        let mut all = Vec::with_capacity(1 + self.archives.len());
        all.push(ArchiveRef {
            name: None,
            url: &self.url,
            sha256: &self.sha256,
            size: self.size,
            format: &self.format,
            strip_components: self.strip_components,
        });
        all.extend(self.archives.iter().map(|part| ArchiveRef {
            name: part.name.as_deref(),
            url: &part.url,
            sha256: &part.sha256,
            size: part.size,
            format: &part.format,
            strip_components: part.strip_components,
        }));
        all
    }

    /// Sum of the archive sizes (free-space planning, progress).
    pub fn total_size(&self) -> u64 {
        self.size + self.archives.iter().map(|part| part.size).sum::<u64>()
    }
}

/// A borrowed view of one archive of an [`Artifact`].
#[derive(Debug, Clone, Copy)]
pub struct ArchiveRef<'a> {
    pub name: Option<&'a str>,
    pub url: &'a str,
    pub sha256: &'a str,
    pub size: u64,
    pub format: &'a str,
    pub strip_components: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// Artifact format: `tar.gz`, `tar.xz`, `zip` or `phar`. A `phar` artifact
    /// is a single PHP file, not an archive; see [`PharSpec`].
    pub format: String,
    #[serde(default)]
    pub strip_components: u32,
    /// Additional archives extracted into the same prefix after the primary
    /// one (multi-component runtimes such as Rust). Empty for a
    /// single-archive runtime, which keeps old manifests unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archives: Vec<ArchivePart>,
    /// Where a `format: "phar"` artifact is written inside the prefix.
    /// Absent for every archive format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phar: Option<PharSpec>,
    #[serde(default)]
    pub requires: Requirements,
    pub provenance: Provenance,
}

impl RuntimeSpec {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An app package: a desktop app (window) and, optionally, the server
/// program behind its API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppSpec {
    /// The desktop app id: windows, access rules and icons use it.
    pub id: String,
    pub window: AppWindow,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<AppServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppWindow {
    /// The window's ES module (relative path), whose default export mounts
    /// the app: `export default { mount(ctx, sdk) }`.
    pub entry: String,
    /// Style sheets the window needs (relative paths).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<String>,
    /// The window SDK version the module is built against.
    pub sdk: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub singleton: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_size: Option<WindowSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<WindowSize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WindowSize {
    pub w: u32,
    pub h: u32,
}

/// The program RAST runs for an app (as its own service account's child,
/// restarted when it stops) and forwards `apiPrefix` to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppServer {
    /// Relative path of the program inside the package.
    pub executable: String,
    /// The host API version it speaks.
    pub host_api: u32,
    /// `/api/<segment>`: requests below it go to the program.
    pub api_prefix: String,
    /// Prefix of the live event types it may publish, e.g. `ai.`.
    pub event_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub kind: String,
    pub id: String,
    #[serde(rename = "type")]
    pub package_type: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub publisher: Publisher,
    /// Upstream runtime version.
    pub version: String,
    /// Package revision (rebuilds of the same upstream version).
    pub revision: u32,
    /// Update stream; defaults to `major.minor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rast_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// `type: "runtime"`: what the runtime provides (absent for apps).
    #[serde(default, skip_serializing_if = "RuntimeSpec::is_empty")]
    pub runtime: RuntimeSpec,
    /// `type: "app"`: the desktop app and its server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppSpec>,
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Presentation icon embedded as base64; allowed on app and runtime
    /// manifests alike and also used for catalog package entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PackageIcon>,
}

impl Manifest {
    pub fn channel(&self) -> String {
        self.channel.clone().unwrap_or_else(|| {
            Version::parse(&self.version)
                .map(|v| v.default_channel())
                .unwrap_or_default()
        })
    }

    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.target == target)
    }

    pub fn targets(&self) -> Vec<String> {
        self.artifacts.iter().map(|a| a.target.clone()).collect()
    }

    pub fn is_prerelease(&self) -> bool {
        Version::parse(&self.version).is_some_and(|v| v.is_prerelease())
    }

    pub fn executable(&self, name: &str) -> Option<&Executable> {
        self.runtime.executables.iter().find(|e| e.name == name)
    }

    /// What to run for `exe`, relative to the installed prefix: the
    /// executable itself, or its generated launcher for a [`Mirror`] package.
    pub fn launch_path(&self, exe: &Executable) -> String {
        if self.runtime.mirror.is_some() {
            format!("{MIRROR_DIR}/{}", exe.name)
        } else {
            exe.path.clone()
        }
    }
}

/// Parse and fully validate a manifest document.
pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "manifest is larger than {} KiB",
            MAX_MANIFEST_BYTES / 1024
        ));
    }
    // Look at the declared type and schema first so authors of future package
    // types get a precise message instead of an unknown-field error.
    let loose: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest is not valid JSON: {e}"))?;
    check_envelope(&loose)?;
    let manifest: Manifest = serde_json::from_value(loose)
        .map_err(|e| format!("manifest does not match schema version 1: {e}"))?;
    validate(&manifest)?;
    Ok(manifest)
}

fn check_envelope(value: &serde_json::Value) -> Result<(), String> {
    let obj = value.as_object().ok_or("manifest must be a JSON object")?;
    match obj.get("kind").and_then(|v| v.as_str()) {
        Some(MANIFEST_KIND) => {}
        Some(other) => return Err(format!("kind must be {MANIFEST_KIND:?}, found {other:?}")),
        None => return Err(format!("kind must be {MANIFEST_KIND:?}")),
    }
    match obj.get("schemaVersion").and_then(|v| v.as_u64()) {
        Some(v) if v == SCHEMA_VERSION as u64 => {}
        Some(v) => {
            return Err(format!(
            "manifest schema version {v} is not supported by this RAST (supports {SCHEMA_VERSION})"
        ))
        }
        None => return Err("schemaVersion is required".into()),
    }
    if let Some(kind) = obj.get("type").and_then(|v| v.as_str()) {
        if PLANNED_TYPES.contains(&kind) {
            return Err(format!(
                "package type {kind:?} is not supported yet; this RAST installs: {}",
                SUPPORTED_TYPES.join(", ")
            ));
        }
        if !SUPPORTED_TYPES.contains(&kind) {
            return Err(format!(
                "unknown package type {kind:?}; this RAST installs: {}",
                SUPPORTED_TYPES.join(", ")
            ));
        }
    }
    Ok(())
}

fn is_id(value: &str, max: usize) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && value.len() <= max
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !value.ends_with('-')
}

fn text(field: &str, value: &str, min: usize, max: usize) -> Result<(), String> {
    let len = value.chars().count();
    if len < min || len > max {
        return Err(format!("{field} must be {min}-{max} characters"));
    }
    if value.chars().any(|c| c.is_control() && c != '\n') {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(())
}

pub fn is_https_url(value: &str) -> bool {
    value.len() <= 2048
        && value.starts_with("https://")
        && value.len() > "https://".len()
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn optional_url(field: &str, value: &Option<String>) -> Result<(), String> {
    match value {
        Some(url) if !is_https_url(url) => Err(format!("{field} must be an https:// URL")),
        _ => Ok(()),
    }
}

/// A relative path inside the package: no absolute paths, no `.`/`..`, no
/// empty components, printable characters only.
pub fn is_safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(|c| c.is_control())
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Versions double as directory names: digits, letters, `.`, `-`, `+`.
pub fn is_safe_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.starts_with(|c: char| c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
        && !value.contains("..")
        && Version::parse(value).is_some()
}

/// A phar destination path. Stricter than [`is_safe_relative_path`] because
/// the installer embeds it in a generated `#!/bin/sh` wrapper: only
/// `[A-Za-z0-9._+-]` components, so no shell metacharacter can appear.
pub fn is_phar_path(value: &str) -> bool {
    value.len() <= 200
        && is_safe_relative_path(value)
        && value.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
        })
}

/// An executable a generated phar wrapper can safely live next to: exactly
/// `bin/<name>`, so `$(dirname "$0")/..` is the package prefix.
pub fn is_wrapper_path(path: &str) -> bool {
    matches!(path.split_once('/'), Some(("bin", name)) if !name.is_empty() && !name.contains('/'))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Decode and validate an embedded package icon, returning the image bytes.
///
/// The same rules apply to a manifest's `icon` and to a catalog package
/// entry's `icon`:
///
/// * `mediaType` is `image/svg+xml` or `image/png`;
/// * `data` is standard base64 (padding required, no whitespace, no `data:`
///   prefix);
/// * the decoded image is 1 byte to 64 KiB;
/// * PNG data starts with the 8-byte PNG signature;
/// * SVG data is valid UTF-8 and contains no `<script`, `<foreignObject`,
///   no event handler attribute (`onload="…"`), no `javascript:` URL and no
///   external `href`/`xlink:href` to an http(s) resource.
///
/// The optional attribution text is checked by [`validate_icon`].
pub fn decode_icon(icon: &PackageIcon) -> Result<Vec<u8>, String> {
    if !ICON_MEDIA_TYPES.contains(&icon.media_type.as_str()) {
        return Err(format!(
            "icon mediaType {:?} is not supported (use {})",
            icon.media_type,
            ICON_MEDIA_TYPES.join(" or ")
        ));
    }
    let bytes = B64
        .decode(icon.data.as_bytes())
        .map_err(|error| format!("icon data is not standard base64: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_ICON_BYTES {
        return Err(format!(
            "icon data must be 1 byte to {} KiB, found {} bytes",
            MAX_ICON_BYTES / 1024,
            bytes.len()
        ));
    }
    match icon.media_type.as_str() {
        "image/png" => {
            if !bytes.starts_with(PNG_SIGNATURE) {
                return Err("icon data is not a PNG image (bad signature)".into());
            }
        }
        _ => validate_icon_svg(&bytes)?,
    }
    Ok(bytes)
}

/// Every rule of an embedded package icon: [`decode_icon`] plus an attribution
/// of at most [`MAX_ICON_ATTRIBUTION_CHARS`] characters.
pub fn validate_icon(icon: &PackageIcon) -> Result<(), String> {
    decode_icon(icon)?;
    if let Some(attribution) = &icon.attribution {
        text(
            "icon.attribution",
            attribution,
            0,
            MAX_ICON_ATTRIBUTION_CHARS,
        )?;
    }
    Ok(())
}

/// Load an icon file (`<name>.svg` or `<name>.png`) as an embeddable
/// [`PackageIcon`], validating it through the same rules as an inline icon.
/// Used by the catalog tooling to embed the official brand marks; attribution
/// is omitted (catalog authors add it in the manifest/schema when needed).
pub fn package_icon_from_file(path: &Path) -> Result<PackageIcon, String> {
    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => {
            return Err(format!(
                "icon {} must be a .svg or .png file",
                path.display()
            ))
        }
    };
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read icon {}: {error}", path.display()))?;
    if metadata.len() > MAX_ICON_BYTES as u64 {
        return Err(format!(
            "icon {} is larger than {} KiB",
            path.display(),
            MAX_ICON_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read icon {}: {error}", path.display()))?;
    if bytes.len() > MAX_ICON_BYTES {
        return Err(format!(
            "icon {} is larger than {} KiB",
            path.display(),
            MAX_ICON_BYTES / 1024
        ));
    }
    let icon = PackageIcon {
        media_type: media_type.into(),
        data: B64.encode(&bytes),
        attribution: None,
    };
    validate_icon(&icon).map_err(|error| format!("icon {}: {error}", path.display()))?;
    Ok(icon)
}

/// SVG is text; reject active content so an embedded icon can not script the
/// Web UI. Simple Icons brand marks (the official catalog's icons) pass.
fn validate_icon_svg(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "icon data is not valid UTF-8 (image/svg+xml must be text)")?;
    // The checks are case-insensitive: SVG/XML names and attribute values in
    // an HTML foreign-content context are case-insensitive too.
    let lower = text.to_ascii_lowercase();
    for (needle, label) in [
        ("<script", "a script element"),
        ("<foreignobject", "a foreignObject element"),
        ("javascript:", "a javascript: URL"),
    ] {
        if lower.contains(needle) {
            return Err(format!("icon SVG must not contain {label}"));
        }
    }
    if has_svg_event_handler(&lower) {
        return Err("icon SVG must not contain an event handler attribute".into());
    }
    if has_svg_external_href(&lower) {
        return Err("icon SVG must not reference an external http(s) resource".into());
    }
    Ok(())
}

/// Whether the lowercase document contains an `on<alpha>+=` attribute.
fn has_svg_event_handler(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'o'
            && bytes[index + 1] == b'n'
            && bytes[index + 2].is_ascii_alphabetic()
        {
            let mut cursor = index + 2;
            while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                cursor += 1;
            }
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'=' {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Whether the lowercase document has an `href=`/`xlink:href=` whose value
/// starts with an absolute `http:`/`https:` URL.
fn has_svg_external_href(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut search = 0;
    while let Some(offset) = lower[search..].find("href") {
        let start = search + offset + "href".len();
        let mut cursor = start;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'=' {
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let value = lower[cursor..].trim_start_matches(['"', '\'']);
            if value.starts_with("http:") || value.starts_with("https:") {
                return true;
            }
        }
        search = start;
    }
    false
}

/// Every rule of schema version 1 (after JSON shape checks).
pub fn validate(m: &Manifest) -> Result<(), String> {
    if m.schema_version != SCHEMA_VERSION {
        return Err(format!("schemaVersion must be {SCHEMA_VERSION}"));
    }
    if m.kind != MANIFEST_KIND {
        return Err(format!("kind must be {MANIFEST_KIND:?}"));
    }
    if !is_id(&m.id, 40) {
        return Err(format!(
            "id {:?} must be 1-40 lowercase letters, digits or dashes",
            m.id
        ));
    }
    if RESERVED_IDS.contains(&m.id.as_str()) {
        return Err(format!("id {:?} is reserved", m.id));
    }
    if !SUPPORTED_TYPES.contains(&m.package_type.as_str()) {
        return Err(format!("unsupported package type {:?}", m.package_type));
    }
    text("name", &m.name, 1, 64)?;
    text("description", &m.description, 0, 500)?;
    optional_url("homepage", &m.homepage)?;
    if let Some(license) = &m.license {
        text("license", license, 1, 64)?;
    }
    if !is_id(&m.publisher.id, 40) {
        return Err(format!(
            "publisher.id {:?} must be 1-40 lowercase letters, digits or dashes",
            m.publisher.id
        ));
    }
    text("publisher.name", &m.publisher.name, 1, 64)?;
    optional_url("publisher.url", &m.publisher.url)?;
    if !is_safe_version(&m.version) {
        return Err(format!("version {:?} is not a valid version", m.version));
    }
    if m.revision == 0 || m.revision > 10_000 {
        return Err("revision must be between 1 and 10000".into());
    }
    if let Some(channel) = &m.channel {
        if channel.is_empty()
            || channel.len() > 16
            || !channel
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(format!("channel {channel:?} is not valid"));
        }
    }
    if let Some(min) = &m.min_rast_version {
        if Version::parse(min).is_none() {
            return Err(format!("minRastVersion {min:?} is not a version"));
        }
    }
    if let Some(date) = &m.release_date {
        text("releaseDate", date, 1, 32)?;
    }
    if let Some(notes) = &m.notes {
        text("notes", notes, 0, 1000)?;
    }
    if let Some(icon) = &m.icon {
        validate_icon(icon)?;
    }
    match m.package_type.as_str() {
        "app" => {
            if !m.runtime.is_empty() {
                return Err("an app package has no runtime section".into());
            }
            validate_app(
                m.app
                    .as_ref()
                    .ok_or("an app package needs an app section")?,
            )?;
        }
        _ => {
            if m.app.is_some() {
                return Err("only app packages have an app section".into());
            }
            validate_runtime(&m.runtime)?;
        }
    }
    validate_artifacts(&m.artifacts)?;
    validate_phar(&m.runtime, &m.artifacts)?;
    Ok(())
}

fn validate_app(app: &AppSpec) -> Result<(), String> {
    if !is_id(&app.id, 40) {
        return Err(format!(
            "app.id {:?} must be 1-40 lowercase letters, digits or dashes",
            app.id
        ));
    }
    let window = &app.window;
    let file = |field: &str, path: &str, ext: &str| {
        if is_safe_relative_path(path) && path.ends_with(ext) {
            Ok(())
        } else {
            Err(format!(
                "{field} {path:?} must be a relative path ending in {ext}"
            ))
        }
    };
    file("app.window.entry", &window.entry, ".js")?;
    if window.styles.len() > 8 {
        return Err("app.window.styles lists at most 8 files".into());
    }
    for style in &window.styles {
        file("app.window.styles", style, ".css")?;
    }
    if !WINDOW_SDK_VERSIONS.contains(&window.sdk) {
        return Err(format!(
            "app.window.sdk {} is not offered by this RAST (offers {:?})",
            window.sdk, WINDOW_SDK_VERSIONS
        ));
    }
    for size in [window.default_size, window.min_size].into_iter().flatten() {
        if !(200..=4000).contains(&size.w) || !(150..=3000).contains(&size.h) {
            return Err("app.window sizes must be 200-4000 wide and 150-3000 high".into());
        }
    }
    if let Some(server) = &app.server {
        if !is_safe_relative_path(&server.executable) {
            return Err(format!(
                "app.server.executable {:?} must be a relative path",
                server.executable
            ));
        }
        if !HOST_API_VERSIONS.contains(&server.host_api) {
            return Err(format!(
                "app.server.hostApi {} is not offered by this RAST (offers {:?})",
                server.host_api, HOST_API_VERSIONS
            ));
        }
        let segment = server.api_prefix.strip_prefix("/api/").unwrap_or("");
        if !is_id(segment, 32) {
            return Err(format!(
                "app.server.apiPrefix {:?} must be /api/<lowercase name>",
                server.api_prefix
            ));
        }
        if HOST_API_SEGMENTS.contains(&segment) {
            return Err(format!(
                "app.server.apiPrefix {:?} belongs to RAST",
                server.api_prefix
            ));
        }
        let event = server.event_prefix.strip_suffix('.').unwrap_or("");
        if !is_id(event, 32) {
            return Err(format!(
                "app.server.eventPrefix {:?} must be a lowercase name and a dot, like \"ai.\"",
                server.event_prefix
            ));
        }
    }
    Ok(())
}

/// Package-level rules for single-file phar artifacts: their executables are
/// generated wrappers in `bin/`, and a wrapper can never shadow the phar
/// itself.
fn validate_phar(runtime: &RuntimeSpec, artifacts: &[Artifact]) -> Result<(), String> {
    let phars: Vec<&PharSpec> = artifacts.iter().filter_map(|a| a.phar.as_ref()).collect();
    if phars.is_empty() {
        return Ok(());
    }
    for exe in &runtime.executables {
        if !is_wrapper_path(&exe.path) {
            return Err(format!(
                "executable {:?} of a phar package must be a wrapper in bin/ (bin/<name>)",
                exe.path
            ));
        }
    }
    for phar in &phars {
        if runtime.executables.iter().any(|exe| exe.path == phar.path) {
            return Err(format!(
                "executable {:?} would overwrite the phar file",
                phar.path
            ));
        }
    }
    Ok(())
}

fn validate_runtime(r: &RuntimeSpec) -> Result<(), String> {
    if r.executables.is_empty() || r.executables.len() > 32 {
        return Err("runtime.executables must list 1-32 executables".into());
    }
    let mut names = BTreeSet::new();
    for exe in &r.executables {
        let valid_name = !exe.name.is_empty()
            && exe.name.len() <= 32
            && exe.name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && exe
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
        if !valid_name {
            return Err(format!("executable name {:?} is not valid", exe.name));
        }
        if !names.insert(exe.name.as_str()) {
            return Err(format!("executable {:?} is listed twice", exe.name));
        }
        if !is_safe_relative_path(&exe.path) {
            return Err(format!(
                "executable path {:?} must be a relative path inside the package",
                exe.path
            ));
        }
    }
    if let Some(mirror) = &r.mirror {
        for exe in &r.executables {
            if !is_phar_path(&exe.path) {
                return Err(format!(
                    "executable path {:?} of a mirrored package must use only [A-Za-z0-9._+-] components",
                    exe.path
                ));
            }
        }
        if mirror.copy.len() > 16 || mirror.copy_tree.len() > 16 {
            return Err("runtime.mirror lists at most 16 copy and 16 copyTree entries".into());
        }
        for path in mirror.copy.iter().chain(&mirror.copy_tree) {
            if !is_phar_path(path) || path.starts_with(MIRROR_DIR) {
                return Err(format!(
                    "runtime.mirror path {path:?} must be a relative path of [A-Za-z0-9._+-] components"
                ));
            }
        }
    }
    if r.checks.is_empty() || r.checks.len() > 8 {
        return Err("runtime.checks must list 1-8 version checks".into());
    }
    for check in &r.checks {
        if !names.contains(check.executable.as_str()) {
            return Err(format!(
                "check uses {:?}, which is not a declared executable",
                check.executable
            ));
        }
        if check.args.len() > 8 {
            return Err("a check may pass at most 8 arguments".into());
        }
        for arg in &check.args {
            text("check argument", arg, 1, 64)?;
        }
        text("check expect", &check.expect, 1, 128)?;
        if let Some(label) = &check.label {
            text("check label", label, 1, 32)?;
        }
    }
    if r.bundled.len() > 16 {
        return Err("runtime.bundled may list at most 16 tools".into());
    }
    for tool in &r.bundled {
        text("bundled name", &tool.name, 1, 32)?;
        text("bundled version", &tool.version, 1, 32)?;
    }
    if r.extensions.len() > 200 {
        return Err("runtime.extensions may list at most 200 entries".into());
    }
    for ext in &r.extensions {
        text("extension", ext, 1, 48)?;
    }
    if r.env.len() > 16 {
        return Err("runtime.env may list at most 16 variables".into());
    }
    for (name, value) in &r.env {
        let valid_name = !name.is_empty()
            && name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_uppercase())
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid_name {
            return Err(format!(
                "runtime.env name {name:?} is not an environment variable"
            ));
        }
        if !is_env_template(value) {
            return Err(format!(
                "runtime.env.{name} must be \"{{root}}\" or \"{{root}}/<relative path>\""
            ));
        }
    }
    if r.path_dirs.len() > 8 {
        return Err("runtime.pathDirs may list at most 8 directories".into());
    }
    for dir in &r.path_dirs {
        // `.` means the prefix itself (runtimes like .NET, Bun and Deno put
        // their command at the archive root); anything else must be a safe
        // relative directory inside it.
        if dir != "." && !is_safe_relative_path(dir) {
            return Err(format!(
                "runtime.pathDirs entry {dir:?} must be \".\" or a relative path inside the package"
            ));
        }
    }
    Ok(())
}

/// `{root}` or `{root}/<safe relative path>`: the only templates allowed in
/// [`RuntimeSpec::env`] values.
fn is_env_template(value: &str) -> bool {
    if value.len() > 512 {
        return false;
    }
    match value.strip_prefix("{root}") {
        Some("") => true,
        Some(rest) => rest.strip_prefix('/').is_some_and(is_safe_relative_path),
        None => false,
    }
}

fn validate_artifacts(artifacts: &[Artifact]) -> Result<(), String> {
    if artifacts.is_empty() || artifacts.len() > 8 {
        return Err("artifacts must list 1-8 host targets".into());
    }
    let mut targets = BTreeSet::new();
    for a in artifacts {
        if !KNOWN_TARGETS.contains(&a.target.as_str()) {
            return Err(format!(
                "unknown target {:?}; known targets: {}",
                a.target,
                KNOWN_TARGETS.join(", ")
            ));
        }
        if !targets.insert(a.target.as_str()) {
            return Err(format!("target {:?} is listed twice", a.target));
        }
        if a.archives.len() > MAX_ARCHIVES_PER_TARGET - 1 {
            return Err(format!(
                "artifacts for {} may list at most {} additional archives",
                a.target,
                MAX_ARCHIVES_PER_TARGET - 1
            ));
        }
        // A phar artifact is one verified file written into the prefix (never
        // executed at install time); every other format is extracted and may
        // carry component archives.
        if a.format == "phar" {
            if !a.archives.is_empty() {
                return Err(format!(
                    "artifact {}: a phar artifact cannot list additional archives",
                    a.target
                ));
            }
            if a.strip_components != 0 {
                return Err(format!(
                    "artifact {}: a phar artifact must use stripComponents 0",
                    a.target
                ));
            }
            let phar = a.phar.as_ref().ok_or_else(|| {
                format!(
                    "artifact {}: format \"phar\" requires a phar destination path",
                    a.target
                )
            })?;
            if !is_phar_path(&phar.path) {
                return Err(format!(
                    "artifact {}: phar.path {:?} must be a relative path of [A-Za-z0-9._+-] components",
                    a.target, phar.path
                ));
            }
        } else if a.phar.is_some() {
            return Err(format!(
                "artifact {}: phar metadata is only allowed with format \"phar\"",
                a.target
            ));
        }
        let mut urls = BTreeSet::new();
        for archive in a.archives() {
            let label = match archive.name {
                Some(name) => format!("{} ({name})", a.target),
                None => a.target.clone(),
            };
            if !is_https_url(archive.url) {
                return Err(format!("artifact archive {label} URL must be https://"));
            }
            if !urls.insert(archive.url) {
                return Err(format!("artifact archive {label} lists a URL twice"));
            }
            if !is_sha256(archive.sha256) {
                return Err(format!(
                    "artifact archive {label} sha256 must be 64 lowercase hex characters"
                ));
            }
            if archive.size == 0 || archive.size > MAX_ARTIFACT_BYTES {
                return Err(format!("artifact archive {label} size is out of range"));
            }
            // The primary archive may be the single-file phar; components are
            // always extracted.
            let formats = if archive.name.is_none() {
                ARCHIVE_FORMATS
            } else {
                EXTRACTABLE_FORMATS
            };
            if !formats.contains(&archive.format) {
                return Err(format!(
                    "artifact archive {label} format {:?} is not supported ({})",
                    archive.format,
                    formats.join(", ")
                ));
            }
            if archive.strip_components > 4 {
                return Err("stripComponents must be 0-4".into());
            }
        }
        if let Some(glibc) = &a.requires.glibc {
            if Version::parse(glibc).is_none() {
                return Err(format!("requires.glibc {glibc:?} is not a version"));
            }
        }
        if let Some(notes) = &a.requires.notes {
            text("requires.notes", notes, 0, 500)?;
        }
        let p = &a.provenance;
        if p.kind != "upstream" && p.kind != "built" {
            return Err("provenance.kind must be \"upstream\" or \"built\"".into());
        }
        if !is_https_url(&p.source) {
            return Err("provenance.source must be an https:// URL".into());
        }
        if let Some(sha) = &p.source_sha256 {
            if !is_sha256(sha) {
                return Err("provenance.sourceSha256 must be a sha256 hex digest".into());
            }
        }
        if let Some(text_value) = &p.verification {
            text("provenance.verification", text_value, 1, 300)?;
        }
        optional_url("provenance.buildUrl", &p.build_url)?;
        optional_url("provenance.attestationUrl", &p.attestation_url)?;
    }
    Ok(())
}

/// Order manifests newest first.
pub fn sort_newest_first(list: &mut [Manifest]) {
    list.sort_by(|a, b| version::compare(&b.version, &a.version).then(b.revision.cmp(&a.revision)));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample() -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 1,
            "kind": "rast-package",
            "id": "lua",
            "type": "runtime",
            "name": "Lua",
            "description": "Lua scripting language",
            "homepage": "https://www.lua.org",
            "license": "MIT",
            "publisher": {"id": "example", "name": "Example Publisher", "url": "https://github.com/example"},
            "version": "5.4.8",
            "revision": 1,
            "minRastVersion": "0.5.0",
            "runtime": {
                "executables": [{"name": "lua", "path": "bin/lua"}, {"name": "luac", "path": "bin/luac"}],
                "checks": [{"executable": "lua", "args": ["-v"], "expect": "Lua 5.4.8"}]
            },
            "artifacts": [{
                "target": "linux-x86_64",
                "url": "https://github.com/example/lua/releases/download/v5.4.8-1/lua-5.4.8-linux-x86_64.tar.gz",
                "sha256": "a".repeat(64),
                "size": 1234,
                "format": "tar.gz",
                "stripComponents": 1,
                "provenance": {"kind": "built", "source": "https://www.lua.org/ftp/lua-5.4.8.tar.gz"}
            }]
        })
    }

    /// The sample's artifact as a struct (tests adjust url/sha256/size).
    pub fn sample_artifact() -> Artifact {
        serde_json::from_value(sample()["artifacts"][0].clone()).unwrap()
    }

    fn parse_value(v: &serde_json::Value) -> Result<Manifest, String> {
        parse(&serde_json::to_vec(v).unwrap())
    }

    #[test]
    fn valid_manifest_parses() {
        let m = parse_value(&sample()).unwrap();
        assert_eq!(m.channel(), "5.4");
        assert_eq!(m.targets(), vec!["linux-x86_64"]);
        assert!(m.artifact_for("linux-aarch64").is_none());
    }

    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;

    #[test]
    fn rejects_unsafe_or_unknown_fields() {
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "abs path",
                Box::new(|v| v["runtime"]["executables"][0]["path"] = "/usr/bin/lua".into()),
            ),
            (
                "dotdot",
                Box::new(|v| v["runtime"]["executables"][0]["path"] = "bin/../../lua".into()),
            ),
            (
                "http url",
                Box::new(|v| v["artifacts"][0]["url"] = "http://example.com/a.tar.gz".into()),
            ),
            (
                "bad sha",
                Box::new(|v| v["artifacts"][0]["sha256"] = "XYZ".into()),
            ),
            (
                "bad target",
                Box::new(|v| v["artifacts"][0]["target"] = "windows-x86_64".into()),
            ),
            (
                "bad format",
                Box::new(|v| v["artifacts"][0]["format"] = "rar".into()),
            ),
            ("bad id", Box::new(|v| v["id"] = "../lua".into())),
            ("reserved id", Box::new(|v| v["id"] = "bin".into())),
            (
                "bad version",
                Box::new(|v| v["version"] = "5.4.8/../x".into()),
            ),
            (
                "unknown exe in check",
                Box::new(|v| v["runtime"]["checks"][0]["executable"] = "sh".into()),
            ),
            (
                "unknown field",
                Box::new(|v| v["postInstall"] = "rm -rf /".into()),
            ),
            (
                "script op",
                Box::new(|v| v["runtime"]["install"] = "curl | sh".into()),
            ),
            ("schema", Box::new(|v| v["schemaVersion"] = 2.into())),
            ("zero revision", Box::new(|v| v["revision"] = 0.into())),
        ];
        for (name, mutate) in cases {
            let mut v = sample();
            mutate(&mut v);
            assert!(parse_value(&v).is_err(), "{name} accepted");
        }
    }

    #[test]
    fn future_types_get_a_precise_message() {
        let mut v = sample();
        v["type"] = "service".into();
        let err = parse_value(&v).unwrap_err();
        assert!(err.contains("not supported yet"), "{err}");
    }

    /// An app package like AI Agent: a window and a server.
    pub fn sample_app() -> serde_json::Value {
        let mut v = sample();
        let obj = v.as_object_mut().unwrap();
        obj.remove("runtime");
        obj.insert("id".into(), "ai-agent".into());
        obj.insert("type".into(), "app".into());
        obj.insert("name".into(), "AI Agent".into());
        obj.insert("version".into(), "1.0.0".into());
        obj.insert(
            "app".into(),
            serde_json::json!({
                "id": "agent",
                "window": {"entry": "ui/app.js", "styles": ["ui/app.css"], "sdk": 1, "singleton": true, "defaultSize": {"w": 1080, "h": 720}},
                "server": {"executable": "bin/rast-ai-agent", "hostApi": 1, "apiPrefix": "/api/ai", "eventPrefix": "ai."}
            }),
        );
        v
    }

    #[test]
    fn app_packages_parse_and_are_checked() {
        let m = parse_value(&sample_app()).unwrap();
        let app = m.app.as_ref().unwrap();
        assert_eq!((app.id.as_str(), app.window.sdk), ("agent", 1));
        assert!(m.runtime.is_empty());
        let round = serde_json::to_value(&m).unwrap();
        assert!(round.get("runtime").is_none(), "{round}");
        assert_eq!(parse_value(&round).unwrap(), m);
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "no app section",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("app");
                }),
            ),
            (
                "a runtime section too",
                Box::new(|v| v["runtime"] = sample()["runtime"].clone()),
            ),
            (
                "app on a runtime",
                Box::new(|v| v["type"] = "runtime".into()),
            ),
            (
                "RAST's own API",
                Box::new(|v| v["app"]["server"]["apiPrefix"] = "/api/files".into()),
            ),
            (
                "deep prefix",
                Box::new(|v| v["app"]["server"]["apiPrefix"] = "/api/ai/x".into()),
            ),
            (
                "not under /api",
                Box::new(|v| v["app"]["server"]["apiPrefix"] = "/ai".into()),
            ),
            (
                "event prefix",
                Box::new(|v| v["app"]["server"]["eventPrefix"] = "ai".into()),
            ),
            (
                "future host API",
                Box::new(|v| v["app"]["server"]["hostApi"] = 2.into()),
            ),
            (
                "future SDK",
                Box::new(|v| v["app"]["window"]["sdk"] = 9.into()),
            ),
            (
                "entry outside",
                Box::new(|v| v["app"]["window"]["entry"] = "../app.js".into()),
            ),
            (
                "entry not a module",
                Box::new(|v| v["app"]["window"]["entry"] = "ui/app.html".into()),
            ),
            (
                "server outside",
                Box::new(|v| v["app"]["server"]["executable"] = "/bin/sh".into()),
            ),
            ("app id", Box::new(|v| v["app"]["id"] = "Agent!".into())),
            (
                "tiny window",
                Box::new(|v| v["app"]["window"]["minSize"] = serde_json::json!({"w": 10, "h": 10})),
            ),
            (
                "unknown field",
                Box::new(|v| v["app"]["window"]["html"] = true.into()),
            ),
        ];
        for (name, mutate) in cases {
            let mut v = sample_app();
            mutate(&mut v);
            assert!(parse_value(&v).is_err(), "{name} accepted");
        }
        let mut window_only = sample_app();
        window_only["app"].as_object_mut().unwrap().remove("server");
        assert!(parse_value(&window_only)
            .unwrap()
            .app
            .unwrap()
            .server
            .is_none());
    }

    /// A zip artifact like Bun/Deno: one archive, stripComponents 0/1.
    #[test]
    fn zip_artifacts_are_valid() {
        let mut v = sample();
        v["artifacts"][0]["format"] = "zip".into();
        v["artifacts"][0]["url"] =
            "https://github.com/example/lua/releases/download/v5.4.8-1/lua.zip".into();
        let m = parse_value(&v).unwrap();
        assert_eq!(m.artifacts[0].format, "zip");
        assert!(m.artifacts[0].archives.is_empty());
        assert_eq!(m.artifacts[0].total_size(), 1234);
    }

    /// Rust: the primary archive (rustc) plus the other components, each with
    /// its own URL/hash/size/strip; all five merge into the same prefix.
    #[test]
    fn multi_archive_artifacts_are_valid() {
        let mut v = sample();
        v["artifacts"][0]["stripComponents"] = serde_json::json!(2);
        v["artifacts"][0]["archives"] = serde_json::json!([
            {"name": "rust-std", "url": "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-x86_64-unknown-linux-gnu.tar.gz",
             "sha256": "b".repeat(64), "size": 2000, "format": "tar.gz", "stripComponents": 2},
            {"name": "cargo", "url": "https://static.rust-lang.org/dist/2026-11-12/cargo-1.100.0-x86_64-unknown-linux-gnu.tar.gz",
             "sha256": "c".repeat(64), "size": 3000, "format": "tar.gz", "stripComponents": 2},
            {"name": "rustfmt", "url": "https://static.rust-lang.org/dist/2026-11-12/rustfmt-1.100.0-x86_64-unknown-linux-gnu.tar.gz",
             "sha256": "d".repeat(64), "size": 4000, "format": "tar.gz", "stripComponents": 2},
            {"name": "clippy", "url": "https://static.rust-lang.org/dist/2026-11-12/clippy-1.100.0-x86_64-unknown-linux-gnu.tar.gz",
             "sha256": "e".repeat(64), "size": 5000, "format": "tar.gz", "stripComponents": 2}
        ]);
        let m = parse_value(&v).unwrap();
        let artifact = &m.artifacts[0];
        assert_eq!(artifact.archives().len(), 5);
        assert_eq!(artifact.total_size(), 1234 + 2000 + 3000 + 4000 + 5000);
        assert_eq!(artifact.archives()[1].name, Some("rust-std"));
        assert_eq!(artifact.archives()[4].strip_components, 2);

        // Each archive is validated on its own.
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "archive http url",
                Box::new(|v| {
                    v["artifacts"][0]["archives"][0]["url"] =
                        "http://static.rust-lang.org/a.tar.gz".into()
                }),
            ),
            (
                "archive bad sha",
                Box::new(|v| v["artifacts"][0]["archives"][1]["sha256"] = "nope".into()),
            ),
            (
                "archive zero size",
                Box::new(|v| v["artifacts"][0]["archives"][2]["size"] = 0.into()),
            ),
            (
                "archive bad format",
                Box::new(|v| v["artifacts"][0]["archives"][3]["format"] = "rar".into()),
            ),
            (
                "archive strip too high",
                Box::new(|v| v["artifacts"][0]["archives"][0]["stripComponents"] = 9.into()),
            ),
            (
                "archive duplicate url",
                Box::new(|v| {
                    v["artifacts"][0]["archives"][0]["url"] = v["artifacts"][0]["url"].clone()
                }),
            ),
            (
                "too many archives",
                Box::new(|v| {
                    let archives = v["artifacts"][0]["archives"].as_array().unwrap();
                    let mut many = archives.clone();
                    for _ in 0..8 {
                        many.push(archives[0].clone());
                    }
                    v["artifacts"][0]["archives"] = serde_json::Value::Array(many);
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut v = v.clone();
            mutate(&mut v);
            assert!(parse_value(&v).is_err(), "{name} accepted");
        }
    }

    /// Manifests written before multi-component support must stay
    /// byte-compatible: no empty `archives`/`env`/`pathDirs` keys.
    #[test]
    fn single_archive_manifests_serialize_unchanged() {
        let m = parse_value(&sample()).unwrap();
        let value = serde_json::to_value(&m).unwrap();
        assert!(value["artifacts"][0].get("archives").is_none());
        assert!(value["artifacts"][0].get("phar").is_none());
        assert!(value["runtime"].get("env").is_none());
        assert!(value["runtime"].get("pathDirs").is_none());
    }

    /// `.tar.xz` artifacts (Clang, Flutter, Zig) are valid; the format was
    /// added after schema version 1 shipped, so old manifests stay valid.
    #[test]
    fn tar_xz_artifacts_are_valid() {
        let mut v = sample();
        v["artifacts"][0]["format"] = "tar.xz".into();
        v["artifacts"][0]["url"] =
            "https://github.com/example/lua/releases/download/v5.4.8-1/lua-linux-x86_64.tar.xz"
                .into();
        let m = parse_value(&v).unwrap();
        assert_eq!(m.artifacts[0].format, "tar.xz");
        assert!(m.artifacts[0].phar.is_none());
    }

    /// A single-file phar (Composer) plus its generated `bin/` wrapper.
    fn phar_sample() -> serde_json::Value {
        let mut v = sample();
        v["id"] = "composer".into();
        v["version"] = "2.11.2".into();
        v["runtime"] = serde_json::json!({
            "executables": [{"name": "composer", "path": "bin/composer"}],
            "checks": [{"executable": "composer", "args": ["--version"], "expect": "Composer version 2.11.2"}]
        });
        v["artifacts"][0]["format"] = "phar".into();
        v["artifacts"][0]["stripComponents"] = 0.into();
        v["artifacts"][0]["url"] =
            "https://github.com/composer/composer/releases/download/2.11.2/composer.phar".into();
        v["artifacts"][0]["phar"] = serde_json::json!({"path": "composer.phar"});
        v
    }

    #[test]
    fn phar_artifacts_validate_and_serialize_the_destination() {
        let m = parse_value(&phar_sample()).unwrap();
        assert_eq!(m.artifacts[0].format, "phar");
        assert_eq!(m.artifacts[0].phar.as_ref().unwrap().path, "composer.phar");
        let value = serde_json::to_value(&m).unwrap();
        assert_eq!(value["artifacts"][0]["phar"]["path"], "composer.phar");
        assert!(is_phar_path("composer.phar"));
        assert!(is_phar_path("tools/phpstan.phar"));
        assert!(is_wrapper_path("bin/composer"));
        assert!(!is_wrapper_path("composer"));
        assert!(!is_wrapper_path("bin/x/y"));

        let cases: Vec<(&str, Mutation)> = vec![
            (
                "missing phar destination",
                Box::new(|v| {
                    v["artifacts"][0].as_object_mut().unwrap().remove("phar");
                }),
            ),
            (
                "phar metadata on an archive",
                Box::new(|v| v["artifacts"][0]["format"] = "tar.gz".into()),
            ),
            (
                "phar with component archives",
                Box::new(|v| {
                    v["artifacts"][0]["format"] = "phar".into();
                    v["artifacts"][0]["archives"] = serde_json::json!([
                        {"url": "https://example.com/a.tar.gz", "sha256": "b".repeat(64),
                         "size": 10, "format": "tar.gz", "stripComponents": 1}
                    ]);
                }),
            ),
            (
                "phar stripComponents",
                Box::new(|v| v["artifacts"][0]["stripComponents"] = 1.into()),
            ),
            (
                "phar path escapes",
                Box::new(|v| v["artifacts"][0]["phar"]["path"] = "../composer.phar".into()),
            ),
            (
                "phar path shell metacharacter",
                Box::new(|v| v["artifacts"][0]["phar"]["path"] = "composer$(id).phar".into()),
            ),
            (
                "wrapper outside bin",
                Box::new(|v| v["runtime"]["executables"][0]["path"] = "composer".into()),
            ),
            (
                "wrapper overwrites the phar",
                Box::new(|v| v["artifacts"][0]["phar"]["path"] = "bin/composer".into()),
            ),
            (
                "component archive is a phar",
                Box::new(|v| {
                    v["artifacts"][0]["format"] = "tar.gz".into();
                    v["artifacts"][0]["archives"] = serde_json::json!([
                        {"url": "https://example.com/a.phar", "sha256": "b".repeat(64),
                         "size": 10, "format": "phar", "stripComponents": 0}
                    ]);
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut value = phar_sample();
            mutate(&mut value);
            assert!(parse_value(&value).is_err(), "{name} accepted");
        }
    }

    /// A 1x1 transparent PNG (a real, minimal PNG file).
    const PNG_1X1_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn tiny_svg() -> &'static str {
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="currentColor" d="M12 2 2 22h20z"/></svg>"#
    }

    fn icon(media_type: &str, data: &[u8]) -> serde_json::Value {
        serde_json::json!({"mediaType": media_type, "data": B64.encode(data)})
    }

    #[test]
    fn mirrored_packages_launch_through_generated_launchers() {
        let mut v = sample();
        v["runtime"]["mirror"] =
            serde_json::json!({"copy": ["bin", "bin/cache"], "copyTree": [".git"]});
        let m = parse_value(&v).unwrap();
        let exe = m.executable("lua").unwrap();
        assert_eq!(m.launch_path(exe), ".rast-mirror/lua");
        assert_eq!(parse_value(&sample()).unwrap().launch_path(exe), "bin/lua");
        // The paths end up in a shell script: no shell syntax, nothing absolute.
        for bad in [
            "bin cache",
            "bin;x",
            "$HOME",
            "/etc",
            "../x",
            ".rast-mirror",
        ] {
            let mut v = sample();
            v["runtime"]["mirror"] = serde_json::json!({"copy": [bad]});
            assert!(parse_value(&v).is_err(), "{bad}");
        }
        let mut v = sample();
        v["runtime"]["mirror"] = serde_json::json!({});
        v["runtime"]["executables"][0]["path"] = "bin/lu a".into();
        assert!(parse_value(&v).is_err());
    }

    #[test]
    fn package_icons_are_embedded_and_validated() {
        let mut v = sample();
        v["icon"] = icon("image/svg+xml", tiny_svg().as_bytes());
        let m = parse_value(&v).unwrap();
        let embedded = m.icon.as_ref().unwrap();
        assert_eq!(embedded.media_type, "image/svg+xml");
        assert!(embedded.attribution.is_none());
        assert_eq!(decode_icon(embedded).unwrap(), tiny_svg().as_bytes());
        // Round-trips with camelCase keys and no attribution when absent.
        let value = serde_json::to_value(&m).unwrap();
        assert_eq!(value["icon"]["mediaType"], "image/svg+xml");
        assert_eq!(value["icon"]["data"], B64.encode(tiny_svg().as_bytes()));
        assert!(value["icon"].get("attribution").is_none());

        // Attribution (CC0 brand marks credit their source) is optional text.
        v["icon"]["attribution"] = "Simple Icons (CC0)".into();
        let m = parse_value(&v).unwrap();
        assert_eq!(
            m.icon.as_ref().unwrap().attribution.as_deref(),
            Some("Simple Icons (CC0)")
        );

        // A real PNG passes (signature check).
        let png = B64.decode(PNG_1X1_B64).unwrap();
        v["icon"] = icon("image/png", &png);
        let m = parse_value(&v).unwrap();
        assert_eq!(m.icon.as_ref().unwrap().media_type, "image/png");
        assert_eq!(decode_icon(m.icon.as_ref().unwrap()).unwrap(), png);

        // Manifests without icons keep serializing without the key.
        let m = parse_value(&sample()).unwrap();
        assert!(serde_json::to_value(&m).unwrap().get("icon").is_none());
        // Unknown icon keys are rejected (deny_unknown_fields).
        let mut unknown = sample();
        unknown["icon"] = serde_json::json!({
            "mediaType": "image/svg+xml", "data": B64.encode(tiny_svg()), "colour": "red"
        });
        assert!(parse_value(&unknown).is_err());
    }

    #[test]
    fn invalid_icons_are_rejected() {
        let png = B64.decode(PNG_1X1_B64).unwrap();
        let cases: Vec<(&str, serde_json::Value, &str)> = vec![
            (
                "unknown media type",
                icon("image/gif", tiny_svg().as_bytes()),
                "mediaType",
            ),
            (
                "bad base64",
                serde_json::json!({"mediaType": "image/svg+xml", "data": "not base64!"}),
                "base64",
            ),
            (
                "base64 with whitespace",
                serde_json::json!({"mediaType": "image/svg+xml", "data": "PHN2 Zz48L3N2Zz4="}),
                "base64",
            ),
            (
                "too large",
                icon("image/png", &vec![0u8; MAX_ICON_BYTES + 1]),
                "64 KiB",
            ),
            ("empty", icon("image/png", &[]), "1 byte"),
            (
                "bad PNG signature",
                icon("image/png", tiny_svg().as_bytes()),
                "PNG",
            ),
            (
                "non-UTF-8 SVG",
                icon("image/svg+xml", &[0xff, 0xfe]),
                "UTF-8",
            ),
            (
                "script SVG",
                icon("image/svg+xml", b"<svg><script>alert(1)</script></svg>"),
                "script",
            ),
            (
                "foreignObject SVG",
                icon(
                    "image/svg+xml",
                    b"<svg><foreignObject><body/></foreignObject></svg>",
                ),
                "foreignObject",
            ),
            (
                "javascript URL",
                icon(
                    "image/svg+xml",
                    b"<svg><a href=\"javascript:alert(1)\"><text>x</text></a></svg>",
                ),
                "javascript:",
            ),
            (
                "event handler",
                icon(
                    "image/svg+xml",
                    b"<svg onload=\"alert(1)\"><path d=\"M0 0\"/></svg>",
                ),
                "event handler",
            ),
            (
                "external href",
                icon(
                    "image/svg+xml",
                    b"<svg><image href=\"https://evil.example/a.png\"/></svg>",
                ),
                "external http(s)",
            ),
            (
                "external xlink href",
                icon(
                    "image/svg+xml",
                    b"<svg><image xlink:href='http://evil.example/a.png'/></svg>",
                ),
                "external http(s)",
            ),
            (
                "attribution too long",
                {
                    let mut icon = icon("image/svg+xml", tiny_svg().as_bytes());
                    icon["attribution"] = "x".repeat(MAX_ICON_ATTRIBUTION_CHARS + 1).into();
                    icon
                },
                "0-300 characters",
            ),
        ];
        for (name, icon_value, expected) in cases {
            let mut v = sample();
            v["icon"] = icon_value;
            let error = parse_value(&v).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }

        // An internal fragment reference is fine; a PNG may be any size up to
        // the bound (checked above with all-zero bytes that are large but not
        // valid PNGs only after the size check).
        let mut valid = sample();
        valid["icon"] = icon(
            "image/svg+xml",
            b"<svg xmlns:xlink=\"http://www.w3.org/1999/xlink\"><use xlink:href=\"#icon\"/></svg>",
        );
        parse_value(&valid).unwrap();
        assert_eq!(
            decode_icon(&PackageIcon {
                media_type: "image/png".into(),
                data: B64.encode(&png),
                attribution: None,
            })
            .unwrap()
            .len(),
            png.len()
        );
    }

    #[test]
    fn icon_files_are_loaded_through_the_same_rules() {
        let dir = tempfile::tempdir().unwrap();
        let svg_path = dir.path().join("python.svg");
        std::fs::write(&svg_path, tiny_svg()).unwrap();
        let icon = package_icon_from_file(&svg_path).unwrap();
        assert_eq!(icon.media_type, "image/svg+xml");
        assert!(icon.attribution.is_none());
        assert_eq!(decode_icon(&icon).unwrap(), tiny_svg().as_bytes());

        let png_path = dir.path().join("app.PNG");
        std::fs::write(&png_path, B64.decode(PNG_1X1_B64).unwrap()).unwrap();
        let icon = package_icon_from_file(&png_path).unwrap();
        assert_eq!(icon.media_type, "image/png");

        // Missing files, wrong extensions, oversized files and dangerous SVG
        // are clear errors that name the file.
        let missing = dir.path().join("missing.svg");
        let error = package_icon_from_file(&missing).unwrap_err();
        assert!(error.contains("missing.svg"), "{error}");

        let gif_path = dir.path().join("icon.gif");
        std::fs::write(&gif_path, b"GIF89a").unwrap();
        let error = package_icon_from_file(&gif_path).unwrap_err();
        assert!(error.contains(".svg or .png"), "{error}");

        let big_path = dir.path().join("big.svg");
        std::fs::write(&big_path, vec![b'x'; MAX_ICON_BYTES + 1]).unwrap();
        let error = package_icon_from_file(&big_path).unwrap_err();
        assert!(error.contains("larger than 64 KiB"), "{error}");

        let dangerous = dir.path().join("dangerous.svg");
        std::fs::write(&dangerous, b"<svg onload=\"alert(1)\"></svg>").unwrap();
        let error = package_icon_from_file(&dangerous).unwrap_err();
        assert!(error.contains("dangerous.svg"), "{error}");
        assert!(error.contains("event handler"), "{error}");
    }

    #[test]
    fn runtime_env_and_path_dirs_are_validated() {
        let mut v = sample();
        v["runtime"]["env"] = serde_json::json!({
            "JAVA_HOME": "{root}",
            "PROBE": "{root}/lib/tools",
        });
        v["runtime"]["pathDirs"] = serde_json::json!(["bin"]);
        let m = parse_value(&v).unwrap();
        assert_eq!(m.runtime.env["JAVA_HOME"], "{root}");
        assert_eq!(m.runtime.path_dirs, vec!["bin".to_string()]);

        // `.` is the prefix itself; other entries stay inside the package.
        v["runtime"]["pathDirs"] = serde_json::json!([".", "bin"]);
        parse_value(&v).unwrap();
        for bad in [
            serde_json::json!(["../bin"]),
            serde_json::json!(["/usr/bin"]),
            serde_json::json!(["bin/"]),
        ] {
            let mut value = v.clone();
            value["runtime"]["pathDirs"] = bad.clone();
            assert!(parse_value(&value).is_err(), "{bad} accepted");
        }
        for bad_env in [
            serde_json::json!({"lower": "{root}"}),
            serde_json::json!({"HOME": "/home/user"}),
            serde_json::json!({"H": "{root}/../etc"}),
            serde_json::json!({"H": "prefix"}),
        ] {
            let mut value = v.clone();
            value["runtime"]["env"] = bad_env.clone();
            assert!(parse_value(&value).is_err(), "{bad_env} accepted");
        }
    }
}
