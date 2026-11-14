//! Safe handling of prebuilt Linux application archives and AppImages.
//!
//! The server used to run [`rast-install.py`](../../worker/linux_assets/) inside
//! the Docker image to unpack an uploaded app and describe its launcher. This
//! module is the unprivileged server-side port of that detection logic: it
//! unpacks a portable archive (or an AppImage), finds the program to start,
//! reads its `.desktop` entry, icon and update information, and describes what
//! it found. Nothing here needs root: extraction happens in a private staging
//! directory owned by the `rast` service account, and an uploaded AppImage is
//! never executed as root.
//!
//! Safety rules (all enforced and covered by tests):
//!
//! * only regular files, directories, symbolic links and hard links are
//!   accepted; devices, FIFOs, sockets and setuid/setgid entries are refused;
//! * no absolute paths and no `..` components; nothing is ever written through
//!   a symbolic link;
//! * link targets must resolve inside the extraction root, both when the link
//!   is created (lexically) and once everything exists (physically);
//! * the expanded size is bounded by `max(4 GiB, 8 × compressed size)` and the
//!   entry count by [`MAX_ENTRIES`];
//! * permissions are normalised afterwards: directories `0755`, files `0644`
//!   or `0755` when they had an execute bit, never setuid/setgid/sticky and
//!   never world writable.
//!
//! Everything a caller gets back (launcher argv, desktop entry, icon) is data;
//! running the app is the session manager's job.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use base64::Engine as _;
use serde_json::{json, Value};
use tar::EntryType;
use walkdir::WalkDir;

/// Largest number of archive entries accepted.
pub const MAX_ENTRIES: usize = 100_000;
/// Smallest accepted expanded size: a legitimately large archive may exceed
/// this, but only when it is plausibly compressed (8× the compressed size).
pub const MAX_EXPANDED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest icon stored in the database.
pub const MAX_ICON_BYTES: u64 = 512 * 1024;
/// Largest number of files scanned for launchers/icons in one tree.
pub const MAX_SCAN_FILES: usize = 50_000;
/// Largest number of ambiguous launcher candidates offered to the admin.
pub const MAX_CANDIDATES: usize = 16;
/// How long unpacking an AppImage may take before it is considered damaged.
pub const APPIMAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// The actionable message for uploads that are source code, not a prebuilt app.
pub const SOURCE_ARCHIVE_MESSAGE: &str = "This is a source archive; upload a prebuilt .deb, \
     AppImage or portable binary archive (or build it on the server distribution first).";

/// Compression of a portable application archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    Xz,
    Bzip2,
    None,
}

impl Compression {
    /// Compression of `name`, or `None` when it is not a supported archive.
    pub fn from_file_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::Gzip)
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            Some(Self::Xz)
        } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
            Some(Self::Bzip2)
        } else if lower.ends_with(".tar") {
            Some(Self::None)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gzip => "tar.gz",
            Self::Xz => "tar.xz",
            Self::Bzip2 => "tar.bz2",
            Self::None => "tar",
        }
    }
}

/// Limits applied while unpacking one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: usize,
    pub max_bytes: u64,
}

impl Limits {
    /// Limits for an archive that is `compressed` bytes on disk: the expanded
    /// size may never exceed `max(4 GiB, 8 × compressed)`.
    pub fn for_compressed(compressed: u64) -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_bytes: MAX_EXPANDED_BYTES.max(compressed.saturating_mul(8)),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_bytes: MAX_EXPANDED_BYTES,
        }
    }
}

/// What an extraction wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExtractStats {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
}

/// An archive that was unpacked into an empty staging directory.
#[derive(Debug, Clone)]
pub struct ExtractedTree {
    /// Directory that holds the app (the archive's single top-level directory
    /// when it has one, the staging root otherwise).
    pub root: PathBuf,
    pub stats: ExtractStats,
    pub compression: Compression,
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Create the extraction root (it must not exist or be empty) and make it
/// private.
fn prepare_root(root: &Path) -> Result<(), String> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() => {
            let mut entries = fs::read_dir(root)
                .map_err(|e| format!("cannot inspect the extraction folder: {e}"))?;
            if entries.next().is_some() {
                return Err("the extraction folder is not empty".into());
            }
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("the extraction folder is a symbolic link".into())
        }
        Ok(_) => return Err("the extraction path is not a folder".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(root)
            .map_err(|e| format!("cannot create the extraction folder: {e}"))?,
        Err(error) => return Err(format!("cannot inspect the extraction folder: {error}")),
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot secure the extraction folder: {e}"))?;
    Ok(())
}

/// Open the archive through its decompressor.
fn decoder(archive: &Path, compression: Compression) -> Result<Box<dyn Read>, String> {
    let file =
        fs::File::open(archive).map_err(|e| format!("cannot open {}: {e}", archive.display()))?;
    let reader = io::BufReader::new(file);
    Ok(match compression {
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(reader)),
        Compression::Xz => Box::new(xz2::read::XzDecoder::new(reader)),
        Compression::Bzip2 => Box::new(bzip2::read::BzDecoder::new(reader)),
        Compression::None => Box::new(reader),
    })
}

/// Normalise an archive path into safe relative components.
fn archive_components(raw: &[u8]) -> Result<Option<Vec<String>>, String> {
    let text =
        std::str::from_utf8(raw).map_err(|_| "archive contains a non-UTF-8 path".to_string())?;
    if text.starts_with('/') {
        return Err(format!("archive contains an absolute path: {text}"));
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(format!("archive path leaves the package: {text}")),
            other => {
                if other.contains('\0') || other.contains('\\') {
                    return Err(format!("archive contains an invalid path: {text}"));
                }
                parts.push(other.to_string());
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts))
}

/// Resolve a link target lexically from the link's directory; `None` when it
/// would leave the root.
fn lexical_target(link_parent: &[String], target: &str) -> Option<Vec<String>> {
    if target.is_empty() || target.starts_with('/') {
        return None;
    }
    let mut stack: Vec<String> = link_parent.to_vec();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other.to_string()),
        }
    }
    Some(stack)
}

fn join(root: &Path, parts: &[String]) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
    }
    path
}

/// Create the parent directories of `parts`, refusing to traverse any link.
fn ensure_parents(root: &Path, parts: &[String]) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "archive writes through a link: {}",
                    parts.join("/")
                ))
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("archive entry {} is below a file", parts.join("/"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|e| format!("cannot create a directory: {e}"))?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).ok();
            }
            Err(error) => return Err(format!("cannot inspect {}: {error}", current.display())),
        }
    }
    Ok(())
}

/// Remove an earlier entry at the same path (tar semantics: the last one
/// wins), but never a directory.
fn clear_existing(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(format!(
            "archive replaces a directory with a file: {}",
            path.display()
        )),
        Ok(_) => fs::remove_file(path).map_err(|e| format!("cannot replace an entry: {e}")),
        Err(_) => Ok(()),
    }
}

/// Header-only pass over the archive: refuse huge name/extension records
/// before the tar reader buffers them whole, and absurd entry counts.
fn prescan(archive: &Path, compression: Compression, limits: Limits) -> Result<(), String> {
    let mut tar_archive = tar::Archive::new(decoder(archive, compression)?);
    let entries = tar_archive
        .entries()
        .map_err(|error| format!("cannot read the archive: {error}"))?
        .raw(true);
    for (count, entry) in entries.enumerate() {
        let entry = entry.map_err(|error| format!("the archive is corrupt: {error}"))?;
        if count > limits.max_entries.saturating_mul(3) {
            return Err(format!(
                "The archive has more than {} files.",
                limits.max_entries
            ));
        }
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            EntryType::XGlobalHeader
                | EntryType::XHeader
                | EntryType::GNULongName
                | EntryType::GNULongLink
        ) && entry.size() > MAX_EXTENSION_BYTES
        {
            return Err(format!(
                "The archive names a file with a {} byte record (limit {MAX_EXTENSION_BYTES}).",
                entry.size()
            ));
        }
    }
    Ok(())
}

/// Largest GNU long-name/long-link or PAX extension record accepted.
pub const MAX_EXTENSION_BYTES: u64 = 64 * 1024;

/// Extract `archive` into the existing, empty directory `root`.
///
/// The archive is validated entry by entry; anything unsafe aborts the
/// extraction and leaves the caller to remove the staging directory.
pub fn extract_archive(
    archive: &Path,
    compression: Compression,
    root: &Path,
    limits: Limits,
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    prepare_root(root)?;
    prescan(archive, compression, limits)?;
    let mut tar_archive = tar::Archive::new(decoder(archive, compression)?);
    tar_archive.set_preserve_permissions(false);
    let mut stats = ExtractStats::default();
    let mut links: Vec<PathBuf> = Vec::new();
    let mut count = 0usize;
    let entries = tar_archive
        .entries()
        .map_err(|error| format!("cannot read the archive: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("the archive is corrupt: {error}"))?;
        count += 1;
        if count > limits.max_entries {
            return Err(format!(
                "The archive has more than {} files.",
                limits.max_entries
            ));
        }
        if count.is_multiple_of(500) && cancelled() {
            return Err("cancelled".into());
        }
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            EntryType::XGlobalHeader
                | EntryType::XHeader
                | EntryType::GNULongName
                | EntryType::GNULongLink
        ) {
            continue;
        }
        let mode = entry.header().mode().unwrap_or(0o644);
        if mode & 0o6000 != 0 {
            let name = String::from_utf8_lossy(&entry.path_bytes()).to_string();
            return Err(format!(
                "the archive marks {name} setuid/setgid; uploaded apps must not carry privileged files"
            ));
        }
        let path_bytes = entry.path_bytes().into_owned();
        let Some(parts) = archive_components(&path_bytes)? else {
            continue;
        };
        let dest = join(root, &parts);
        ensure_parents(root, &parts)?;
        match kind {
            EntryType::Directory => {
                match fs::symlink_metadata(&dest) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(format!(
                            "the archive replaces a file with a folder: {}",
                            parts.join("/")
                        ))
                    }
                    Err(_) => {
                        fs::create_dir(&dest).map_err(|e| format!("cannot create a folder: {e}"))?
                    }
                }
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).ok();
                stats.dirs += 1;
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.size();
                stats.bytes = stats.bytes.saturating_add(size);
                if stats.bytes > limits.max_bytes {
                    return Err(if limits.max_bytes < MAX_EXPANDED_BYTES {
                        "the uploaded archive expands beyond the size limit".into()
                    } else {
                        "the uploaded archive expands beyond the size limit (the archive may be a decompression bomb)".into()
                    });
                }
                clear_existing(&dest)?;
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|e| format!("cannot write {}: {e}", parts.join("/")))?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|e| format!("cannot extract {}: {e}", parts.join("/")))?;
                if copied != size {
                    return Err(format!(
                        "the archive entry {} is truncated",
                        parts.join("/")
                    ));
                }
                let safe = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                fs::set_permissions(&dest, fs::Permissions::from_mode(safe)).ok();
                stats.files += 1;
            }
            EntryType::Symlink => {
                let target_bytes = entry
                    .link_name_bytes()
                    .ok_or("a symbolic link has no target")?
                    .into_owned();
                let target = std::str::from_utf8(&target_bytes)
                    .map_err(|_| "a symbolic link has a non-UTF-8 target".to_string())?
                    .to_string();
                if lexical_target(&parts[..parts.len() - 1], &target).is_none() {
                    return Err(format!(
                        "the archive's symbolic link {} points outside the package ({target})",
                        parts.join("/")
                    ));
                }
                clear_existing(&dest)?;
                std::os::unix::fs::symlink(&target, &dest)
                    .map_err(|e| format!("cannot create a link: {e}"))?;
                links.push(dest);
                stats.links += 1;
            }
            EntryType::Link => {
                let target_bytes = entry
                    .link_name_bytes()
                    .ok_or("a hard link has no target")?
                    .into_owned();
                let Some(target_parts) = archive_components(&target_bytes)? else {
                    return Err(format!("the hard link {} has no target", parts.join("/")));
                };
                ensure_parents(root, &target_parts)?;
                let source = join(root, &target_parts);
                let meta = fs::symlink_metadata(&source).map_err(|_| {
                    format!("the hard link {} targets a missing file", parts.join("/"))
                })?;
                if !meta.is_file() {
                    return Err(format!(
                        "the hard link {} must target a regular file",
                        parts.join("/")
                    ));
                }
                clear_existing(&dest)?;
                fs::hard_link(&source, &dest)
                    .map_err(|e| format!("cannot create a hard link: {e}"))?;
                stats.files += 1;
            }
            other => {
                let what = if other.is_fifo() {
                    "a FIFO"
                } else if other.is_character_special() || other.is_block_special() {
                    "a device file"
                } else {
                    "a special file"
                };
                return Err(format!(
                    "the archive contains {} which is {what}; only files, folders and links are allowed",
                    parts.join("/")
                ));
            }
        }
    }
    verify_links(root, &links)?;
    sanitize_tree(root)?;
    Ok(stats)
}

/// Physical check once every entry exists: each link resolves inside root.
fn verify_links(root: &Path, links: &[PathBuf]) -> Result<(), String> {
    let root = fs::canonicalize(root).map_err(|e| format!("cannot resolve the package: {e}"))?;
    for link in links {
        let resolved = fs::canonicalize(link).map_err(|_| {
            format!(
                "the archive's symbolic link {} does not resolve inside the package",
                display_rel(&root, link)
            )
        })?;
        if resolved == root {
            return Err(format!(
                "the archive's symbolic link {} resolves to the package root and would escape after installation",
                display_rel(&root, link)
            ));
        }
        if !resolved.starts_with(&root) {
            return Err(format!(
                "the archive's symbolic link {} resolves outside the package",
                display_rel(&root, link)
            ));
        }
    }
    Ok(())
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Normalise the permissions of an unpacked tree: folders `0755`, files
/// `0644`/`0755`, no setuid/setgid/sticky bits, never world writable, and
/// every symbolic link must stay inside the tree. Also used for AppImage
/// extractions, which are produced by a third-party runtime rather than by
/// [`extract_archive`].
pub fn sanitize_tree(root: &Path) -> Result<(), String> {
    let root = fs::canonicalize(root).map_err(|e| format!("cannot resolve the package: {e}"))?;
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry.map_err(|e| format!("cannot inspect the package: {e}"))?;
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("cannot inspect {}: {e}", display_rel(&root, path)))?;
        if meta.file_type().is_symlink() {
            let resolved = fs::canonicalize(path).map_err(|_| {
                format!(
                    "the package's symbolic link {} does not resolve",
                    display_rel(&root, path)
                )
            })?;
            if !resolved.starts_with(&root) {
                return Err(format!(
                    "the package's symbolic link {} resolves outside the package",
                    display_rel(&root, path)
                ));
            }
            continue;
        }
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            if mode & 0o7777 != 0o755 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).ok();
            }
        } else if meta.is_file() {
            if mode & 0o6000 != 0 {
                return Err(format!(
                    "the package marks {} setuid/setgid; uploaded apps must not carry privileged files",
                    display_rel(&root, path)
                ));
            }
            let safe = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            if mode & 0o7777 != safe {
                fs::set_permissions(path, fs::Permissions::from_mode(safe)).ok();
            }
        } else {
            return Err(format!(
                "the package contains {} which is not a file, folder or link",
                display_rel(&root, path)
            ));
        }
    }
    Ok(())
}

/// The directory that holds the app: the archive's single top-level folder
/// when it has one, the extraction root otherwise.
pub fn unwrap_top_level(root: &Path) -> Result<PathBuf, String> {
    let mut entries = fs::read_dir(root)
        .map_err(|e| format!("cannot inspect the package: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot inspect the package: {e}"))?;
    entries.retain(|entry| entry.file_name() != ".DS_Store");
    if entries.len() == 1 {
        let only = &entries[0];
        match only.file_type() {
            Ok(kind) if kind.is_dir() => return Ok(only.path()),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    Ok(root.to_path_buf())
}

/// Unpack a portable archive into the empty `staging_root` and return the
/// directory that holds the app (with its single top-level folder unwrapped).
pub fn extract_portable(
    archive: &Path,
    file_name: &str,
    staging_root: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractedTree, String> {
    let compression = Compression::from_file_name(file_name).ok_or_else(|| {
        "This file is not a portable archive RAST can unpack; upload a .tar.gz, .tar.xz, .tar.bz2 or .tar file.".to_string()
    })?;
    let compressed = fs::metadata(archive)
        .map_err(|e| format!("cannot read the uploaded file: {e}"))?
        .len();
    let stats = extract_archive(
        archive,
        compression,
        staging_root,
        Limits::for_compressed(compressed),
        cancelled,
    )?;
    let root = unwrap_top_level(staging_root)?;
    // A link that resolved to the staging wrapper is outside the app once the
    // single top-level folder becomes the install root.
    sanitize_tree(&root)?;
    Ok(ExtractedTree {
        root,
        stats,
        compression,
    })
}

/// Make an installed application directory readable by the unprivileged app
/// identities (they are not the owner; the session grants ACLs on top).
pub fn make_install_dir(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err("the application folder is not a folder".into());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot set permissions on the application folder: {e}"))
}

// ---------------------------------------------------------------------------
// Desktop entries and command lines
// ---------------------------------------------------------------------------

/// The `[Desktop Entry]` group of a `.desktop` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub path: PathBuf,
    pub entry_type: String,
    pub name: Option<String>,
    pub comment: Option<String>,
    pub exec: Option<String>,
    pub icon: Option<String>,
    pub version: Option<String>,
    pub appimage_version: Option<String>,
    pub terminal: bool,
    pub working_dir: Option<String>,
    pub no_display: bool,
    pub hidden: bool,
    pub categories: Option<String>,
}

/// Read a `.desktop` file (its `[Desktop Entry]` group). Returns `None` when
/// the file cannot be read or has no such group.
pub fn parse_desktop_entry(path: &Path) -> Option<DesktopEntry> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut entry = DesktopEntry {
        path: path.to_path_buf(),
        entry_type: "Application".to_string(),
        ..DesktopEntry::default()
    };
    let mut group: Option<String> = None;
    let mut found = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            group = Some(line.to_string());
            found |= line == "[Desktop Entry]";
            continue;
        }
        if group.as_deref() != Some("[Desktop Entry]") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unescape_desktop_value(value.trim());
        match key {
            "Type" => {
                if !value.is_empty() {
                    entry.entry_type = value
                }
            }
            "Name" => entry.name = Some(value),
            "Comment" => entry.comment = Some(value),
            "Exec" => entry.exec = Some(value),
            "Icon" => entry.icon = Some(value),
            "Version" => entry.version = Some(value),
            "X-AppImage-Version" => entry.appimage_version = Some(value),
            "Terminal" => entry.terminal = value.eq_ignore_ascii_case("true"),
            "Path" => entry.working_dir = Some(value),
            "NoDisplay" => entry.no_display = value.eq_ignore_ascii_case("true"),
            "Hidden" => entry.hidden = value.eq_ignore_ascii_case("true"),
            "Categories" => entry.categories = Some(value),
            _ => {}
        }
    }
    found.then_some(entry)
}

/// Undo the escape sequences allowed in desktop entry values.
fn unescape_desktop_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Is this entry a visible application launcher?
pub fn desktop_entry_usable(entry: &DesktopEntry) -> bool {
    entry.entry_type == "Application"
        && !entry.no_display
        && !entry.hidden
        && entry
            .exec
            .as_deref()
            .is_some_and(|exec| !exec.trim().is_empty())
}

/// Desktop field codes (`%f`, `%u`, …) are removed from `Exec` commands.
const FIELD_CODES: &[char] = &[
    'f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'i', 'c', 'k', 'v', 'm',
];

/// Split an `Exec` value into argv the way the desktop entry spec says
/// (quotes and escapes), dropping field codes and un-escaping `%%`.
pub fn exec_argv(value: &str) -> Vec<String> {
    split_shell_words(value)
        .into_iter()
        .map(|word| strip_field_codes(&word))
        .filter(|word| !word.is_empty())
        .collect()
}

fn strip_field_codes(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some(code) if FIELD_CODES.contains(code) => {
                chars.next();
            }
            // Unknown/deprecated field codes are dropped as well.
            Some(_) => {
                chars.next();
            }
            None => out.push('%'),
        }
    }
    out
}

/// Split a command line into words, honouring single and double quotes and
/// backslash escapes. Malformed input is split best-effort.
fn split_shell_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = value.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        started = true;
                    }
                }
                ch if ch.is_whitespace() => {
                    if started || !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                other => {
                    current.push(other);
                    started = true;
                }
            },
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Icon bytes ready to be stored in `linux_apps.icon`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconData {
    pub kind: &'static str,
    pub bytes: Vec<u8>,
}

impl IconData {
    pub fn to_json(&self) -> Value {
        json!({
            "type": self.kind,
            "data": base64::engine::general_purpose::STANDARD.encode(&self.bytes),
        })
    }
}

/// Width/height of a PNG file from its IHDR header.
pub fn png_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut file = fs::File::open(path).ok()?;
    let mut head = [0u8; 24];
    file.read_exact(&mut head).ok()?;
    if head[..8] != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let height = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

/// Icon preference: medium PNGs first, then SVG, then large/small PNGs.
fn icon_score(path: &Path) -> i64 {
    if path
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("svg"))
    {
        return 300;
    }
    let Some((width, height)) = png_dimensions(path) else {
        return -1;
    };
    let side = width.min(height);
    if side > 512 {
        200
    } else if side >= 128 {
        400 + side as i64
    } else {
        side as i64
    }
}

/// Find an icon by name under `roots`. A root may be a file (package file
/// lists) or a directory (icon themes); directories are scanned for a file
/// whose stem equals `name`.
pub fn find_icon(name: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let named = Path::new(name);
    if named.is_absolute() {
        let meta = fs::metadata(named).ok()?;
        return (meta.is_file() && icon_score(named) >= 0).then(|| named.to_path_buf());
    }
    // A relative path with separators is resolved from the icon roots too.
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        match fs::metadata(root) {
            Ok(meta) if meta.is_file() => {
                let stem = root
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_default();
                if stem == name && icon_score(root) >= 0 {
                    found.push(root.clone());
                }
            }
            Ok(meta) if meta.is_dir() => {
                let mut scanned = 0usize;
                for entry in WalkDir::new(root).follow_links(false).max_depth(8) {
                    let Ok(entry) = entry else { continue };
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    scanned += 1;
                    if scanned > MAX_SCAN_FILES {
                        break;
                    }
                    let path = entry.path();
                    let ext_ok = path.extension().is_some_and(|ext| {
                        let ext = ext.to_string_lossy();
                        ext.eq_ignore_ascii_case("png") || ext.eq_ignore_ascii_case("svg")
                    });
                    if !ext_ok {
                        continue;
                    }
                    let stem = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if stem == name && icon_score(path) >= 0 {
                        found.push(path.to_path_buf());
                    }
                }
            }
            _ => {}
        }
        if !found.is_empty() {
            break;
        }
    }
    found.into_iter().max_by_key(|path| icon_score(path))
}

/// Any reasonable icon in an unpacked application tree ("icon"/"logo" names
/// or one of the hints).
pub fn guess_icon(root: &Path, hints: &[String]) -> Option<PathBuf> {
    let hints: Vec<String> = hints
        .iter()
        .map(|hint| simplify(hint))
        .filter(|hint| hint.len() >= 3)
        .collect();
    let mut best: Option<PathBuf> = None;
    let mut scanned = 0usize;
    for entry in WalkDir::new(root).follow_links(false).max_depth(6) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        scanned += 1;
        if scanned > MAX_SCAN_FILES {
            break;
        }
        let path = entry.path();
        let ext_ok = path.extension().is_some_and(|ext| {
            let ext = ext.to_string_lossy();
            ext.eq_ignore_ascii_case("png") || ext.eq_ignore_ascii_case("svg")
        });
        if !ext_ok {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let named = stem.contains("icon")
            || stem.contains("logo")
            || hints
                .iter()
                .any(|hint| !hint.is_empty() && stem.contains(hint));
        if !named || icon_score(path) < 0 {
            continue;
        }
        let better = match &best {
            Some(current) => icon_score(path) > icon_score(current),
            None => true,
        };
        if better {
            best = Some(path.to_path_buf());
        }
    }
    best
}

/// Read an icon file: at most [`MAX_ICON_BYTES`], PNG or SVG only.
pub fn read_icon(path: &Path) -> Option<IconData> {
    let resolved = fs::canonicalize(path).ok()?;
    let meta = fs::metadata(&resolved).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ICON_BYTES {
        return None;
    }
    let bytes = fs::read(&resolved).ok()?;
    let svg_name = resolved
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("svg"));
    if svg_name || bytes.windows(4).take(2048).any(|w| w == b"<svg") {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        if head.contains("<svg") {
            return Some(IconData { kind: "svg", bytes });
        }
        return None;
    }
    if bytes.starts_with(b"\x89PNG") {
        return Some(IconData { kind: "png", bytes });
    }
    None
}

/// Icon for a desktop entry: its `Icon=` value, then a guessed icon.
pub fn entry_icon(entry: &DesktopEntry, roots: &[PathBuf], hints: &[String]) -> Option<IconData> {
    let by_name = entry
        .icon
        .as_deref()
        .and_then(|name| find_icon(name, roots))
        .and_then(|path| read_icon(&path));
    by_name.or_else(|| {
        let mut all_hints = hints.to_vec();
        if let Some(name) = &entry.name {
            all_hints.push(name.clone());
        }
        roots
            .iter()
            .find_map(|root| guess_icon(root, &all_hints))
            .and_then(|path| read_icon(&path))
    })
}

// ---------------------------------------------------------------------------
// ELF inspection
// ---------------------------------------------------------------------------

/// Is `path` an ELF executable?
pub fn is_elf(path: &Path) -> bool {
    let mut head = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .map(|()| head == *b"\x7fELF")
        .unwrap_or(false)
}

/// Debian architecture of an ELF file (`amd64`, `arm64`, `i386`, …).
pub fn elf_architecture(path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 20];
    let mut file = fs::File::open(path).ok()?;
    file.read_exact(&mut head).ok()?;
    if head[..4] != *b"\x7fELF" {
        return None;
    }
    let little = match head[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let machine = if little {
        u16::from_le_bytes([head[18], head[19]])
    } else {
        u16::from_be_bytes([head[18], head[19]])
    };
    Some(match machine {
        3 => "i386",
        8 => "mips",
        20 => "powerpc",
        21 => "ppc64",
        22 => "s390x",
        40 => "armhf",
        62 => "amd64",
        183 => "arm64",
        243 => "riscv64",
        _ => return None,
    })
}

/// Value of the `.upd_info` ELF section of an AppImage: its embedded update
/// information. Read from the file header, without executing anything.
pub fn appimage_update_info(image: &Path) -> Option<String> {
    let mut file = fs::File::open(image).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut ident = [0u8; 16];
    file.read_exact(&mut ident).ok()?;
    if ident[..4] != *b"\x7fELF" {
        return None;
    }
    let wide = match ident[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let little = match ident[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let read_u16 = |file: &mut fs::File, at: u64| -> Option<u16> {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut bytes = [0u8; 2];
        file.read_exact(&mut bytes).ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let read_u32 = |file: &mut fs::File, at: u64| -> Option<u32> {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut bytes = [0u8; 4];
        file.read_exact(&mut bytes).ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let read_u64 = |file: &mut fs::File, at: u64| -> Option<u64> {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut bytes = [0u8; 8];
        file.read_exact(&mut bytes).ok()?;
        Some(if little {
            u64::from_le_bytes(bytes)
        } else {
            u64::from_be_bytes(bytes)
        })
    };
    let (shoff, shentsize, shnum, shstrndx) = if wide {
        (
            read_u64(&mut file, 0x28)?,
            read_u16(&mut file, 0x3A)?,
            read_u16(&mut file, 0x3C)?,
            read_u16(&mut file, 0x3E)?,
        )
    } else {
        (
            read_u32(&mut file, 0x20)? as u64,
            read_u16(&mut file, 0x2E)?,
            read_u16(&mut file, 0x30)?,
            read_u16(&mut file, 0x32)?,
        )
    };
    let minimum = if wide { 64 } else { 40 };
    if shnum == 0 || shnum > 4096 || shentsize < minimum {
        return None;
    }
    let table_end = shoff.checked_add(u64::from(shnum).checked_mul(u64::from(shentsize))?)?;
    if table_end > file_len {
        return None;
    }
    if shstrndx >= shnum {
        return None;
    }
    // Section header: name, offset and size (layouts from the ELF spec).
    let section = |file: &mut fs::File, index: u16| -> Option<(u32, u64, u64)> {
        let base = shoff + u64::from(index) * u64::from(shentsize);
        if wide {
            Some((
                read_u32(file, base)?,
                read_u64(file, base + 24)?,
                read_u64(file, base + 32)?,
            ))
        } else {
            Some((
                read_u32(file, base)?,
                read_u32(file, base + 16)? as u64,
                read_u32(file, base + 20)? as u64,
            ))
        }
    };
    let (_name, names_offset, names_size) = section(&mut file, shstrndx)?;
    if names_offset.checked_add(names_size)? > file_len {
        return None;
    }
    for index in 0..shnum {
        let (name, offset, size) = section(&mut file, index)?;
        if name == 0 {
            continue;
        }
        let name_at = names_offset.checked_add(u64::from(name))?;
        if name_at >= names_offset.saturating_add(names_size) {
            continue;
        }
        file.seek(SeekFrom::Start(name_at)).ok()?;
        let mut name_bytes = Vec::new();
        let mut byte = [0u8; 1];
        for _ in 0..64 {
            file.read_exact(&mut byte).ok()?;
            if byte[0] == 0 {
                break;
            }
            name_bytes.push(byte[0]);
        }
        if name_bytes != b".upd_info" {
            continue;
        }
        let read_size = size.min(1024);
        if offset.checked_add(read_size)? > file_len {
            return None;
        }
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut raw = vec![0u8; read_size as usize];
        file.read_exact(&mut raw).ok()?;
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        let text = String::from_utf8_lossy(&raw[..end]).trim().to_string();
        return (!text.is_empty()).then_some(text);
    }
    None
}

/// Parse an AppImage's update information string into the JSON shape the
/// update sources in [`super::updates`] expect.
pub fn parse_update_info(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split('|').collect();
    if parts[0] == "gh-releases-zsync" && parts.len() == 5 {
        return Some(json!({
            "kind": "github",
            "owner": parts[1],
            "repo": parts[2],
            "tag": parts[3],
            "file": parts[4],
        }));
    }
    if parts[0] == "zsync" && parts.len() == 2 && parts[1].starts_with("https://") {
        return Some(json!({
            "kind": "zsync",
            "url": parts[1],
        }));
    }
    None
}

// ---------------------------------------------------------------------------
// AppImage extraction
// ---------------------------------------------------------------------------

/// A run AppImage that was unpacked and inspected.
#[derive(Debug, Clone)]
pub struct AppImageRun {
    /// The unpacked app (`…/squashfs-root`).
    pub root: PathBuf,
    /// Private working directory the caller must remove after moving `root`.
    pub work: PathBuf,
    /// `.upd_info` parsed for the update check, if any.
    pub update: Option<Value>,
    /// Raw `.upd_info` text, if any.
    pub update_info: Option<String>,
    pub architecture: Option<&'static str>,
    pub discovery: AppImageDiscovery,
}

/// What an AppImage contains.
#[derive(Debug, Clone)]
pub struct AppImageDiscovery {
    pub launcher: Launcher,
    pub icon: Option<IconData>,
    pub entry: Option<DesktopEntry>,
    pub chromium: bool,
}

/// Unpack an AppImage into a private directory below `parent` and inspect it.
///
/// `--appimage-extract` executes the AppImage's own runtime, so this refuses to
/// run as root: the server must be the unprivileged `rast` account.
pub fn extract_appimage(
    image: &Path,
    image_file_name: &str,
    parent: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AppImageRun, String> {
    if unsafe { libc::geteuid() } == 0 {
        return Err(
            "RAST refuses to run an uploaded AppImage as root; run the server as the unprivileged rast user"
                .into(),
        );
    }
    let meta = fs::symlink_metadata(image)
        .map_err(|error| format!("cannot read the uploaded AppImage: {error}"))?;
    if !meta.is_file() {
        return Err("the uploaded AppImage is not a regular file".into());
    }
    if meta.len() == 0 {
        return Err("the uploaded AppImage is empty".into());
    }
    let update_info = appimage_update_info(image);
    let update = update_info.as_deref().and_then(parse_update_info);
    let work = parent.join(format!("appimage-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        fs::create_dir_all(&work)
            .map_err(|e| format!("cannot prepare the AppImage working folder: {e}"))?;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot secure the AppImage working folder: {e}"))?;
        let home = work.join("home");
        let tmp = work.join("tmp");
        fs::create_dir(&home).map_err(|e| format!("cannot prepare the AppImage workspace: {e}"))?;
        fs::create_dir(&tmp).map_err(|e| format!("cannot prepare the AppImage workspace: {e}"))?;
        let copy = work.join("app.AppImage");
        fs::copy(image, &copy).map_err(|e| format!("cannot copy the AppImage: {e}"))?;
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot prepare the AppImage: {e}"))?;
        let stderr_path = work.join("extract.stderr");
        run_limited(&copy, &work, &home, &tmp, &stderr_path, cancelled)?;
        let root = work.join("squashfs-root");
        let root_meta = fs::symlink_metadata(&root).map_err(|_| {
            "This AppImage could not be unpacked (it may be damaged or very old).".to_string()
        })?;
        if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
            return Err(
                "This AppImage could not be unpacked (it may be damaged or very old).".into(),
            );
        }
        sanitize_tree(&root)?;
        let discovery = discover_appimage(&root, image_file_name)?;
        Ok((root, discovery))
    })();
    match result {
        Ok((root, discovery)) => Ok(AppImageRun {
            root,
            work,
            update,
            update_info,
            architecture: elf_architecture(image),
            discovery,
        }),
        Err(error) => {
            let _ = fs::remove_dir_all(&work);
            Err(error)
        }
    }
}

/// Execute the AppImage's `--appimage-extract` mode with a private HOME/TMP,
/// bounded by [`APPIMAGE_TIMEOUT`]; its output goes to a private log file.
fn run_limited(
    program: &Path,
    work: &Path,
    home: &Path,
    tmp: &Path,
    stderr_path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let stderr = fs::File::create(stderr_path)
        .map_err(|e| format!("cannot prepare the AppImage log: {e}"))?;
    let mut command = Command::new(program);
    command
        .arg("--appimage-extract")
        .current_dir(work)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", home)
        .env("TMPDIR", tmp)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|e| format!("the AppImage could not be started: {e}"))?;
    let deadline = Instant::now() + APPIMAGE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(format!(
                    "This AppImage could not be unpacked: {}",
                    tail_of_file(stderr_path)
                        .unwrap_or_else(|| format!("the extractor exited with {status}"))
                ));
            }
            Ok(None) => {
                if cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("cancelled".into());
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("unpacking this AppImage took too long (it may be damaged)".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the AppImage could not be started: {error}"));
            }
        }
    }
}

/// Last non-empty line of a log file (bounded), for actionable errors.
fn tail_of_file(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut line = line.to_string();
    if line.len() > 300 {
        let mut cut = 300;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push('…');
    }
    Some(line)
}

// ---------------------------------------------------------------------------
// Launchers
// ---------------------------------------------------------------------------

/// The program RAST starts for an app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launcher {
    /// Command to run. The first element is an absolute program path (inside
    /// the managed install directory for portable apps).
    pub argv: Vec<String>,
    /// The `.desktop` file, relative to the install directory (portable) or
    /// absolute on the host (`.deb`).
    pub desktop_entry: Option<String>,
    pub name: Option<String>,
    pub comment: Option<String>,
    pub icon: Option<String>,
    pub terminal: bool,
    pub working_dir: Option<String>,
    /// Chromium-based (the native session must keep its sandbox as-is).
    pub chromium: bool,
}

impl Launcher {
    pub fn to_json(&self) -> Value {
        json!({
            "argv": self.argv,
            "desktopEntry": self.desktop_entry,
            "name": self.name,
            "comment": self.comment,
            "icon": self.icon,
            "terminal": self.terminal,
            "workingDir": self.working_dir,
            "chromium": self.chromium,
        })
    }

    /// Rewrite paths that point inside `from` so they point into `to`
    /// (used when a staging tree is moved to its managed install directory).
    pub fn rebase(&mut self, from: &Path, to: &Path) {
        let rebase = |value: &str| -> String {
            let path = Path::new(value);
            match path.strip_prefix(from) {
                Ok(rest) if rest.as_os_str().is_empty() => to.display().to_string(),
                Ok(rest) => {
                    let mut target = to.to_path_buf();
                    target.push(rest);
                    target.display().to_string()
                }
                Err(_) => value.to_string(),
            }
        };
        for arg in &mut self.argv {
            *arg = rebase(arg);
        }
        if let Some(dir) = &mut self.working_dir {
            if Path::new(dir).is_absolute() {
                *dir = rebase(dir);
            }
        }
    }
}

/// One program the admin may choose when an archive does not say which one is
/// the application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub desktop_entry: Option<String>,
}

impl Candidate {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "argv": self.argv,
            "desktopEntry": self.desktop_entry,
        })
    }
}

/// Everything detection found in a portable archive.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub launcher: Option<Launcher>,
    pub icon: Option<IconData>,
    pub candidates: Vec<Candidate>,
    pub launch_guess: bool,
    /// More than one equally plausible program: the admin must choose one.
    pub ambiguous: bool,
    /// The tree looks like source code and no app was found.
    pub source_like: bool,
    pub chromium: bool,
    pub entry: Option<DesktopEntry>,
}

impl Discovery {
    /// The error to show when this tree is not a prebuilt application.
    pub fn rejection(&self) -> String {
        SOURCE_ARCHIVE_MESSAGE.to_string()
    }
}

/// Programs that are never the application (Chromium/Electron helpers).
pub const SKIP_PROGRAMS: &[&str] = &[
    "chrome-sandbox",
    "chrome_crashpad_handler",
    "crashpad_handler",
    "crashreporter",
    "updater",
    "update",
    "uninstall",
    "uninstaller",
    "glxtest",
    "vaapitest",
    "configure",
    "pingsender",
    "minidump-2-core",
];

fn simplify(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn relative_to<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    path.strip_prefix(root).ok()
}

/// All regular files below `root` (symlinks are not followed).
fn walk_files(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).max_depth(max_depth) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        files.push(entry.into_path());
        if files.len() >= MAX_SCAN_FILES {
            break;
        }
    }
    files
}

/// A program found in an archive.
#[derive(Debug, Clone)]
struct FoundProgram {
    path: PathBuf,
    relative: String,
    #[allow(dead_code)]
    elf: bool,
    score: i64,
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Score a candidate program the way the old image installer did.
fn program_score(path: &Path, root: &Path, hint: &str) -> Option<i64> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let lower = name.to_ascii_lowercase();
    if SKIP_PROGRAMS.contains(&lower.as_str())
        || lower.contains(".so")
        || lower.ends_with(".py")
        || lower.ends_with(".pl")
        || lower.ends_with(".txt")
        || !is_executable_file(path)
    {
        return None;
    }
    let relative = relative_to(root, path)?;
    let rel = relative.display().to_string();
    let depth = rel.matches('/').count();
    let elf = is_elf(path);
    let mut score = 0;
    if elf {
        score += 3;
    }
    if lower.ends_with(".sh") {
        score += 1;
    }
    if depth == 0 {
        score += 2;
    } else if rel.starts_with("bin/") {
        score += 1;
    }
    let stem = Path::new(&name)
        .file_stem()
        .map(|stem| simplify(&stem.to_string_lossy()))
        .unwrap_or_default();
    let prefix = common_prefix(&stem, hint);
    if prefix >= 3 {
        score += 4 + prefix.min(6) as i64;
    }
    Some(score)
}

fn find_programs(root: &Path, hint: &str, max_depth: usize) -> Vec<FoundProgram> {
    let hint = simplify(hint);
    let mut programs: Vec<FoundProgram> = walk_files(root, max_depth)
        .into_iter()
        .filter_map(|path| {
            let score = program_score(&path, root, &hint)?;
            let relative = relative_to(root, &path)
                .map(|rel| rel.display().to_string())
                .unwrap_or_default();
            Some(FoundProgram {
                elf: is_elf(&path),
                path,
                relative,
                score,
            })
        })
        .collect();
    programs.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.relative.cmp(&b.relative))
    });
    programs
}

/// Resolve a program named by a desktop entry to a file inside `root`.
fn resolve_inside_tree(root: &Path, program: &str, base: Option<&Path>) -> Option<PathBuf> {
    let root_canonical = fs::canonicalize(root).ok()?;
    let named = Path::new(program);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if named.is_absolute() {
        candidates.push(named.to_path_buf());
    } else {
        if let Some(base) = base {
            candidates.push(base.join(named));
        }
        candidates.push(root.join(named));
    }
    for candidate in candidates {
        let Ok(resolved) = fs::canonicalize(&candidate) else {
            continue;
        };
        if !resolved.starts_with(&root_canonical) || !is_executable_file(&resolved) {
            continue;
        }
        return Some(resolved);
    }
    // Last resort: search the tree for a program with the same file name.
    let file_name = named.file_name()?.to_string_lossy().to_string();
    find_programs(root, "", 4)
        .into_iter()
        .find(|program| {
            program
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy() == file_name)
        })
        .and_then(|program| fs::canonicalize(program.path).ok())
        .filter(|resolved| resolved.starts_with(&root_canonical))
}

/// Resolve a program named by a chosen launcher to a file inside `root`.
pub fn resolve_program(root: &Path, program: &str) -> Option<PathBuf> {
    resolve_inside_tree(root, program, None)
}

/// Chromium-based apps ship `chrome-sandbox` next to the browser binary.
pub fn is_chromium(files: &[PathBuf]) -> bool {
    files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy() == "chrome-sandbox")
    })
}

/// Does the tree look like source code (no prebuilt application)?
fn looks_like_source(root: &Path, files: &[PathBuf]) -> bool {
    const MARKERS: &[&str] = &[
        "Makefile",
        "makefile",
        "GNUmakefile",
        "CMakeLists.txt",
        "configure",
        "configure.ac",
        "meson.build",
        "Cargo.toml",
        "go.mod",
        "setup.py",
        "pyproject.toml",
    ];
    let has_marker = MARKERS
        .iter()
        .any(|marker| fs::symlink_metadata(root.join(marker)).is_ok());
    let has_elf = files.iter().any(|path| is_elf(path));
    if has_elf {
        return false;
    }
    let source_files = files
        .iter()
        .filter(|path| {
            path.extension().is_some_and(|ext| {
                matches!(
                    ext.to_string_lossy().as_ref(),
                    "c" | "cc" | "cpp" | "h" | "hpp" | "rs" | "go" | "py" | "js" | "ts"
                )
            })
        })
        .count();
    has_marker || source_files > 8
}

/// Pick the main `.desktop` entry among the parsed candidates.
fn pick_desktop(entries: &[DesktopEntry], hint: &str) -> Option<DesktopEntry> {
    let hint = simplify(hint);
    let mut best: Option<(DesktopEntry, i64)> = None;
    for entry in entries {
        if !desktop_entry_usable(entry) {
            continue;
        }
        let stem = entry
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut score = 0i64;
        if !hint.is_empty() && stem == hint {
            score += 4;
        } else if !hint.is_empty() && (hint.contains(&stem) || stem.contains(&hint)) {
            score += 2;
        }
        if stem.contains("handler") || stem.contains("url") {
            score -= 3;
        }
        match &best {
            Some((_, best_score)) if score <= *best_score => {}
            _ => best = Some((entry.clone(), score)),
        }
    }
    best.map(|(entry, _)| entry)
}

/// Title from a program file name: `my-cool_app.bin` → "My Cool App".
fn title_from_program(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => &name,
    };
    let cleaned: String = stem
        .chars()
        .map(|ch| if matches!(ch, '-' | '_') { ' ' } else { ch })
        .collect();
    cleaned
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn launcher_from_entry(
    entry: &DesktopEntry,
    root: &Path,
    argv: Vec<String>,
    chromium: bool,
    icon: Option<String>,
) -> Launcher {
    let desktop_entry = relative_to(root, &entry.path).map(|path| path.display().to_string());
    let working_dir = entry.working_dir.as_deref().and_then(|dir| {
        let path = Path::new(dir);
        if path.is_absolute() {
            // Only a directory inside the package makes sense for a portable app.
            let resolved = fs::canonicalize(path).ok()?;
            resolved
                .starts_with(fs::canonicalize(root).ok()?)
                .then(|| relative_to(root, &resolved).map(|rel| rel.display().to_string()))
                .flatten()
        } else {
            Some(dir.to_string())
        }
    });
    Launcher {
        argv,
        desktop_entry,
        name: entry.name.clone(),
        comment: entry.comment.clone(),
        icon,
        terminal: entry.terminal,
        working_dir,
        chromium,
    }
}

/// Inspect an unpacked portable archive: find its launcher, icon and the
/// candidates an admin can choose from.
pub fn discover_archive(root: &Path, hint: &str) -> Discovery {
    let files = walk_files(root, 6);
    let mut entries: Vec<DesktopEntry> = files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("desktop"))
                && relative_to(root, path)
                    .map(|rel| rel.components().count() <= 4)
                    .unwrap_or(false)
        })
        .filter_map(|path| parse_desktop_entry(path))
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let chromium = is_chromium(&files);
    let mut discovery = Discovery {
        chromium,
        source_like: looks_like_source(root, &files),
        ..Discovery::default()
    };
    let hint_simplified = simplify(hint);
    let icon_roots = vec![
        root.to_path_buf(),
        root.join("usr/share/icons/hicolor"),
        root.join("usr/share/pixmaps"),
    ];
    if entries.iter().any(desktop_entry_usable) {
        if let Some(picked) = pick_desktop(&entries, hint) {
            let mut launcher: Option<Launcher> = None;
            let raw = exec_argv(picked.exec.as_deref().unwrap_or(""));
            if !raw.is_empty() {
                let base = picked.path.parent();
                match resolve_inside_tree(root, &raw[0], base) {
                    Some(program) => {
                        let mut argv = raw;
                        argv[0] = program.display().to_string();
                        launcher = Some(launcher_from_entry(
                            &picked,
                            root,
                            argv,
                            chromium,
                            picked.icon.clone(),
                        ));
                    }
                    None => {
                        if let Some(best) =
                            find_programs(root, &hint_simplified, 4).into_iter().next()
                        {
                            let argv = vec![best.path.display().to_string()];
                            let mut picked = picked.clone();
                            picked
                                .name
                                .get_or_insert_with(|| title_from_program(&best.path));
                            launcher = Some(launcher_from_entry(
                                &picked,
                                root,
                                argv,
                                chromium,
                                picked.icon.clone(),
                            ));
                            discovery.launch_guess = true;
                        }
                    }
                }
            } else if let Some(best) = find_programs(root, &hint_simplified, 4).into_iter().next() {
                let argv = vec![best.path.display().to_string()];
                let mut picked = picked.clone();
                picked
                    .name
                    .get_or_insert_with(|| title_from_program(&best.path));
                launcher = Some(launcher_from_entry(
                    &picked,
                    root,
                    argv,
                    chromium,
                    picked.icon.clone(),
                ));
                discovery.launch_guess = true;
            }
            if launcher.is_some() {
                discovery.icon =
                    entry_icon(&picked, &icon_roots, std::slice::from_ref(&hint_simplified));
                discovery.entry = Some(picked);
                discovery.launcher = launcher;
            }
        }
    }
    if discovery.launcher.is_none() {
        let programs = find_programs(root, &hint_simplified, 4);
        if let Some(best) = programs.first() {
            let mut launcher = Launcher {
                argv: vec![best.path.display().to_string()],
                name: Some(title_from_program(&best.path)),
                chromium,
                ..Launcher::default()
            };
            launcher.icon = None;
            discovery.launch_guess = true;
            discovery.icon = guess_icon(root, std::slice::from_ref(&hint_simplified))
                .as_deref()
                .and_then(read_icon);
            discovery.launcher = Some(launcher);
            // Equally plausible programs need the admin's choice.
            if programs.len() > 1 && programs[0].score == programs[1].score {
                discovery.ambiguous = true;
            }
        }
    }
    // Candidates: usable desktop entries first, then programs.
    for entry in entries.iter().filter(|entry| desktop_entry_usable(entry)) {
        if discovery.candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let raw = exec_argv(entry.exec.as_deref().unwrap_or(""));
        if raw.is_empty() {
            continue;
        }
        let mut argv = raw.clone();
        if let Some(program) = resolve_inside_tree(root, &raw[0], entry.path.parent()) {
            argv[0] = program.display().to_string();
        }
        discovery.candidates.push(Candidate {
            name: entry.name.clone(),
            argv,
            desktop_entry: relative_to(root, &entry.path).map(|p| p.display().to_string()),
        });
    }
    if discovery.candidates.is_empty() {
        for program in find_programs(root, &hint_simplified, 4)
            .into_iter()
            .take(MAX_CANDIDATES)
        {
            discovery.candidates.push(Candidate {
                name: Some(title_from_program(&program.path)),
                argv: vec![program.path.display().to_string()],
                desktop_entry: None,
            });
        }
    }
    if discovery.candidates.len() > 1 && discovery.launch_guess {
        discovery.ambiguous = true;
    }
    discovery
}

/// Inspect an unpacked AppImage the way the old image installer did: `AppRun`
/// is the program, the `.desktop` entry only supplies arguments and metadata.
pub fn discover_appimage(root: &Path, image_file_name: &str) -> Result<AppImageDiscovery, String> {
    let apprun = root.join("AppRun");
    if !is_executable_file(&apprun) {
        return Err("This AppImage has no AppRun program.".into());
    }
    let files = walk_files(root, 4);
    let entries: Vec<DesktopEntry> = files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("desktop"))
                && relative_to(root, path)
                    .map(|rel| rel.components().count() <= 2)
                    .unwrap_or(false)
        })
        .filter_map(|path| parse_desktop_entry(path))
        .filter(desktop_entry_usable)
        .collect();
    let mut entries = entries;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let entry = entries.first().cloned();
    let icon_roots = vec![
        root.to_path_buf(),
        root.join("usr/share/icons/hicolor"),
        root.join("usr/share/pixmaps"),
    ];
    let image_hint = Path::new(image_file_name)
        .file_stem()
        .map(|stem| simplify(&stem.to_string_lossy()))
        .unwrap_or_default();
    let mut argv = vec![apprun.display().to_string()];
    if let Some(entry) = &entry {
        let rest = exec_argv(entry.exec.as_deref().unwrap_or(""));
        if rest.len() > 1 {
            argv.extend(rest.into_iter().skip(1));
        }
    }
    let mut icon = entry
        .as_ref()
        .and_then(|entry| entry_icon(entry, &icon_roots, std::slice::from_ref(&image_hint)));
    if icon.is_none() {
        icon = ["icon.png", "icon.svg", ".DirIcon"]
            .iter()
            .map(|name| root.join(name))
            .find_map(|path| read_icon(&path));
    }
    let launcher = Launcher {
        argv,
        desktop_entry: entry
            .as_ref()
            .and_then(|entry| relative_to(root, &entry.path).map(|p| p.display().to_string())),
        name: entry.as_ref().and_then(|entry| entry.name.clone()),
        comment: entry.as_ref().and_then(|entry| entry.comment.clone()),
        icon: entry.as_ref().and_then(|entry| entry.icon.clone()),
        terminal: entry.as_ref().map(|entry| entry.terminal).unwrap_or(false),
        working_dir: None,
        chromium: is_chromium(&files),
    };
    Ok(AppImageDiscovery {
        launcher,
        icon,
        entry,
        chromium: is_chromium(&files),
    })
}

// ---------------------------------------------------------------------------
// Installed Debian packages
// ---------------------------------------------------------------------------

/// What an installed package maps to on the host filesystem.
#[derive(Debug, Clone, Default)]
pub struct HostDiscovery {
    pub launcher: Option<Launcher>,
    pub icon: Option<IconData>,
    pub entry: Option<DesktopEntry>,
    pub chromium: bool,
}

/// The command for a program on the host: an absolute path when it exists,
/// otherwise the PATH lookup, otherwise the bare name.
fn resolve_host_program(program: &str, package_files: &[PathBuf]) -> String {
    let named = Path::new(program);
    if named.is_absolute() {
        if is_executable_file(named) {
            return program.to_string();
        }
        // Some packages list the binary through a symlink or with a suffix.
        let file_name = named
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        if let Some(file_name) = file_name {
            if let Some(found) = package_files.iter().find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy() == file_name)
                    && is_executable_file(path)
            }) {
                return found.display().to_string();
            }
        }
        return program.to_string();
    }
    if let Some(found) = package_files.iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy() == program)
            && is_executable_file(path)
    }) {
        return found.display().to_string();
    }
    which(program).unwrap_or_else(|| program.to_string())
}

/// Search `PATH` like `shutil.which`.
fn which(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| is_executable_file(candidate))
        .map(|path| path.display().to_string())
}

/// Inspect the files an installed `.deb` package owns: its `.desktop` entry,
/// launcher command, icon and Chromium marker.
pub fn discover_installed_package(package: &str, files: &[PathBuf]) -> HostDiscovery {
    let chromium = is_chromium(files);
    let mut desktops: Vec<DesktopEntry> = files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("desktop"))
                && path.to_string_lossy().contains("/applications/")
        })
        .filter_map(|path| parse_desktop_entry(path))
        .collect();
    desktops.sort_by(|a, b| a.path.cmp(&b.path));
    let desktop = pick_desktop(&desktops, package);
    let mut icon_roots: Vec<PathBuf> = files
        .iter()
        .filter(|path| {
            path.extension().is_some_and(|ext| {
                let ext = ext.to_string_lossy();
                ext.eq_ignore_ascii_case("png") || ext.eq_ignore_ascii_case("svg")
            })
        })
        .cloned()
        .collect();
    icon_roots.push(PathBuf::from("/usr/share/icons/hicolor"));
    icon_roots.push(PathBuf::from("/usr/share/pixmaps"));
    let Some(entry) = desktop else {
        return HostDiscovery {
            chromium,
            ..HostDiscovery::default()
        };
    };
    let raw = exec_argv(entry.exec.as_deref().unwrap_or(""));
    let Some((program, rest)) = raw.split_first() else {
        return HostDiscovery {
            chromium,
            ..HostDiscovery::default()
        };
    };
    let mut argv = vec![resolve_host_program(program, files)];
    argv.extend(rest.iter().cloned());
    let working_dir = entry.working_dir.as_deref().and_then(|dir| {
        let path = Path::new(dir);
        (path.is_absolute() && path.is_dir()).then(|| dir.to_string())
    });
    let launcher = Launcher {
        argv,
        desktop_entry: Some(entry.path.display().to_string()),
        name: entry.name.clone(),
        comment: entry.comment.clone(),
        icon: entry.icon.clone(),
        terminal: entry.terminal,
        working_dir,
        chromium,
    };
    let icon = entry_icon(&entry, &icon_roots, &[package.to_string()]);
    HostDiscovery {
        launcher: Some(launcher),
        icon,
        entry: Some(entry),
        chromium,
    }
}

/// Verify that a launcher found in an install directory still points at an
/// executable file of that directory (used before switching to a new version).
pub fn validate_launcher(root: &Path, launcher: &Launcher) -> Result<(), String> {
    let Some(program) = launcher.argv.first() else {
        return Err("the app's launcher has no command".into());
    };
    let root = fs::canonicalize(root).map_err(|e| format!("cannot resolve the app folder: {e}"))?;
    let program_path = Path::new(program);
    let resolved = fs::canonicalize(program_path)
        .map_err(|_| format!("the app's program {program} is missing after installation"))?;
    if !resolved.starts_with(&root) {
        return Err("the app's program is outside its install folder".into());
    }
    if !is_executable_file(&resolved) {
        return Err(format!("the app's program {program} is not executable"));
    }
    Ok(())
}

/// Every path component of `relative` is a normal name (no `..`, no absolute
/// path, no NUL or backslash).
pub fn is_safe_relative(relative: &str) -> bool {
    let path = Path::new(relative);
    if path.is_absolute() {
        return false;
    }
    let mut any = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => any = true,
            _ => return false,
        }
    }
    any
}

/// The set of fields a desktop entry parser recognised; used by tests and by
/// the preview JSON to show what the entry said.
pub fn desktop_entry_summary(entry: &DesktopEntry) -> Value {
    json!({
        "path": entry.path.display().to_string(),
        "name": entry.name,
        "comment": entry.comment,
        "exec": entry.exec,
        "icon": entry.icon,
        "terminal": entry.terminal,
        "version": entry.version,
        "appimageVersion": entry.appimage_version,
    })
}

/// Sorted, unique program names from `files` (used to detect helper programs
/// and for logging).
pub fn base_names(files: &[PathBuf]) -> BTreeSet<String> {
    files
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression as GzCompression};
    use std::io::Write;

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    enum Item<'a> {
        Dir(&'a str, u32),
        File(&'a str, &'a [u8], u32),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Fifo(&'a str),
        CharDevice(&'a str),
    }

    fn raw_header(kind: EntryType, name: &str, link: &str, mode: u32, size: u64) -> tar::Header {
        let mut header = tar::Header::new_old();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_size(size);
        header.set_mtime(0);
        {
            let old = header.as_old_mut();
            old.name[..name.len()].copy_from_slice(name.as_bytes());
            old.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        header.set_cksum();
        header
    }

    fn build_tar(path: &Path, items: &[Item<'_>], compression: Compression) {
        let mut plain = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut plain);
            for item in items {
                match item {
                    Item::Dir(name, mode) => {
                        let header = raw_header(EntryType::Directory, name, "", *mode, 0);
                        builder.append(&header, io::empty()).unwrap();
                    }
                    Item::File(name, data, mode) => {
                        let header =
                            raw_header(EntryType::Regular, name, "", *mode, data.len() as u64);
                        builder.append(&header, *data).unwrap();
                    }
                    Item::Symlink(name, target) => {
                        let header = raw_header(EntryType::Symlink, name, target, 0o777, 0);
                        builder.append(&header, io::empty()).unwrap();
                    }
                    Item::Hardlink(name, target) => {
                        let header = raw_header(EntryType::Link, name, target, 0o644, 0);
                        builder.append(&header, io::empty()).unwrap();
                    }
                    Item::Fifo(name) => {
                        let header = raw_header(EntryType::Fifo, name, "", 0o644, 0);
                        builder.append(&header, io::empty()).unwrap();
                    }
                    Item::CharDevice(name) => {
                        let header = raw_header(EntryType::Char, name, "", 0o644, 0);
                        builder.append(&header, io::empty()).unwrap();
                    }
                }
            }
            builder.finish().unwrap();
        }
        match compression {
            Compression::Gzip => {
                let mut encoder =
                    GzEncoder::new(fs::File::create(path).unwrap(), GzCompression::fast());
                encoder.write_all(&plain).unwrap();
                encoder.finish().unwrap();
            }
            Compression::Xz => {
                let mut encoder = xz2::write::XzEncoder::new(fs::File::create(path).unwrap(), 1);
                encoder.write_all(&plain).unwrap();
                encoder.finish().unwrap();
            }
            Compression::Bzip2 => {
                let mut encoder = bzip2::write::BzEncoder::new(
                    fs::File::create(path).unwrap(),
                    bzip2::Compression::fast(),
                );
                encoder.write_all(&plain).unwrap();
                encoder.finish().unwrap();
            }
            Compression::None => fs::write(path, &plain).unwrap(),
        }
    }

    fn fixture_dir() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("app.tar.gz");
        let root = dir.path().join("root");
        (dir, archive, root)
    }

    fn extract(
        archive: &Path,
        compression: Compression,
        root: &Path,
        limits: Limits,
    ) -> Result<ExtractStats, String> {
        extract_archive(archive, compression, root, limits, &|| false)
    }

    // -----------------------------------------------------------------
    // Extraction safety
    // -----------------------------------------------------------------

    #[test]
    fn extracts_and_unwraps_the_single_top_level_folder() {
        let (dir, archive, root) = fixture_dir();
        build_tar(
            &archive,
            &[
                Item::Dir("App-1.0/", 0o755),
                Item::Dir("App-1.0/bin/", 0o777),
                Item::File("App-1.0/bin/app", b"#!/bin/sh\necho hi\n", 0o4755),
                Item::File("App-1.0/bin/data", b"data", 0o666),
                Item::Symlink("App-1.0/bin/alias", "app"),
                Item::Hardlink("App-1.0/bin/copy", "App-1.0/bin/app"),
            ],
            Compression::Gzip,
        );
        // setuid entry must be refused, so use a separate clean archive first.
        let error = extract(&archive, Compression::Gzip, &root, Limits::default()).unwrap_err();
        assert!(error.contains("setuid"), "{error}");
        fs::remove_dir_all(&root).unwrap();

        build_tar(
            &archive,
            &[
                Item::Dir("App-1.0/", 0o755),
                Item::Dir("App-1.0/bin/", 0o777),
                Item::File("App-1.0/bin/app", b"#!/bin/sh\n", 0o755),
                Item::File("App-1.0/bin/data", b"data", 0o666),
                Item::Symlink("App-1.0/bin/alias", "app"),
                Item::Hardlink("App-1.0/bin/copy", "App-1.0/bin/app"),
            ],
            Compression::Gzip,
        );
        let stats = extract(&archive, Compression::Gzip, &root, Limits::default()).unwrap();
        assert_eq!(stats.files, 3);
        assert_eq!(
            stats.links, 1,
            "one symbolic link (the hard link is a file)"
        );
        let top = unwrap_top_level(&root).unwrap();
        assert_eq!(top.file_name().unwrap(), "App-1.0");
        let mode = fs::metadata(top.join("bin/data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o644, "world write stripped");
        let dir_mode = fs::metadata(top.join("bin")).unwrap().permissions().mode() & 0o7777;
        assert_eq!(dir_mode, 0o755);
        assert!(is_executable_file(&top.join("bin/app")));
        assert!(top.join("bin/alias").exists());
        assert_eq!(fs::read(top.join("bin/copy")).unwrap(), b"#!/bin/sh\n");
        let _ = dir;
    }

    #[test]
    fn xz_and_bzip2_archives_are_unpacked() {
        for compression in [Compression::Xz, Compression::Bzip2, Compression::None] {
            let (dir, archive, root) = fixture_dir();
            build_tar(
                &archive,
                &[
                    Item::Dir("tool/", 0o755),
                    Item::File("tool/tool", b"binary", 0o755),
                ],
                compression,
            );
            let stats = extract(&archive, compression, &root, Limits::default()).unwrap();
            assert_eq!(stats.files, 1, "{compression:?}");
            assert_eq!(stats.dirs, 1);
            fs::remove_dir_all(&root).unwrap();
            let _ = dir;
        }
        // extract_portable derives the compression from the original file name
        // and refuses anything else before touching the tree.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("app.zip");
        fs::write(&archive, b"PK").unwrap();
        let root = dir.path().join("root");
        let error = extract_portable(&archive, "app.zip", &root, &|| false).unwrap_err();
        assert!(error.contains("not a portable archive"), "{error}");
        assert!(!root.exists());
    }

    #[test]
    fn extracts_compressed_archives_by_name() {
        for (name, compression) in [
            ("app.tar.gz", Compression::Gzip),
            ("app.tgz", Compression::Gzip),
            ("app.tar.xz", Compression::Xz),
            ("app.txz", Compression::Xz),
            ("app.tar.bz2", Compression::Bzip2),
            ("app.tbz2", Compression::Bzip2),
            ("app.tar", Compression::None),
        ] {
            assert_eq!(
                Compression::from_file_name(name),
                Some(compression),
                "{name}"
            );
        }
        assert_eq!(Compression::from_file_name("app.zip"), None);
        assert_eq!(Compression::from_file_name("app.deb"), None);
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for (name, item) in [
            ("dotdot", Item::File("../evil", b"x", 0o644)),
            ("absolute", Item::File("/etc/evil", b"x", 0o644)),
            ("nested dotdot", Item::File("pkg/../../evil", b"x", 0o644)),
            ("backslash", Item::File("pkg\\evil", b"x", 0o644)),
        ] {
            let (dir, archive, root) = fixture_dir();
            build_tar(&archive, &[item], Compression::Gzip);
            let error = extract(&archive, Compression::Gzip, &root, Limits::default()).unwrap_err();
            assert!(
                error.contains("absolute") || error.contains("leaves") || error.contains("invalid"),
                "{name}: {error}"
            );
            let _ = dir;
        }
    }

    #[test]
    fn rejects_links_that_escape_or_write_through() {
        let cases: Vec<(&str, Vec<Item<'_>>)> = vec![
            (
                "absolute symlink",
                vec![
                    Item::Dir("pkg/", 0o755),
                    Item::Symlink("pkg/l", "/etc/passwd"),
                ],
            ),
            (
                "escaping symlink",
                vec![
                    Item::Dir("pkg/", 0o755),
                    Item::Symlink("pkg/l", "../../etc"),
                ],
            ),
            (
                "physical escape",
                vec![
                    Item::Dir("pkg/", 0o755),
                    Item::Symlink("pkg/s", "."),
                    Item::Symlink("pkg/esc", "s/.."),
                ],
            ),
            (
                "write through link",
                vec![
                    Item::Dir("pkg/", 0o755),
                    Item::Dir("pkg/d/", 0o755),
                    Item::Symlink("pkg/l", "d"),
                    Item::File("pkg/l/x", b"x", 0o644),
                ],
            ),
            (
                "hardlink outside",
                vec![
                    Item::Dir("pkg/", 0o755),
                    Item::Hardlink("pkg/h", "../etc/passwd"),
                ],
            ),
            ("fifo", vec![Item::Dir("pkg/", 0o755), Item::Fifo("pkg/f")]),
            (
                "device",
                vec![Item::Dir("pkg/", 0o755), Item::CharDevice("pkg/d")],
            ),
            (
                "dangling symlink",
                vec![Item::Dir("pkg/", 0o755), Item::Symlink("pkg/l", "missing")],
            ),
        ];
        for (name, items) in cases {
            let (dir, archive, root) = fixture_dir();
            build_tar(&archive, &items, Compression::Gzip);
            let result = extract(&archive, Compression::Gzip, &root, Limits::default());
            assert!(result.is_err(), "{name} was accepted");
            let _ = dir;
        }
    }

    #[test]
    fn refuses_too_many_entries_and_too_much_data() {
        let (dir, archive, root) = fixture_dir();
        let data = [0u8; 512];
        let items: Vec<Item<'_>> = (0..20)
            .map(|index| {
                Item::File(
                    Box::leak(format!("f{index}").into_boxed_str()),
                    &data,
                    0o644,
                )
            })
            .collect();
        build_tar(&archive, &items, Compression::Gzip);
        let limits = Limits {
            max_entries: 10,
            max_bytes: 1024 * 1024,
        };
        let error = extract(&archive, Compression::Gzip, &root, limits).unwrap_err();
        assert!(error.contains("more than"), "{error}");
        fs::remove_dir_all(&root).unwrap();
        let limits = Limits {
            max_entries: 100,
            max_bytes: 1024,
        };
        let error = extract(&archive, Compression::Gzip, &root, limits).unwrap_err();
        assert!(error.contains("size limit"), "{error}");
        let _ = dir;
    }

    #[test]
    fn expansion_limits_follow_the_compressed_size() {
        assert_eq!(
            Limits::for_compressed(10).max_bytes,
            MAX_EXPANDED_BYTES,
            "small archives still get the 4 GiB floor"
        );
        assert_eq!(Limits::for_compressed(1024).max_bytes, MAX_EXPANDED_BYTES);
        assert_eq!(
            Limits::for_compressed(2 * 1024 * 1024 * 1024).max_bytes,
            16 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn trees_are_sanitised_after_unpacking() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let file = dir.path().join("sub/file");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(dir.path().join("sub"), fs::Permissions::from_mode(0o777)).unwrap();
        sanitize_tree(dir.path()).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            fs::metadata(dir.path().join("sub"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }

    #[test]
    fn sanitise_refuses_setuid_files_in_appimage_trees() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("chrome-sandbox");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).unwrap();
        let error = sanitize_tree(dir.path()).unwrap_err();
        assert!(error.contains("setuid"), "{error}");
    }

    #[test]
    fn sanitise_refuses_links_leaving_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let error = sanitize_tree(dir.path()).unwrap_err();
        assert!(error.contains("outside"), "{error}");
    }

    // -----------------------------------------------------------------
    // Desktop entries and commands
    // -----------------------------------------------------------------

    #[test]
    fn desktop_entries_are_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.desktop");
        fs::write(
            &path,
            "# comment\n[Desktop Entry]\nType=Application\nName=My App\nComment=Does things\n\
             Exec=my-app --flag %U\nIcon=my-app\nTerminal=false\nPath=/opt/my app\n\
             X-AppImage-Version=1.2.3\nNoDisplay=false\n\n[Desktop Action new]\nName=New\nExec=my-app --new\n",
        )
        .unwrap();
        let entry = parse_desktop_entry(&path).unwrap();
        assert_eq!(entry.name.as_deref(), Some("My App"));
        assert_eq!(entry.comment.as_deref(), Some("Does things"));
        assert_eq!(entry.exec.as_deref(), Some("my-app --flag %U"));
        assert_eq!(entry.icon.as_deref(), Some("my-app"));
        assert_eq!(entry.working_dir.as_deref(), Some("/opt/my app"));
        assert_eq!(entry.appimage_version.as_deref(), Some("1.2.3"));
        assert!(!entry.terminal);
        assert!(desktop_entry_usable(&entry));

        let hidden = dir.path().join("hidden.desktop");
        fs::write(
            &hidden,
            "[Desktop Entry]\nName=Hidden\nNoDisplay=true\nExec=x\n",
        )
        .unwrap();
        let hidden = parse_desktop_entry(&hidden).unwrap();
        assert!(!desktop_entry_usable(&hidden));
        assert!(parse_desktop_entry(&dir.path().join("missing.desktop")).is_none());
    }

    #[test]
    fn exec_values_become_argv_without_field_codes() {
        assert_eq!(exec_argv("my-app --flag %U"), vec!["my-app", "--flag"]);
        assert_eq!(
            exec_argv(r#""/opt/my app/bin/app" --name="a b" 'x y'"#),
            vec!["/opt/my app/bin/app", "--name=a b", "x y"]
        );
        assert_eq!(exec_argv("app %%literal").last().unwrap(), "%literal");
        assert_eq!(exec_argv("app --file=%f"), vec!["app", "--file="]);
        assert_eq!(exec_argv("app --file=%F%u"), vec!["app", "--file="]);
        assert!(exec_argv("   ").is_empty());
        assert_eq!(exec_argv("app \"unterminated"), vec!["app", "unterminated"]);
    }

    #[test]
    fn desktop_entry_values_are_unescaped() {
        assert_eq!(unescape_desktop_value("A\\sB\\tC"), "A B\tC");
        assert_eq!(unescape_desktop_value("x\\nY"), "x\nY");
        assert_eq!(unescape_desktop_value("100%%"), "100%%");
    }

    // -----------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------

    fn write_tree(root: &Path, files: &[(&str, &[u8], u32)]) {
        fs::create_dir_all(root).unwrap();
        for (relative, data, mode) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, data).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
        }
    }

    fn fake_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn archives_with_a_desktop_entry_use_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        let icon = fake_png(128, 128);
        write_tree(
            &root,
            &[
                ("bin/example", b"\x7fELF-rest", 0o755),
                (
                    "example.desktop",
                    b"[Desktop Entry]\nType=Application\nName=Example\n\
                      Comment=Example app\nExec=bin/example --hello %U\nIcon=example\n",
                    0o644,
                ),
                (
                    "usr/share/icons/hicolor/128x128/apps/example.png",
                    &icon,
                    0o644,
                ),
            ],
        );
        let discovery = discover_archive(&root, "example");
        let launcher = discovery.launcher.as_ref().expect("launcher");
        assert_eq!(launcher.name.as_deref(), Some("Example"));
        assert_eq!(launcher.comment.as_deref(), Some("Example app"));
        assert_eq!(launcher.desktop_entry.as_deref(), Some("example.desktop"));
        assert_eq!(
            launcher.argv,
            vec![
                root.join("bin/example").display().to_string(),
                "--hello".to_string()
            ]
        );
        assert!(!discovery.launch_guess);
        assert!(!discovery.ambiguous);
        let icon = discovery.icon.expect("icon");
        assert_eq!(icon.kind, "png");
        assert_eq!(
            png_dimensions(&root.join("usr/share/icons/hicolor/128x128/apps/example.png")),
            Some((128, 128))
        );
    }

    #[test]
    fn archives_without_a_desktop_entry_guess_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tool");
        write_tree(
            &root,
            &[
                ("tool", b"\x7fELF-tool", 0o755),
                ("chrome-sandbox", b"\x7fELF-sandbox", 0o4755),
                ("lib/tool.so", b"shared", 0o755),
                ("README.txt", b"docs", 0o644),
            ],
        );
        // The setuid helper is skipped, never picked nor executed.
        let discovery = discover_archive(&root, "tool");
        let launcher = discovery.launcher.as_ref().expect("launcher");
        assert_eq!(launcher.argv, vec![root.join("tool").display().to_string()]);
        assert!(discovery.launch_guess);
        // The sandbox helper marks a Chromium-based tree, and is never the program.
        assert!(discovery.chromium);
        assert!(!discovery.source_like);
        assert!(!launcher.argv[0].contains("chrome-sandbox"));
    }

    #[test]
    fn equally_plausible_programs_need_a_choice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("multi");
        write_tree(
            &root,
            &[
                ("alpha", b"\x7fELF-alpha", 0o755),
                ("beta", b"\x7fELF-beta", 0o755),
            ],
        );
        let discovery = discover_archive(&root, "multi");
        assert!(discovery.ambiguous, "two equal programs");
        assert!(discovery.candidates.len() >= 2);
        assert!(discovery
            .candidates
            .iter()
            .any(|candidate| candidate.name.as_deref() == Some("Alpha")));
    }

    #[test]
    fn chromium_is_detected_from_its_sandbox_helper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("browser");
        write_tree(
            &root,
            &[
                ("chrome", b"\x7fELF-chrome", 0o755),
                ("chrome-sandbox", b"\x7fELF-sandbox", 0o755),
            ],
        );
        let discovery = discover_archive(&root, "chrome");
        assert!(discovery.chromium);
    }

    #[test]
    fn source_archives_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        write_tree(
            &root,
            &[
                (
                    "CMakeLists.txt",
                    b"cmake_minimum_required(VERSION 3.0)\n",
                    0o644,
                ),
                ("configure", b"#!/bin/sh\n", 0o755),
                ("src/main.c", b"int main(){return 0;}\n", 0o644),
                ("src/util.c", b"int util(){return 0;}\n", 0o644),
            ],
        );
        let discovery = discover_archive(&root, "src");
        assert!(discovery.source_like);
        assert!(discovery.launcher.is_none());
        assert_eq!(discovery.rejection(), SOURCE_ARCHIVE_MESSAGE);
    }

    #[test]
    fn desktop_files_are_ranked_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        write_tree(
            &root,
            &[
                ("bin/demo", b"\x7fELF-demo", 0o755),
                (
                    "usr/share/applications/demo-url-handler.desktop",
                    b"[Desktop Entry]\nName=URL handler\nExec=bin/demo %u\n",
                    0o644,
                ),
                (
                    "usr/share/applications/demo.desktop",
                    b"[Desktop Entry]\nName=Demo\nExec=bin/demo\n",
                    0o644,
                ),
            ],
        );
        let discovery = discover_archive(&root, "demo-1.0");
        let launcher = discovery.launcher.as_ref().expect("launcher");
        assert_eq!(launcher.name.as_deref(), Some("Demo"));
        assert_eq!(
            launcher.desktop_entry.as_deref(),
            Some("usr/share/applications/demo.desktop")
        );
    }

    #[test]
    fn launcher_rebasing_moves_paths_to_the_install_dir() {
        let mut launcher = Launcher {
            argv: vec![
                "/tmp/stage/app/bin/tool".to_string(),
                "--data=/tmp/stage/app/share".to_string(),
            ],
            desktop_entry: Some("tool.desktop".to_string()),
            working_dir: Some("/tmp/stage/app".to_string()),
            ..Launcher::default()
        };
        launcher.rebase(Path::new("/tmp/stage/app"), Path::new("/data/apps/tool/1"));
        assert_eq!(
            launcher.argv,
            vec![
                "/data/apps/tool/1/bin/tool".to_string(),
                "--data=/tmp/stage/app/share".to_string()
            ]
        );
        assert_eq!(launcher.working_dir.as_deref(), Some("/data/apps/tool/1"));
        assert_eq!(launcher.desktop_entry.as_deref(), Some("tool.desktop"));
    }

    #[test]
    fn installed_packages_are_inspected() {
        let dir = tempfile::tempdir().unwrap();
        let desktop_dir = dir.path().join("usr/share/applications");
        fs::create_dir_all(&desktop_dir).unwrap();
        let icon = fake_png(256, 256);
        let icon_path = dir
            .path()
            .join("usr/share/icons/hicolor/256x256/apps/example.png");
        fs::create_dir_all(icon_path.parent().unwrap()).unwrap();
        fs::write(&icon_path, &icon).unwrap();
        let program = dir.path().join("usr/bin/example");
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        fs::write(&program, b"\x7fELF-example").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let desktop = desktop_dir.join("example.desktop");
        fs::write(
            &desktop,
            "[Desktop Entry]\nType=Application\nName=Example\nComment=A demo\n\
             Exec=example --flag %U\nIcon=example\n",
        )
        .unwrap();
        let files = vec![
            desktop.clone(),
            icon_path.clone(),
            program.clone(),
            dir.path().join("opt/example/chrome-sandbox"),
        ];
        let discovery = discover_installed_package("example-package", &files);
        let launcher = discovery.launcher.expect("launcher");
        assert_eq!(launcher.name.as_deref(), Some("Example"));
        assert_eq!(
            launcher.desktop_entry.as_deref(),
            Some(desktop.to_str().unwrap())
        );
        assert_eq!(launcher.argv[0], program.display().to_string());
        assert_eq!(launcher.argv[1], "--flag");
        assert_eq!(discovery.icon.expect("icon").kind, "png");
        assert!(discovery.chromium);
    }

    #[test]
    fn programs_are_scored_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("x");
        write_tree(
            &root,
            &[
                ("app-1.0", b"\x7fELF", 0o755),
                ("crashpad_handler", b"\x7fELF", 0o755),
                ("setup.sh", b"#!/bin/sh\n", 0o755),
                ("lib.so", b"\x7fELF", 0o755),
            ],
        );
        let programs = find_programs(&root, "app", 4);
        assert!(!programs.is_empty());
        assert!(
            programs
                .iter()
                .all(|program| !program.relative.contains("crashpad_handler")),
            "helper must be skipped"
        );
        assert!(programs.iter().all(|program| program.relative != "lib.so"));
    }

    #[test]
    fn base_names_are_collected() {
        let names = base_names(&[
            PathBuf::from("/x/a"),
            PathBuf::from("/y/b"),
            PathBuf::from("/z/a"),
        ]);
        assert_eq!(names.len(), 2);
        assert!(names.contains("a") && names.contains("b"));
    }

    // -----------------------------------------------------------------
    // ELF and AppImage metadata
    // -----------------------------------------------------------------

    fn fake_elf_header(width: u16, machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2; // 64-bit
        header[5] = 1; // little endian
        header[6] = 1;
        header[16..18].copy_from_slice(&width.to_le_bytes());
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header
    }

    #[test]
    fn elf_architectures_are_read() {
        let dir = tempfile::tempdir().unwrap();
        for (machine, architecture) in [(62u16, "amd64"), (183, "arm64"), (3, "i386")] {
            let path = dir.path().join(format!("elf-{machine}"));
            fs::write(&path, fake_elf_header(62, machine)).unwrap();
            assert!(is_elf(&path));
            assert_eq!(elf_architecture(&path), Some(architecture));
        }
        let text = dir.path().join("text");
        fs::write(&text, b"not an elf").unwrap();
        assert!(!is_elf(&text));
        assert_eq!(elf_architecture(&text), None);
    }

    /// Build a minimal 64-bit little-endian ELF file with a `.upd_info`
    /// section so the section-table reader can be tested without an AppImage.
    fn fake_appimage(update_info: &str) -> Vec<u8> {
        let header_size = 64u32;
        let section_size = 64u16;
        let shoff = u64::from(header_size);
        let shstrtab_offset = shoff + u64::from(section_size) * 3;
        let mut shstrtab = Vec::new();
        shstrtab.push(0);
        let shstrtab_name = 1u32;
        shstrtab.extend_from_slice(b".shstrtab\0");
        let upd_name = shstrtab.len() as u32;
        shstrtab.extend_from_slice(b".upd_info\0");
        let upd_offset = shstrtab_offset + shstrtab.len() as u64;
        let data = update_info.as_bytes();
        let file_size = upd_offset + data.len() as u64;

        let mut file = vec![0u8; header_size as usize];
        file[..4].copy_from_slice(b"\x7fELF");
        file[4] = 2;
        file[5] = 1;
        file[6] = 1;
        file[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        file[18..20].copy_from_slice(&62u16.to_le_bytes()); // amd64
        file[20..24].copy_from_slice(&1u32.to_le_bytes());
        file[40..48].copy_from_slice(&shoff.to_le_bytes());
        file[52..54].copy_from_slice(&(header_size as u16).to_le_bytes());
        file[58..60].copy_from_slice(&section_size.to_le_bytes());
        file[60..62].copy_from_slice(&3u16.to_le_bytes());
        file[62..64].copy_from_slice(&1u16.to_le_bytes());

        let section = |name: u32, kind: u32, offset: u64, size: u64| {
            let mut bytes = [0u8; 64];
            bytes[0..4].copy_from_slice(&name.to_le_bytes());
            bytes[4..8].copy_from_slice(&kind.to_le_bytes());
            bytes[24..32].copy_from_slice(&offset.to_le_bytes());
            bytes[32..40].copy_from_slice(&size.to_le_bytes());
            bytes
        };
        // Section 0: null.
        file.extend_from_slice(&[0u8; 64]);
        // Section 1: .shstrtab.
        file.extend_from_slice(&section(
            shstrtab_name,
            3,
            shstrtab_offset,
            shstrtab.len() as u64,
        ));
        // Section 2: .upd_info.
        file.extend_from_slice(&section(upd_name, 1, upd_offset, data.len() as u64));
        file.extend_from_slice(&shstrtab);
        file.extend_from_slice(data);
        assert_eq!(file.len() as u64, file_size);
        file
    }

    #[test]
    fn appimage_update_information_is_read_from_the_elf_sections() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "gh-releases-zsync|owner|repo|latest|App-*.AppImage.zsync";
        let image = dir.path().join("App-1.0.AppImage");
        fs::write(&image, fake_appimage(raw)).unwrap();
        assert_eq!(appimage_update_info(&image).as_deref(), Some(raw));
        let update = parse_update_info(raw).unwrap();
        assert_eq!(update["kind"], "github");
        assert_eq!(update["owner"], "owner");
        assert_eq!(update["file"], "App-*.AppImage.zsync");

        let missing = dir.path().join("plain");
        fs::write(&missing, b"not an elf").unwrap();
        assert_eq!(appimage_update_info(&missing), None);
        // A truncated but well-formed ELF header must not panic.
        let truncated = dir.path().join("truncated");
        fs::write(&truncated, fake_elf_header(2, 62)).unwrap();
        assert_eq!(appimage_update_info(&truncated), None);
    }

    #[test]
    fn appimage_update_information_variants_are_parsed() {
        assert_eq!(
            parse_update_info("zsync|https://example.org/app.zsync").unwrap()["kind"],
            "zsync"
        );
        assert!(parse_update_info("zsync|http://example.org/app.zsync").is_none());
        assert!(parse_update_info("unknown|stuff").is_none());
        assert!(parse_update_info("").is_none());
        assert!(parse_update_info("gh-releases-zsync|only|four").is_none());
    }

    #[test]
    fn appimage_discovery_uses_apprun_and_desktop_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("squashfs-root");
        write_tree(
            &root,
            &[
                ("AppRun", b"\x7fELF-apprun", 0o755),
                (
                    "app.desktop",
                    b"[Desktop Entry]\nType=Application\nName=Demo Image\n\
                      Exec=AppRun --demo\nIcon=demo\nX-AppImage-Version=2.5.1\n",
                    0o644,
                ),
                ("demo.png", &fake_png(256, 256), 0o644),
            ],
        );
        let discovery = discover_appimage(&root, "Demo-2.5.1-x86_64.AppImage").unwrap();
        assert_eq!(discovery.launcher.name.as_deref(), Some("Demo Image"));
        assert_eq!(
            discovery.launcher.argv,
            vec![
                root.join("AppRun").display().to_string(),
                "--demo".to_string()
            ]
        );
        assert_eq!(
            discovery.entry.unwrap().appimage_version.as_deref(),
            Some("2.5.1")
        );
        assert!(discovery.icon.is_some());
        assert!(discover_appimage(dir.path(), "x.AppImage").is_err());
    }

    #[test]
    fn icons_are_scored_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.png");
        fs::write(&small, fake_png(32, 32)).unwrap();
        let medium = dir.path().join("medium.png");
        fs::write(&medium, fake_png(256, 256)).unwrap();
        let large = dir.path().join("large.png");
        fs::write(&large, fake_png(1024, 1024)).unwrap();
        let svg = dir.path().join("icon.svg");
        fs::write(&svg, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        assert!(icon_score(&medium) > icon_score(&small));
        assert!(icon_score(&medium) > icon_score(&svg));
        assert!(icon_score(&svg) > icon_score(&large));
        let found = find_icon("medium", &[small.clone(), medium.clone()]).unwrap();
        assert_eq!(found, medium);
        assert_eq!(find_icon("missing", &[dir.path().to_path_buf()]), None);
        assert_eq!(
            find_icon("large", &[dir.path().to_path_buf()]).unwrap(),
            large
        );

        let icon = read_icon(&medium).unwrap();
        assert_eq!(icon.kind, "png");
        assert!(icon.to_json()["data"]
            .as_str()
            .unwrap()
            .starts_with("iVBOR"));
        let fake = dir.path().join("fake.png");
        fs::write(&fake, b"<html>").unwrap();
        assert!(read_icon(&fake).is_none());
        let big = dir.path().join("big.png");
        fs::write(&big, vec![0u8; (MAX_ICON_BYTES + 1) as usize]).unwrap();
        assert!(read_icon(&big).is_none());
    }

    #[test]
    fn png_dimensions_read_the_ihdr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.png");
        fs::write(&path, fake_png(300, 200)).unwrap();
        assert_eq!(png_dimensions(&path), Some((300, 200)));
        let bad = dir.path().join("bad.png");
        fs::write(&bad, b"nope").unwrap();
        assert_eq!(png_dimensions(&bad), None);
    }

    #[test]
    fn launcher_validation_checks_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        fs::create_dir_all(root.join("bin")).unwrap();
        let program = root.join("bin/app");
        fs::write(&program, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = Launcher {
            argv: vec![program.display().to_string()],
            ..Launcher::default()
        };
        validate_launcher(&root, &launcher).unwrap();

        let missing = Launcher {
            argv: vec![root.join("bin/missing").display().to_string()],
            ..Launcher::default()
        };
        assert!(validate_launcher(&root, &missing).is_err());

        let outside = Launcher {
            argv: vec!["/bin/sh".to_string()],
            ..Launcher::default()
        };
        assert!(validate_launcher(&root, &outside).is_err());
    }

    #[test]
    fn safe_relative_paths_are_checked() {
        assert!(is_safe_relative("bin/app"));
        assert!(!is_safe_relative("../app"));
        assert!(!is_safe_relative("/bin/app"));
        assert!(!is_safe_relative("bin/../../app"));
        assert!(!is_safe_relative(""));
    }

    #[test]
    fn titles_come_from_program_names() {
        assert_eq!(
            title_from_program(Path::new("/x/my-cool_app.bin")),
            "My Cool App"
        );
        assert_eq!(title_from_program(Path::new("/x/firefox")), "Firefox");
    }

    #[test]
    fn desktop_summaries_are_json() {
        let entry = DesktopEntry {
            path: PathBuf::from("/x.app.desktop"),
            name: Some("X".into()),
            ..DesktopEntry::default()
        };
        assert_eq!(desktop_entry_summary(&entry)["name"], "X");
    }
}
