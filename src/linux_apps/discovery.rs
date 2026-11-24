//! Applications installed outside the App Center: discovery and inventory.
//!
//! People install desktop applications in ways RAST does not manage: a
//! `.desktop` entry in an XDG directory, an AppImage dropped in
//! `~/Applications`, a portable binary in `~/.local/bin`, or an app an external
//! installer (for example a launcher that itself runs inside a managed RAST
//! profile) puts inside its own profile. This module finds those applications,
//! parses their desktop entries safely and records them in `discovered_apps`
//! (migration 15) so App Center can list, approve and launch them in the
//! person's shared Xpra session.
//!
//! Design rules:
//! * discovery never installs, updates or uninstalls anything;
//! * an `Exec` line is parsed into an argv array and executed without a shell —
//!   field codes are stripped, never expanded by a shell;
//! * a `.desktop` file is only used when it is a regular file (symlinks are
//!   refused), small enough, of type `Application`, has a usable `Name`, `Exec`
//!   and `TryExec`, and its program resolves to an executable file;
//! * a rescan updates rows in place (stable ids) and marks vanished entries
//!   `unavailable` — it never deletes inventory data;
//! * files the web server cannot read (identity homes are 0700 and owned by
//!   the app identity) are scanned by the privileged helper, which executes the
//!   scan **as the identity**;
//! * system-wide entries are recorded but only usable by ordinary accounts
//!   after an administrator approved them (`linux_discovery_system_apps`).

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{
    config::DiscoverySystemApps,
    db,
    error::{AppError, AppResult},
    events::EventBus,
};

/// Event: the discovered-application inventory changed. Every client reloads
/// the list it may see.
pub const DISCOVERED_EVENT: &str = "linuxapps.discovered";

/// Hard cap on entries one scan may record (a busy `/opt` must not flood the
/// inventory).
pub const MAX_SCAN_ENTRIES: usize = 512;
/// Largest desktop file the scan reads.
pub const MAX_DESKTOP_FILE_BYTES: u64 = 256 * 1024;
/// Directory depth of a bounded scan (roots are shallow by convention).
pub const MAX_SCAN_DEPTH: usize = 4;
/// Longest argv the parser accepts (desktop files are small).
pub const MAX_EXEC_ARGS: usize = 64;
/// Default `PATH` used when the scan agent runs without one.
pub const DEFAULT_SCAN_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin";

/// What kind of entry a scanned path belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The person's own XDG directories / `~/Applications`.
    User,
    /// System directories (`/usr/share/applications`, `/opt`, ...).
    System,
    /// Inside a managed RAST application profile (an external installer put it
    /// there), owned by the identity that runs that profile.
    ManagedProfile,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::ManagedProfile => "managed-profile",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "system" => Some(Self::System),
            "managed-profile" => Some(Self::ManagedProfile),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// `.desktop` parser
// ---------------------------------------------------------------------------

/// One parsed `[Desktop Entry]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    /// `Name[xx]` values by locale.
    pub localized_names: BTreeMap<String, String>,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    /// Parsed `Exec` argv (field codes stripped; never a shell string).
    pub exec: Vec<String>,
    /// The raw `Exec` value (diagnostics only, never executed).
    pub exec_raw: String,
    pub try_exec: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub terminal: bool,
    pub startup_wm_class: Option<String>,
    pub hidden: bool,
    pub no_display: bool,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub mime_type: Vec<String>,
    pub actions: Vec<String>,
    /// AppImage/application version (`X-AppImage-Version` or `Version` when the
    /// entry is clearly an application version).
    pub version: Option<String>,
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file. Returns `None` when
/// the file is not an application entry RAST can launch.
pub fn parse_desktop_entry(text: &str) -> Option<DesktopEntry> {
    let mut entry = DesktopEntry::default();
    let mut in_entry = false;
    let mut seen_entry = false;
    let mut app_version: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = &line[1..line.len() - 1] == "Desktop Entry";
            seen_entry |= in_entry;
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Type" => {
                // Only applications can be launched; links/dirs are skipped.
                if !value.is_empty() && value != "Application" {
                    return None;
                }
            }
            "Name" => {
                if !value.is_empty() && value.len() <= 256 {
                    entry.name = value.to_string();
                }
            }
            "GenericName" => entry.generic_name = non_empty(value),
            "Comment" => entry.comment = non_empty(value),
            "Exec" => {
                if entry.exec_raw.is_empty() {
                    entry.exec_raw = value.to_string();
                }
            }
            "TryExec" => entry.try_exec = non_empty(value),
            "Icon" => entry.icon = non_empty(value),
            "Categories" => entry.categories = list_value(value, 32),
            "MimeType" => entry.mime_type = list_value(value, 64),
            "Actions" => entry.actions = list_value(value, 32),
            "OnlyShowIn" => entry.only_show_in = list_value(value, 32),
            "NotShowIn" => entry.not_show_in = list_value(value, 32),
            "Terminal" => entry.terminal = value.eq_ignore_ascii_case("true"),
            "Hidden" => entry.hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => entry.no_display = value.eq_ignore_ascii_case("true"),
            "StartupWMClass" => entry.startup_wm_class = non_empty(value),
            "X-AppImage-Version" | "AppImage-Version" => app_version = non_empty(value),
            _ => {
                if let Some(locale) = key.strip_prefix("Name[") {
                    if let Some(locale) = locale.strip_suffix(']') {
                        if !locale.is_empty() && !value.is_empty() {
                            entry
                                .localized_names
                                .insert(locale.to_string(), value.to_string());
                        }
                    }
                }
            }
        }
    }
    if !seen_entry || entry.name.is_empty() || entry.hidden || entry.no_display {
        return None;
    }
    entry.exec = parse_exec(&entry.exec_raw);
    if entry.exec.is_empty() {
        return None;
    }
    entry.version = app_version;
    Some(entry)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn list_value(value: &str, max: usize) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .take(max)
        .map(str::to_string)
        .collect()
}

/// Pick the best localized `Name` for a locale (`de_DE.UTF-8` → `de_DE`, `de`).
pub fn localized_name<'a>(entry: &'a DesktopEntry, locale: &str) -> &'a str {
    let normal = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('-', "_");
    if !normal.is_empty() {
        if let Some(name) = entry.localized_names.get(&normal) {
            return name;
        }
        if let Some((language, _)) = normal.split_once('_') {
            if let Some(name) = entry.localized_names.get(language) {
                return name;
            }
        }
    }
    &entry.name
}

/// Parse an `Exec` value into argv: quote-aware splitting, backslash escapes,
/// then field codes are removed (`%f`, `%U`, `%i`, …; `%%` becomes a literal
/// `%`). The result is executed directly, never through a shell.
pub fn parse_exec(raw: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    started = true;
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
        if args.len() > MAX_EXEC_ARGS {
            return Vec::new();
        }
    }
    if started {
        args.push(current);
    }
    args.iter()
        .filter_map(|arg| strip_field_codes(arg))
        .filter(|arg| !arg.is_empty())
        .take(MAX_EXEC_ARGS)
        .collect()
}

/// Remove desktop-entry field codes from one argv token. `%%` is a literal
/// percent sign; every other single-letter code expands to nothing (the
/// placeholders are for file/URL arguments RAST does not pass yet). An
/// unsupported code (`%x`) invalidates the token rather than guessing.
fn strip_field_codes(arg: &str) -> Option<String> {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(code) if "fFuUidDnNvcmk".contains(code) => {}
            Some(_) => return None,
            None => return None,
        }
    }
    Some(out)
}

/// Resolve a program (absolute path or `PATH` name) to an executable regular
/// file. Symlinks are resolved (the *final* file must be executable).
pub fn resolve_program(program: &str, path_env: &str) -> Option<PathBuf> {
    if program.is_empty() || program.len() > 4096 || program.contains('\0') {
        return None;
    }
    if program.contains('/') {
        return executable_file(Path::new(program));
    }
    for dir in path_env.split(':').filter(|dir| !dir.is_empty()) {
        if let Some(path) = executable_file(&Path::new(dir).join(program)) {
            return Some(path);
        }
    }
    None
}

fn executable_file(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if !path.is_absolute() {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    Some(path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Scan entries (shared by the server-side scan and the identity scan agent)
// ---------------------------------------------------------------------------

/// One application found by a scan. Serialized over the helper protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanEntry {
    pub scope: String,
    pub source: String,
    pub desktop_path: String,
    pub desktop_id: Option<String>,
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    /// Parsed `Exec` argv (absolute program first).
    pub exec: Vec<String>,
    pub try_exec: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub terminal: bool,
    pub startup_wm_class: Option<String>,
    pub version: Option<String>,
    pub install_dir: Option<String>,
}

/// The identity scan agent's output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentScan {
    pub entries: Vec<ScanEntry>,
    #[serde(default)]
    pub skipped: usize,
}

/// One root to scan: a scope label plus its absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRoot {
    pub scope: Scope,
    pub path: PathBuf,
}

/// Parse one `"<scope>:<absolute path>"` scan root (helper protocol form).
pub fn parse_scan_root(raw: &str) -> Result<ScanRoot, String> {
    let Some((scope, path)) = raw.split_once(':') else {
        return Err(format!("invalid scan root {raw:?}: expected scope:path"));
    };
    let scope = Scope::parse(scope).ok_or_else(|| format!("invalid scan root scope in {raw:?}"))?;
    if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
        return Err(format!("invalid scan root path in {raw:?}"));
    }
    Ok(ScanRoot {
        scope,
        path: PathBuf::from(path),
    })
}

/// Entry point of the hidden per-identity `desktop-scan-agent` subcommand: walk
/// the given roots **as the current user** and write the parsed entries to
/// stdout as one JSON object. The privileged helper starts it with the
/// identity's uid/gid, so identity-owned directories are readable without the
/// web server ever needing root.
pub fn run_scan_agent(roots: &[String], expected_build: Option<&str>) -> Result<(), String> {
    if let Some(expected) = expected_build {
        if expected != crate::executable::build_id() {
            return Err(format!(
                "this desktop scan agent is build {} but the helper expected {expected}; \
                 restart rast-native.service",
                crate::executable::build_id()
            ));
        }
    }
    if roots.len() > 64 {
        return Err("too many scan roots".to_string());
    }
    let mut parsed = Vec::with_capacity(roots.len());
    for raw in roots {
        parsed.push(parse_scan_root(raw)?);
    }
    let path_env = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_SCAN_PATH.to_string());
    let scan = scan_roots(&parsed, &path_env);
    let text = serde_json::to_string(&scan)
        .map_err(|error| format!("cannot encode the scan result: {error}"))?;
    println!("{text}");
    Ok(())
}

/// Walk `roots` and return launchable applications. Runs as whichever user
/// calls it (the server directly for system roots, the identity agent for
/// identity-owned roots), so it only reads what that user may read.
pub fn scan_roots(roots: &[ScanRoot], path_env: &str) -> AgentScan {
    let mut scan = AgentScan::default();
    let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut desktop_execs: BTreeSet<String> = BTreeSet::new();
    let mut appimages: Vec<(ScanRoot, PathBuf, String)> = Vec::new();
    for root in roots {
        if !root.path.is_dir() || !root.path.is_absolute() {
            continue;
        }
        let mut stack = vec![(root.path.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > MAX_SCAN_DEPTH || scan.entries.len() >= MAX_SCAN_ENTRIES {
                continue;
            }
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for item in read.flatten() {
                if scan.entries.len() >= MAX_SCAN_ENTRIES {
                    break;
                }
                let path = item.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.file_type().is_symlink() {
                    continue;
                }
                let name = item.file_name().to_string_lossy().into_owned();
                if metadata.is_dir() {
                    if depth < MAX_SCAN_DEPTH && !name.starts_with('.') {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if !metadata.is_file() || !seen_paths.insert(path.clone()) {
                    continue;
                }
                if name.ends_with(".desktop") {
                    match scan_desktop_file(root.scope, &path, path_env) {
                        Some(entry) => {
                            if let Some(program) = entry.exec.first() {
                                desktop_execs.insert(program.clone());
                            }
                            scan.entries.push(entry);
                        }
                        None => scan.skipped += 1,
                    }
                    continue;
                }
                if name.to_ascii_lowercase().ends_with(".appimage") {
                    appimages.push((root.clone(), path, name));
                    continue;
                }
                // A portable program: an executable file in an application
                // directory (`~/Applications`, `~/.local/bin`). Only accepted
                // when the file really is an ELF binary.
                if portable_dir(&root.path) && is_executable(&metadata) && is_elf(&path) {
                    let stem = Path::new(&name)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| name.clone());
                    scan.entries.push(ScanEntry {
                        scope: root.scope.as_str().to_string(),
                        source: "portable".to_string(),
                        desktop_path: path.display().to_string(),
                        desktop_id: None,
                        name: stem,
                        generic_name: None,
                        comment: None,
                        exec: vec![path.display().to_string()],
                        try_exec: None,
                        icon: None,
                        categories: Vec::new(),
                        terminal: false,
                        startup_wm_class: None,
                        version: None,
                        install_dir: path.parent().map(|parent| parent.display().to_string()),
                    });
                }
            }
        }
    }
    // AppImages become entries unless a desktop entry in the same scan already
    // launches exactly that file (avoids a duplicate inventory row).
    for (root, path, name) in appimages {
        if scan.entries.len() >= MAX_SCAN_ENTRIES {
            break;
        }
        let program = path.display().to_string();
        if desktop_execs.contains(&program) {
            continue;
        }
        if !is_executable_path(&path) {
            scan.skipped += 1;
            continue;
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or(name);
        let source = if root.scope == Scope::ManagedProfile {
            "external-installer"
        } else {
            "appimage"
        };
        scan.entries.push(ScanEntry {
            scope: root.scope.as_str().to_string(),
            source: source.to_string(),
            desktop_path: program.clone(),
            desktop_id: None,
            name: stem,
            generic_name: None,
            comment: None,
            exec: vec![program],
            try_exec: None,
            icon: None,
            categories: Vec::new(),
            terminal: false,
            startup_wm_class: None,
            version: None,
            install_dir: path.parent().map(|parent| parent.display().to_string()),
        });
    }
    scan
}

fn portable_dir(root: &Path) -> bool {
    root.file_name()
        .map(|name| name == "Applications" || name == ".local" || name == "bin")
        .unwrap_or(false)
}

fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

fn is_executable_path(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && is_executable(&metadata))
        .unwrap_or(false)
}

fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == [0x7f, b'E', b'L', b'F']
}

fn scan_desktop_file(scope: Scope, path: &Path, path_env: &str) -> Option<ScanEntry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_DESKTOP_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let entry = parse_desktop_entry(&text)?;
    // TryExec must resolve or the application cannot be started.
    if let Some(try_exec) = &entry.try_exec {
        resolve_program(try_exec, path_env)?;
    }
    let program = entry.exec.first()?;
    let program = resolve_program(program, path_env)?;
    let mut exec = entry.exec.clone();
    exec[0] = program.display().to_string();
    let source = if scope == Scope::ManagedProfile {
        "external-installer"
    } else {
        "xdg"
    };
    Some(ScanEntry {
        scope: scope.as_str().to_string(),
        source: source.to_string(),
        desktop_path: path.display().to_string(),
        desktop_id: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        name: entry.name.clone(),
        generic_name: entry.generic_name.clone(),
        comment: entry.comment.clone(),
        exec,
        try_exec: entry.try_exec.clone(),
        icon: entry.icon.clone(),
        categories: entry.categories.clone(),
        terminal: entry.terminal,
        startup_wm_class: entry.startup_wm_class.clone(),
        version: entry.version.clone(),
        install_dir: path.parent().map(|parent| parent.display().to_string()),
    })
}

// ---------------------------------------------------------------------------
// Identity scan helper seam
// ---------------------------------------------------------------------------

/// The privileged part: run the scan as the identity because identity homes
/// are 0700 and the unprivileged server cannot read them.
#[async_trait]
pub trait DiscoveryHelper: Send + Sync + 'static {
    /// `roots` are `"<scope>:<absolute path>"` strings the helper re-validates.
    async fn desktop_scan(
        &self,
        user_id: i64,
        roots: &[String],
    ) -> Result<Value, crate::linux_apps::native_session::HelperError>;
}

/// Production adapter over [`crate::native::client::NativeClient`].
pub struct ClientDiscoveryHelper(pub crate::native::client::NativeClient);

#[async_trait]
impl DiscoveryHelper for ClientDiscoveryHelper {
    async fn desktop_scan(
        &self,
        user_id: i64,
        roots: &[String],
    ) -> Result<Value, crate::linux_apps::native_session::HelperError> {
        self.0.desktop_scan(user_id, roots).await.map_err(|error| {
            crate::linux_apps::native_session::HelperError {
                code: error.code,
                message: error.message,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Registry (migration 15)
// ---------------------------------------------------------------------------

/// One `discovered_apps` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DiscoveredRow {
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub exec_json: String,
    pub try_exec: Option<String>,
    pub icon: Option<String>,
    pub categories: Option<String>,
    pub terminal: i64,
    pub startup_wm_class: Option<String>,
    pub desktop_path: String,
    pub desktop_id: Option<String>,
    pub owner_user_id: Option<i64>,
    pub scope: String,
    pub source: String,
    pub install_dir: Option<String>,
    pub version: Option<String>,
    pub status: String,
    pub approved: i64,
    pub hidden: i64,
    pub last_seen_ms: i64,
    pub last_launched_ms: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl DiscoveredRow {
    pub fn exec(&self) -> Vec<String> {
        serde_json::from_str(&self.exec_json).unwrap_or_default()
    }

    pub fn categories(&self) -> Vec<String> {
        self.categories
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default()
    }

    /// Administrators see every entry; people see their own and approved
    /// system entries.
    pub fn visible_to(&self, user_id: i64, is_admin: bool) -> bool {
        if is_admin {
            return true;
        }
        if self.owner_user_id == Some(user_id) {
            return self.hidden == 0;
        }
        self.scope == "system" && self.approved == 1 && self.hidden == 0
    }

    /// Whether this entry may be started by the given person right now.
    pub fn launchable_for(&self, user_id: i64, is_admin: bool) -> bool {
        self.visible_to(user_id, is_admin) && self.status == "ready" && !self.exec().is_empty()
    }

    pub fn json(&self, user_id: i64, is_admin: bool) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "genericName": self.generic_name,
            "comment": self.comment,
            "exec": self.exec(),
            "tryExec": self.try_exec,
            "icon": self.icon,
            "categories": self.categories(),
            "terminal": self.terminal == 1,
            "startupWmClass": self.startup_wm_class,
            "desktopPath": self.desktop_path,
            "desktopId": self.desktop_id,
            "ownerUserId": self.owner_user_id,
            "scope": self.scope,
            "source": self.source,
            "installDir": self.install_dir,
            "version": self.version,
            "status": self.status,
            "approved": self.approved == 1,
            "hidden": self.hidden == 1,
            "lastSeenMs": self.last_seen_ms,
            "lastLaunchedMs": self.last_launched_ms,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "canLaunch": self.launchable_for(user_id, is_admin),
        })
    }
}

/// Stable id of one discovered path: `d-` + 16 hex chars of a SHA-256 over
/// (scope, owner, path). Fits the native app id grammar (≤ 40 chars).
pub fn stable_id(scope: Scope, owner_user_id: Option<i64>, desktop_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.as_str().as_bytes());
    hasher.update(b"|");
    match owner_user_id {
        Some(owner) => hasher.update(owner.to_string().as_bytes()),
        None => hasher.update(b"-"),
    }
    hasher.update(b"|");
    hasher.update(desktop_path.as_bytes());
    let digest = hasher.finalize();
    let mut id = String::from("d-");
    for byte in digest.iter().take(8) {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Status an entry gets from its scope and policy (`approved`/`hidden`).
fn computed_status(scope: Scope, policy: DiscoverySystemApps) -> (&'static str, i64, i64) {
    match scope {
        Scope::System => match policy {
            DiscoverySystemApps::Approval => ("pending_approval", 0, 0),
            DiscoverySystemApps::Auto => ("ready", 1, 0),
            DiscoverySystemApps::Hidden => ("discovered", 0, 1),
        },
        Scope::User | Scope::ManagedProfile => ("ready", 1, 0),
    }
}

/// Insert or refresh one scanned batch. Returns the number of entries written.
/// Rows that were previously known for this owner/scope but are missing from
/// `entries` become `unavailable`; nothing is deleted.
pub async fn reconcile(
    pool: &SqlitePool,
    scope: Scope,
    owner_user_id: Option<i64>,
    policy: DiscoverySystemApps,
    entries: &[ScanEntry],
) -> AppResult<usize> {
    let now = db::now_ms();
    let (status, approved, hidden) = computed_status(scope, policy);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut written = 0usize;
    for entry in entries.iter().take(MAX_SCAN_ENTRIES) {
        if entry.scope != scope.as_str() {
            continue;
        }
        let entry_scope = Scope::parse(&entry.scope).unwrap_or(scope);
        let id = stable_id(entry_scope, owner_user_id, &entry.desktop_path);
        seen.insert(id.clone());
        let name = if entry.name.trim().is_empty() {
            Path::new(&entry.desktop_path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Application".to_string())
        } else {
            entry.name.clone()
        };
        let exec_json = serde_json::to_string(&entry.exec).unwrap_or_else(|_| "[]".to_string());
        let categories =
            serde_json::to_string(&entry.categories).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO discovered_apps (
                 id, name, generic_name, comment, exec_json, try_exec, icon, categories,
                 terminal, startup_wm_class, desktop_path, desktop_id, owner_user_id, scope,
                 source, install_dir, version, status, approved, hidden, last_seen_ms,
                 created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 generic_name = excluded.generic_name,
                 comment = excluded.comment,
                 exec_json = excluded.exec_json,
                 try_exec = excluded.try_exec,
                 icon = excluded.icon,
                 categories = excluded.categories,
                 terminal = excluded.terminal,
                 startup_wm_class = excluded.startup_wm_class,
                 desktop_id = excluded.desktop_id,
                 source = excluded.source,
                 install_dir = excluded.install_dir,
                 version = excluded.version,
                 last_seen_ms = excluded.last_seen_ms,
                 updated_at = excluded.updated_at,
                 -- Preserve an administrator's approval/hide decision.
                 status = CASE
                     WHEN discovered_apps.scope = 'system' AND discovered_apps.hidden = 1
                         THEN 'discovered'
                     WHEN discovered_apps.scope = 'system' AND discovered_apps.approved = 1
                         THEN 'ready'
                     ELSE excluded.status
                 END",
        )
        .bind(&id)
        .bind(&name)
        .bind(&entry.generic_name)
        .bind(&entry.comment)
        .bind(&exec_json)
        .bind(&entry.try_exec)
        .bind(&entry.icon)
        .bind(&categories)
        .bind(i64::from(entry.terminal))
        .bind(&entry.startup_wm_class)
        .bind(&entry.desktop_path)
        .bind(&entry.desktop_id)
        .bind(owner_user_id)
        .bind(scope.as_str())
        .bind(&entry.source)
        .bind(&entry.install_dir)
        .bind(&entry.version)
        .bind(status)
        .bind(approved)
        .bind(hidden)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
        written += 1;
    }
    // Previously discovered rows that the scan did not see. Manual imports are
    // not part of a scan and keep their state.
    let previous: Vec<(String,)> = match owner_user_id {
        Some(owner) => {
            sqlx::query_as(
                "SELECT id FROM discovered_apps
                 WHERE scope = ? AND owner_user_id = ? AND status != 'unavailable'
                   AND source != 'manual'",
            )
            .bind(scope.as_str())
            .bind(owner)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT id FROM discovered_apps
                 WHERE scope = ? AND owner_user_id IS NULL AND status != 'unavailable'
                   AND source != 'manual'",
            )
            .bind(scope.as_str())
            .fetch_all(pool)
            .await?
        }
    };
    for (id,) in previous {
        if seen.contains(&id) {
            continue;
        }
        sqlx::query(
            "UPDATE discovered_apps SET status = 'unavailable', updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&id)
        .execute(pool)
        .await?;
    }
    Ok(written)
}

/// Which entries one account may see (administrators: everything).
pub async fn list(
    pool: &SqlitePool,
    user_id: i64,
    is_admin: bool,
) -> AppResult<Vec<DiscoveredRow>> {
    let rows: Vec<DiscoveredRow> = if is_admin {
        sqlx::query_as(
            "SELECT * FROM discovered_apps
             ORDER BY hidden ASC, scope ASC, name COLLATE NOCASE ASC, id ASC",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT * FROM discovered_apps
             WHERE (owner_user_id = ?1 AND hidden = 0)
                OR (scope = 'system' AND approved = 1 AND hidden = 0)
             ORDER BY scope ASC, name COLLATE NOCASE ASC, id ASC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

/// One row by id.
pub async fn get(pool: &SqlitePool, id: &str) -> AppResult<DiscoveredRow> {
    sqlx::query_as("SELECT * FROM discovered_apps WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound("Discovered application not found"))
}

/// Administrator action: approve/hide/rename. `None` leaves a field unchanged.
pub async fn patch(
    pool: &SqlitePool,
    id: &str,
    approved: Option<bool>,
    hidden: Option<bool>,
    name: Option<&str>,
) -> AppResult<DiscoveredRow> {
    let mut row = get(pool, id).await?;
    if let Some(approved) = approved {
        row.approved = i64::from(approved);
    }
    if let Some(hidden) = hidden {
        row.hidden = i64::from(hidden);
    }
    if let Some(name) = name {
        let name = name.trim();
        if name.is_empty() || name.len() > 256 {
            return Err(AppError::bad_request("name must be 1-256 characters"));
        }
        row.name = name.to_string();
    }
    if row.scope == "system" {
        row.status = if row.hidden == 1 {
            "discovered".to_string()
        } else if row.approved == 1 {
            "ready".to_string()
        } else {
            "pending_approval".to_string()
        };
    }
    sqlx::query(
        "UPDATE discovered_apps SET name = ?, status = ?, approved = ?, hidden = ?,
             updated_at = ? WHERE id = ?",
    )
    .bind(&row.name)
    .bind(&row.status)
    .bind(row.approved)
    .bind(row.hidden)
    .bind(db::now_ms())
    .bind(&row.id)
    .execute(pool)
    .await?;
    Ok(row)
}

/// Administrator action: forget the registry entry (never touches files).
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<()> {
    let result = sqlx::query("DELETE FROM discovered_apps WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Discovered application not found"));
    }
    Ok(())
}

/// Administrator action: register an existing executable by hand (no
/// installer runs). The file is not modified; only the inventory row is added.
pub async fn import(
    pool: &SqlitePool,
    path: &str,
    name: &str,
    owner_user_id: Option<i64>,
) -> AppResult<DiscoveredRow> {
    let path = path.trim();
    if path.is_empty() || !Path::new(path).is_absolute() || path.contains('\0') {
        return Err(AppError::bad_request("path must be an absolute file path"));
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        // An identity-owned path may be unreadable for the server; the launch
        // itself runs as the identity and will report a real error then.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => None,
        Err(error) => {
            return Err(AppError::bad_request(format!(
                "cannot read {path}: {error}"
            )))
        }
    };
    if let Some(metadata) = metadata {
        use std::os::unix::fs::PermissionsExt;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(AppError::bad_request(
                "the imported path must be an executable regular file",
            ));
        }
    }
    let name = name.trim();
    let name = if name.is_empty() {
        Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Application".to_string())
    } else {
        name.to_string()
    };
    if name.len() > 256 {
        return Err(AppError::bad_request("name must be 1-256 characters"));
    }
    let scope = if owner_user_id.is_some() {
        Scope::User
    } else {
        Scope::System
    };
    let id = stable_id(scope, owner_user_id, path);
    let now = db::now_ms();
    let exec = serde_json::to_string(&vec![path.to_string()]).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "INSERT INTO discovered_apps (
             id, name, exec_json, desktop_path, owner_user_id, scope, source, status,
             approved, hidden, install_dir, last_seen_ms, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 'manual', 'ready', 1, 0, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name, exec_json = excluded.exec_json, scope = excluded.scope,
             owner_user_id = excluded.owner_user_id, status = 'ready', approved = 1,
             hidden = 0, last_seen_ms = excluded.last_seen_ms,
             updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(&name)
    .bind(&exec)
    .bind(path)
    .bind(owner_user_id)
    .bind(scope.as_str())
    .bind(
        Path::new(path)
            .parent()
            .map(|parent| parent.display().to_string()),
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    get(pool, &id).await
}

/// Record that an entry was just launched.
pub async fn mark_launched(pool: &SqlitePool, id: &str) {
    let _ =
        sqlx::query("UPDATE discovered_apps SET last_launched_ms = ?, updated_at = ? WHERE id = ?")
            .bind(db::now_ms())
            .bind(db::now_ms())
            .bind(id)
            .execute(pool)
            .await;
}

/// Simple per-user rescan throttle (the scan spawns an identity process).
pub struct ScanThrottle {
    last: std::sync::Mutex<BTreeMap<i64, Instant>>,
    interval: std::time::Duration,
}

impl ScanThrottle {
    pub fn new(interval: std::time::Duration) -> Self {
        Self {
            last: std::sync::Mutex::new(BTreeMap::new()),
            interval,
        }
    }

    /// `Err` with seconds-to-wait when the last scan was too recent.
    pub fn admit(&self, user_id: i64) -> Result<(), u64> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = last.get(&user_id) {
            let elapsed = at.elapsed();
            if elapsed < self.interval {
                return Err((self.interval - elapsed).as_secs().max(1));
            }
        }
        last.insert(user_id, Instant::now());
        Ok(())
    }
}

/// Publish the inventory-changed event (scoped: owner and admins for user
/// entries, everyone for system entries).
pub fn publish_changed(events: &EventBus, owner_user_id: Option<i64>) {
    match owner_user_id {
        Some(owner) => {
            events.publish_user_and_admin(DISCOVERED_EVENT, owner, json!({"scope": "user"}));
        }
        None => {
            events.publish(DISCOVERED_EVENT, json!({"scope": "system"}));
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn user(id: i64) -> crate::auth::AuthUser {
        crate::auth::AuthUser {
            id,
            username: format!("user{id}"),
            display_name: format!("User {id}"),
            role: crate::auth::Role::User,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    fn desktop(body: &str) -> String {
        format!("[Desktop Entry]\n{body}\n")
    }

    #[test]
    fn parses_a_normal_desktop_entry() {
        let entry = parse_desktop_entry(&desktop(
            "Type=Application\nName=WordCraft\nGenericName=Word Processor\nComment=Write\n\
             Exec=\"/opt/wordcraft/wordcraft\" %F\nIcon=wordcraft\nCategories=Office;WordProcessor;\n\
             Terminal=false\nStartupWMClass=ai.storyteller.wordcraft\nMimeType=text/plain;\n\
             X-AppImage-Version=0.18.1",
        ))
        .expect("entry");
        assert_eq!(entry.name, "WordCraft");
        assert_eq!(entry.generic_name.as_deref(), Some("Word Processor"));
        assert_eq!(entry.exec, vec!["/opt/wordcraft/wordcraft"]);
        assert_eq!(entry.categories, vec!["Office", "WordProcessor"]);
        assert_eq!(
            entry.startup_wm_class.as_deref(),
            Some("ai.storyteller.wordcraft")
        );
        assert_eq!(entry.version.as_deref(), Some("0.18.1"));
    }

    #[test]
    fn quoted_exec_with_spaces_and_field_codes() {
        let entry = parse_desktop_entry(&desktop(
            "Name=App\nExec=\"/opt/My App/bin/app\" --flag \"%U\" %f %i %%literal %x",
        ));
        // `%x` is unsupported: its token is dropped, the rest survives.
        let entry = entry.expect("entry");
        assert_eq!(
            entry.exec,
            vec!["/opt/My App/bin/app", "--flag", "%literal"]
        );
    }

    #[test]
    fn exec_splitter_handles_quotes_escapes_and_placeholders() {
        assert_eq!(parse_exec("app %F"), vec!["app"]);
        assert_eq!(parse_exec("\"a b\" c"), vec!["a b", "c"]);
        assert_eq!(parse_exec("app \\\"x\\\""), vec!["app", "\"x\""]);
        assert_eq!(parse_exec("app 100%%"), vec!["app", "100%"]);
        assert_eq!(parse_exec(""), Vec::<String>::new());
        assert_eq!(parse_exec("\"\""), Vec::<String>::new());
    }

    #[test]
    fn hidden_and_nodisplay_entries_are_not_applications() {
        assert!(parse_desktop_entry(&desktop("Name=A\nExec=/bin/true\nHidden=true")).is_none());
        assert!(parse_desktop_entry(&desktop("Name=A\nExec=/bin/true\nNoDisplay=true")).is_none());
        assert!(parse_desktop_entry(&desktop("Type=Link\nName=A\nURL=https://x")).is_none());
    }

    #[test]
    fn localized_names_are_parsed_and_selected() {
        let entry = parse_desktop_entry(&desktop(
            "Name=Files\nName[de]=Dateien\nName[pt_BR]=Arquivos\nExec=/bin/true",
        ))
        .expect("entry");
        assert_eq!(localized_name(&entry, "de_DE.UTF-8"), "Dateien");
        assert_eq!(localized_name(&entry, "pt_BR"), "Arquivos");
        assert_eq!(localized_name(&entry, "fr"), "Files");
    }

    #[test]
    fn program_resolution_refuses_relative_paths_and_missing_tryexec() {
        assert!(resolve_program("relative/app", DEFAULT_SCAN_PATH).is_none());
        assert!(resolve_program("/definitely/missing/app", DEFAULT_SCAN_PATH).is_none());
        // A desktop entry whose TryExec is missing is skipped by the scan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("x.desktop"),
            desktop("Name=X\nExec=/bin/true\nTryExec=definitely-not-installed"),
        )
        .unwrap();
        let scan = scan_roots(
            &[ScanRoot {
                scope: Scope::System,
                path: dir.path().to_path_buf(),
            }],
            DEFAULT_SCAN_PATH,
        );
        assert!(scan.entries.is_empty());
        assert_eq!(scan.skipped, 1);
    }

    #[test]
    fn scan_finds_desktop_files_appimages_and_portable_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        std::fs::create_dir_all(&apps).unwrap();
        // A real ELF executable: /bin/true copied as a portable program.
        let portable = apps.join("mytool");
        std::fs::copy("/bin/true", &portable).unwrap();
        std::fs::set_permissions(&portable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let appimage = apps.join("thing.AppImage");
        std::fs::copy("/bin/true", &appimage).unwrap();
        std::fs::set_permissions(&appimage, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            apps.join("thing.desktop"),
            desktop("Name=Thing\nExec=\"/bin/true\" %U\nStartupWMClass=thing"),
        )
        .unwrap();
        let scan = scan_roots(
            &[
                ScanRoot {
                    scope: Scope::User,
                    path: apps.clone(),
                },
                ScanRoot {
                    scope: Scope::System,
                    path: dir.path().to_path_buf(),
                },
            ],
            DEFAULT_SCAN_PATH,
        );
        let names: Vec<&str> = scan
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert!(names.contains(&"Thing"), "{names:?}");
        assert!(names.contains(&"mytool"), "{names:?}");
        // The AppImage is covered by the desktop entry's Exec? No: that entry
        // runs /bin/true, so the AppImage stays a separate entry.
        assert!(names.contains(&"thing"), "{names:?}");
    }

    #[test]
    fn stable_ids_are_stable_and_scoped() {
        let a = stable_id(Scope::User, Some(7), "/home/x/A.desktop");
        assert_eq!(a, stable_id(Scope::User, Some(7), "/home/x/A.desktop"));
        assert_ne!(a, stable_id(Scope::User, Some(8), "/home/x/A.desktop"));
        assert_ne!(a, stable_id(Scope::System, None, "/home/x/A.desktop"));
        assert!(a.starts_with("d-") && a.len() <= 40);
        assert!(a[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::connect(&dir.join("discovery.db"), 2)
            .await
            .unwrap()
    }

    /// Owner ids in `discovered_apps` reference `users(id)`.
    async fn ensure_user(pool: &SqlitePool, id: i64) {
        sqlx::query(
            "INSERT OR IGNORE INTO users
                 (id, username, password_hash, role, created_at, updated_at)
             VALUES (?, ?, 'x', 'user', 0, 0)",
        )
        .bind(id)
        .bind(format!("user{id}"))
        .execute(pool)
        .await
        .unwrap();
    }

    fn entry(path: &str, name: &str) -> ScanEntry {
        ScanEntry {
            scope: "user".into(),
            source: "xdg".into(),
            desktop_path: path.into(),
            desktop_id: Some("app.desktop".into()),
            name: name.into(),
            generic_name: None,
            comment: None,
            exec: vec!["/bin/true".into()],
            try_exec: None,
            icon: None,
            categories: vec![],
            terminal: false,
            startup_wm_class: Some("app".into()),
            version: None,
            install_dir: None,
        }
    }

    #[tokio::test]
    async fn reconciliation_inserts_updates_and_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        ensure_user(&pool, 5).await;
        let mut first = entry("/home/u/Applications/a.desktop", "A");
        first.scope = "user".into();
        let count = reconcile(
            &pool,
            Scope::User,
            Some(5),
            DiscoverySystemApps::Approval,
            &[first.clone()],
        )
        .await
        .unwrap();
        assert_eq!(count, 1);
        let rows = list(&pool, 5, false).await.unwrap();
        assert_eq!(rows.len(), 1);
        let id = rows[0].id.clone();
        assert_eq!(rows[0].name, "A");
        assert_eq!(rows[0].status, "ready"); // the owner's own entry needs no approval

        // Rename in a rescan: same id, new name.
        let mut renamed = first.clone();
        renamed.name = "A renamed".into();
        reconcile(
            &pool,
            Scope::User,
            Some(5),
            DiscoverySystemApps::Approval,
            &[renamed],
        )
        .await
        .unwrap();
        let rows = list(&pool, 5, false).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].name, "A renamed");

        // A rescan that no longer sees it marks it unavailable but keeps it.
        reconcile(
            &pool,
            Scope::User,
            Some(5),
            DiscoverySystemApps::Approval,
            &[],
        )
        .await
        .unwrap();
        let rows = list(&pool, 5, false).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "unavailable");
    }

    #[tokio::test]
    async fn system_entries_need_approval_and_are_admin_only_until_then() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        ensure_user(&pool, 5).await;
        ensure_user(&pool, 6).await;
        let mut system = entry("/usr/share/applications/x.desktop", "System App");
        system.scope = "system".into();
        reconcile(
            &pool,
            Scope::System,
            None,
            DiscoverySystemApps::Approval,
            &[system],
        )
        .await
        .unwrap();
        assert!(list(&pool, 5, false).await.unwrap().is_empty());
        let admin_rows = list(&pool, 1, true).await.unwrap();
        assert_eq!(admin_rows.len(), 1);
        assert_eq!(admin_rows[0].status, "pending_approval");
        assert!(!admin_rows[0].launchable_for(5, false));

        let approved = patch(&pool, &admin_rows[0].id, Some(true), None, None)
            .await
            .unwrap();
        assert_eq!(approved.status, "ready");
        let visible = list(&pool, 5, false).await.unwrap();
        assert_eq!(visible.len(), 1);
        assert!(visible[0].launchable_for(5, false));
        // Others' user entries are never visible to an unrelated account.
        let mut other = entry("/home/other/Applications/o.desktop", "O");
        other.scope = "user".into();
        reconcile(
            &pool,
            Scope::User,
            Some(6),
            DiscoverySystemApps::Approval,
            &[other],
        )
        .await
        .unwrap();
        assert_eq!(list(&pool, 5, false).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_and_hidden_system_policies() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let mut system = entry("/usr/share/applications/y.desktop", "Auto App");
        system.scope = "system".into();
        reconcile(
            &pool,
            Scope::System,
            None,
            DiscoverySystemApps::Auto,
            &[system.clone()],
        )
        .await
        .unwrap();
        assert_eq!(list(&pool, 5, false).await.unwrap().len(), 1);

        let mut hidden = entry("/usr/share/applications/z.desktop", "Hidden App");
        hidden.scope = "system".into();
        let id = stable_id(Scope::System, None, &hidden.desktop_path);
        reconcile(
            &pool,
            Scope::System,
            None,
            DiscoverySystemApps::Hidden,
            &[hidden],
        )
        .await
        .unwrap();
        assert!(list(&pool, 5, false)
            .await
            .unwrap()
            .iter()
            .all(|row| row.id != id));
        assert!(list(&pool, 1, true)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == id));
    }

    #[tokio::test]
    async fn manual_import_registers_a_launchable_entry() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let binary = dir.path().join("tool");
        std::fs::copy("/bin/true", &binary).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let row = import(&pool, &binary.display().to_string(), "My Tool", None)
            .await
            .unwrap();
        assert_eq!(row.name, "My Tool");
        assert_eq!(row.source, "manual");
        assert_eq!(row.status, "ready");
        assert!(row.launchable_for(5, false));
        assert_eq!(row.exec(), vec![binary.display().to_string()]);
        // A rescan must not mark manual entries unavailable.
        reconcile(
            &pool,
            Scope::System,
            None,
            DiscoverySystemApps::Approval,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(get(&pool, &row.id).await.unwrap().status, "ready");
    }

    #[test]
    fn user_struct_is_used_for_visibility_expectations() {
        // Keep the AuthUser helper honest: it is what the API passes in tests.
        assert!(!user(5).is_admin());
    }
}
