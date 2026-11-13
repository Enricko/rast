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
//! The JSON Schema for authors lives in `docs/rast-package.schema.json`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::version::{self, Version};

pub const SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_KIND: &str = "rast-package";
pub const MANIFEST_FILE: &str = "rast-package.json";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Host targets RAST knows about (`<os>-<arch>`).
pub const KNOWN_TARGETS: &[&str] = &["linux-x86_64", "linux-aarch64"];
/// Package types this RAST can install.
pub const SUPPORTED_TYPES: &[&str] = &["runtime", "app"];
/// Package types reserved for future releases (rejected with a specific note).
pub const PLANNED_TYPES: &[&str] = &["service"];
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Requirements {
    /// Minimum glibc (`2.28`). Absent = no glibc requirement (static builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glibc: Option<String>,
    /// Free-text host dependency notes shown to administrators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Presentation for app-type packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppSpec>,
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

/// How an app-type package is presented in App Center.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppSpec {
    /// `command` (run an executable in a window) or `web` (serve files from
    /// the package's `app/` directory in a window).
    pub kind: String,
    /// Entry page for `web` apps, relative to `app/` (default index.html).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// Archive format; `tar.gz` in schema version 1.
    pub format: String,
    #[serde(default)]
    pub strip_components: u32,
    #[serde(default)]
    pub requires: Requirements,
    pub provenance: Provenance,
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
    pub runtime: RuntimeSpec,
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Presentation for app-type packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppSpec>,
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

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
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
    if let Some(app) = &m.app {
        if m.package_type != "app" {
            return Err("app metadata is only allowed on app-type packages".into());
        }
        match app.kind.as_str() {
            "command" => {}
            "web" => {
                let entry = app.entry.as_deref().unwrap_or("index.html");
                if !is_safe_relative_path(entry) {
                    return Err("app.entry must be a safe path inside the package".into());
                }
            }
            other => return Err(format!("unknown app kind {other:?}")),
        }
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
    validate_runtime(&m.runtime)?;
    validate_artifacts(&m.artifacts)?;
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
    Ok(())
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
        if !is_https_url(&a.url) {
            return Err(format!("artifact URL for {} must be https://", a.target));
        }
        if !is_sha256(&a.sha256) {
            return Err(format!(
                "artifact sha256 for {} must be 64 lowercase hex characters",
                a.target
            ));
        }
        if a.size == 0 || a.size > MAX_ARTIFACT_BYTES {
            return Err(format!("artifact size for {} is out of range", a.target));
        }
        if a.format != "tar.gz" {
            return Err(format!(
                "artifact format {:?} is not supported (schema version 1: tar.gz)",
                a.format
            ));
        }
        if a.strip_components > 4 {
            return Err("stripComponents must be 0-4".into());
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
                Box::new(|v| v["artifacts"][0]["format"] = "zip".into()),
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
}
