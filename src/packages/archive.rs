//! Safe extraction of `tar.gz`, `tar.xz` and `zip` artifacts into a fresh
//! staging directory.
//!
//! All formats share the same rules:
//!
//! * Paths are normalised (`./` removed), `..` and absolute paths are refused,
//!   and `stripComponents` leading directories are dropped.
//! * Only regular files, directories, symbolic links and hard links are
//!   accepted; devices, FIFOs and other special entries are refused.
//! * Nothing is ever written through a link: every ancestor of an entry must
//!   be a real directory inside the staging root.
//! * Link targets must stay inside the root, checked lexically while
//!   extracting and physically (canonical paths) once everything exists.
//! * setuid/setgid/sticky bits and group/other write bits are dropped.
//! * Entry count and total size are bounded.

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::read::GzDecoder;
use tar::EntryType;
use xz2::read::XzDecoder;
use zip::ZipArchive;

pub const MAX_ENTRIES: usize = 200_000;
/// Upper bound on the bytes one artifact may expand to. Measured with
/// `LLVM-23.1.5-Linux-X64.tar.xz` (2,014,093,084 bytes compressed): the tar
/// stream is 12,413,547,008 bytes and its regular files total 12,405,009,493
/// bytes (~11.55 GiB). The bound is 16 GiB, a ~40% margin over that; the
/// installer still refuses to start without enough free disk, aborts when the
/// room left on the filesystem runs out, and `catalog verify` uses the
/// filesystem's actual free space as its room, so the constant is only the
/// decompression-bomb ceiling.
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Largest GNU long-name/long-link or PAX extension record accepted. The tar
/// reader buffers these records whole, so they are checked before extraction.
pub const MAX_EXTENSION_BYTES: u64 = 64 * 1024;
/// Longest symbolic-link target accepted (the link name is one path).
pub const MAX_SYMLINK_TARGET_BYTES: u64 = 4096;

#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractStats {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
}

/// Normalise an archive path into safe relative components (after stripping).
fn archive_components(raw: &[u8], strip: u32) -> Result<Option<Vec<String>>, String> {
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
    if parts.len() <= strip as usize {
        return Ok(None);
    }
    Ok(Some(parts.split_off(strip as usize)))
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
    let mut p = root.to_path_buf();
    for part in parts {
        p.push(part);
    }
    p
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|e| format!("cannot create directory: {e}"))?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).ok();
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", current.display())),
        }
    }
    Ok(())
}

/// Remove an earlier entry at the same path (tar semantics: last one wins),
/// but never a directory.
fn clear_existing(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(format!(
            "archive replaces a directory with a file: {}",
            path.display()
        )),
        Ok(_) => fs::remove_file(path).map_err(|e| format!("cannot replace entry: {e}")),
        Err(_) => Ok(()),
    }
}

/// Header-only pass over the archive: refuse extension records (GNU long
/// names, PAX headers) larger than [`MAX_EXTENSION_BYTES`] before the tar
/// reader would buffer them in memory, and absurd entry counts.
fn prescan_tar<R: Read>(reader: R, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let mut tar = tar::Archive::new(reader);
    let entries = tar
        .entries()
        .map_err(|e| format!("cannot read archive: {e}"))?
        .raw(true);
    for (count, entry) in entries.enumerate() {
        let entry = entry.map_err(|e| format!("archive is corrupt: {e}"))?;
        // Raw entries include the extension records themselves.
        if count > MAX_ENTRIES * 3 {
            return Err(format!("archive has more than {MAX_ENTRIES} entries"));
        }
        if count % 2000 == 0 && cancelled() {
            return Err("cancelled".into());
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
                "archive has a {} byte name/extension record (limit {MAX_EXTENSION_BYTES})",
                entry.size()
            ));
        }
    }
    Ok(())
}

/// Header-only pass over a gzip-compressed archive.
fn prescan_tar_gz(archive: &Path, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("cannot open download: {e}"))?;
    prescan_tar(GzDecoder::new(io::BufReader::new(file)), cancelled)
}

/// Header-only pass over an xz-compressed archive.
fn prescan_tar_xz(archive: &Path, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("cannot open download: {e}"))?;
    prescan_tar(XzDecoder::new(io::BufReader::new(file)), cancelled)
}

/// Extract `archive` (gzip-compressed tar) into the empty directory `root`.
/// At most `room` bytes (and never more than [`MAX_TOTAL_BYTES`]) are
/// written, so an archive cannot fill the disk.
pub fn extract_tar_gz(
    archive: &Path,
    root: &Path,
    strip: u32,
    room: u64,
    progress: &mut dyn FnMut(&ExtractStats),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    prescan_tar_gz(archive, cancelled)?;
    let file = fs::File::open(archive).map_err(|e| format!("cannot open download: {e}"))?;
    extract_tar(
        GzDecoder::new(io::BufReader::new(file)),
        root,
        strip,
        room,
        progress,
        cancelled,
    )
}

/// Extract `archive` (xz-compressed tar) into the empty directory `root`,
/// with the same rules as [`extract_tar_gz`].
pub fn extract_tar_xz(
    archive: &Path,
    root: &Path,
    strip: u32,
    room: u64,
    progress: &mut dyn FnMut(&ExtractStats),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    prescan_tar_xz(archive, cancelled)?;
    let file = fs::File::open(archive).map_err(|e| format!("cannot open download: {e}"))?;
    extract_tar(
        XzDecoder::new(io::BufReader::new(file)),
        root,
        strip,
        room,
        progress,
        cancelled,
    )
}

fn extract_tar<R: Read>(
    reader: R,
    root: &Path,
    strip: u32,
    room: u64,
    progress: &mut dyn FnMut(&ExtractStats),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    let limit = room.min(MAX_TOTAL_BYTES);
    let mut tar = tar::Archive::new(reader);
    tar.set_preserve_permissions(false);
    let mut stats = ExtractStats::default();
    let mut links: Vec<PathBuf> = Vec::new();
    let mut count = 0usize;
    let entries = tar
        .entries()
        .map_err(|e| format!("cannot read archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("archive is corrupt: {e}"))?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(format!("archive has more than {MAX_ENTRIES} entries"));
        }
        if count.is_multiple_of(500) {
            if cancelled() {
                return Err("cancelled".into());
            }
            progress(&stats);
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
        let path_bytes = entry.path_bytes().into_owned();
        let Some(parts) = archive_components(&path_bytes, strip)? else {
            continue;
        };
        let dest = join(root, &parts);
        ensure_parents(root, &parts)?;
        let mode = entry.header().mode().unwrap_or(0o644);
        match kind {
            EntryType::Directory => {
                match fs::symlink_metadata(&dest) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(format!(
                            "archive replaces a file with a directory: {}",
                            parts.join("/")
                        ))
                    }
                    Err(_) => fs::create_dir(&dest)
                        .map_err(|e| format!("cannot create directory: {e}"))?,
                }
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).ok();
                stats.dirs += 1;
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.size();
                stats.bytes += size;
                if stats.bytes > limit {
                    return Err(if limit < MAX_TOTAL_BYTES {
                        "not enough free disk space to extract the archive".into()
                    } else {
                        "archive expands beyond the size limit".into()
                    });
                }
                clear_existing(&dest)?;
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|e| format!("cannot create {}: {e}", parts.join("/")))?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|e| format!("cannot extract {}: {e}", parts.join("/")))?;
                if copied != size {
                    return Err(format!("archive entry {} is truncated", parts.join("/")));
                }
                // Keep the file's time: tools compare them (Flutter rebuilds
                // its tool when pubspec.yaml is newer than pubspec.lock,
                // Python's .pyc files are tied to their sources' times).
                if let Some(when) = entry.header().mtime().ok().and_then(archive_time) {
                    out.set_modified(when).ok();
                }
                // Keep execute bits; drop setuid/setgid/sticky and group/other write.
                let safe = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                fs::set_permissions(&dest, fs::Permissions::from_mode(safe)).ok();
                stats.files += 1;
            }
            EntryType::Symlink => {
                let target_bytes = entry
                    .link_name_bytes()
                    .ok_or("symbolic link without a target")?
                    .into_owned();
                let target = std::str::from_utf8(&target_bytes)
                    .map_err(|_| "symbolic link with a non-UTF-8 target".to_string())?
                    .to_string();
                if lexical_target(&parts[..parts.len() - 1], &target).is_none() {
                    return Err(format!(
                        "symbolic link {} points outside the package ({target})",
                        parts.join("/")
                    ));
                }
                clear_existing(&dest)?;
                std::os::unix::fs::symlink(&target, &dest)
                    .map_err(|e| format!("cannot create link {}: {e}", parts.join("/")))?;
                links.push(dest);
                stats.links += 1;
            }
            EntryType::Link => {
                let target_bytes = entry
                    .link_name_bytes()
                    .ok_or("hard link without a target")?
                    .into_owned();
                let Some(target_parts) = archive_components(&target_bytes, strip)? else {
                    return Err(format!("hard link {} has no target", parts.join("/")));
                };
                ensure_parents(root, &target_parts)?;
                let source = join(root, &target_parts);
                let meta = fs::symlink_metadata(&source)
                    .map_err(|_| format!("hard link {} targets a missing file", parts.join("/")))?;
                if !meta.is_file() {
                    return Err(format!(
                        "hard link {} must target a regular file",
                        parts.join("/")
                    ));
                }
                clear_existing(&dest)?;
                fs::hard_link(&source, &dest)
                    .map_err(|e| format!("cannot create hard link {}: {e}", parts.join("/")))?;
                stats.files += 1;
            }
            other => {
                return Err(format!(
                    "archive entry {} has an unsupported type ({other:?})",
                    parts.join("/")
                ))
            }
        }
    }
    verify_links(root, &links)?;
    progress(&stats);
    Ok(stats)
}

/// An archive entry's modification time as a file time, except 0 and times
/// in the future (those files keep the extraction time).
fn archive_time(secs: u64) -> Option<SystemTime> {
    let when = UNIX_EPOCH.checked_add(Duration::from_secs(secs))?;
    (secs > 0 && when <= SystemTime::now()).then_some(when)
}

/// Physical check once every entry exists: each link resolves inside root.
fn verify_links(root: &Path, links: &[PathBuf]) -> Result<(), String> {
    let root = fs::canonicalize(root).map_err(|e| format!("cannot resolve staging root: {e}"))?;
    for link in links {
        let resolved = fs::canonicalize(link).map_err(|_| {
            format!(
                "symbolic link {} does not resolve to a file in the package",
                rel(&root, link)
            )
        })?;
        if !resolved.starts_with(&root) {
            return Err(format!(
                "symbolic link {} resolves outside the package",
                rel(&root, link)
            ));
        }
    }
    Ok(())
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Extract one archive in the manifest's declared format.
pub fn extract_archive(
    archive: &Path,
    root: &Path,
    format: &str,
    strip: u32,
    room: u64,
    progress: &mut dyn FnMut(&ExtractStats),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    match format {
        "tar.gz" => extract_tar_gz(archive, root, strip, room, progress, cancelled),
        "tar.xz" => extract_tar_xz(archive, root, strip, room, progress, cancelled),
        "zip" => extract_zip(archive, root, strip, room, progress, cancelled),
        other => Err(format!(
            "archive format {other:?} is not supported (tar.gz, tar.xz, zip)"
        )),
    }
}

/// Extract `archive` (a ZIP file) into the empty directory `root` with the
/// same path, link and size rules as [`extract_tar_gz`].
pub fn extract_zip(
    archive: &Path,
    root: &Path,
    strip: u32,
    room: u64,
    progress: &mut dyn FnMut(&ExtractStats),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats, String> {
    let limit = room.min(MAX_TOTAL_BYTES);
    let file = fs::File::open(archive).map_err(|e| format!("cannot open download: {e}"))?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("cannot read zip archive: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("archive has more than {MAX_ENTRIES} entries"));
    }
    let mut stats = ExtractStats::default();
    let mut links: Vec<PathBuf> = Vec::new();
    for index in 0..zip.len() {
        if index.is_multiple_of(500) {
            if cancelled() {
                return Err("cancelled".into());
            }
            progress(&stats);
        }
        let mut entry = zip
            .by_index(index)
            .map_err(|e| format!("archive is corrupt: {e}"))?;
        if entry.encrypted() {
            return Err("archive contains an encrypted entry".into());
        }
        let name = entry
            .name()
            .map_err(|_| "archive contains a non-UTF-8 path".to_string())?
            .into_owned();
        let Some(parts) = archive_components(name.as_bytes(), strip)? else {
            continue;
        };
        let dest = join(root, &parts);
        ensure_parents(root, &parts)?;
        let mode = entry.unix_mode();
        let file_type = mode.map(|mode| mode & 0o170000);
        if entry.is_dir() || file_type == Some(0o040000) {
            match fs::symlink_metadata(&dest) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "archive replaces a file with a directory: {}",
                        parts.join("/")
                    ))
                }
                Err(_) => {
                    fs::create_dir(&dest).map_err(|e| format!("cannot create directory: {e}"))?
                }
            }
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).ok();
            stats.dirs += 1;
            continue;
        }
        if entry.is_symlink() || file_type == Some(0o120000) {
            let target = read_zip_link_target(&mut entry, &parts)?;
            if lexical_target(&parts[..parts.len() - 1], &target).is_none() {
                return Err(format!(
                    "symbolic link {} points outside the package ({target})",
                    parts.join("/")
                ));
            }
            clear_existing(&dest)?;
            std::os::unix::fs::symlink(&target, &dest)
                .map_err(|e| format!("cannot create link {}: {e}", parts.join("/")))?;
            links.push(dest);
            stats.links += 1;
            continue;
        }
        if !matches!(file_type, None | Some(0o100000)) {
            return Err(format!(
                "archive entry {} has an unsupported type",
                parts.join("/")
            ));
        }
        // A regular file.
        let size = entry.size();
        stats.bytes += size;
        if stats.bytes > limit {
            return Err(if limit < MAX_TOTAL_BYTES {
                "not enough free disk space to extract the archive".into()
            } else {
                "archive expands beyond the size limit".into()
            });
        }
        clear_existing(&dest)?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(|e| format!("cannot create {}: {e}", parts.join("/")))?;
        let copied = io::copy(&mut (&mut entry).take(size), &mut out)
            .map_err(|e| format!("cannot extract {}: {e}", parts.join("/")))?;
        if copied != size {
            return Err(format!("archive entry {} is truncated", parts.join("/")));
        }
        // Keep execute bits; drop setuid/setgid/sticky and group/other write.
        let safe = if mode.is_some_and(|mode| mode & 0o111 != 0) {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&dest, fs::Permissions::from_mode(safe)).ok();
        stats.files += 1;
    }
    verify_links(root, &links)?;
    progress(&stats);
    Ok(stats)
}

fn read_zip_link_target(
    entry: &mut zip::read::ZipFile<'_, fs::File>,
    parts: &[String],
) -> Result<String, String> {
    let mut target = Vec::new();
    (&mut *entry)
        .take(MAX_SYMLINK_TARGET_BYTES + 1)
        .read_to_end(&mut target)
        .map_err(|e| format!("cannot read link {}: {e}", parts.join("/")))?;
    if target.len() as u64 > MAX_SYMLINK_TARGET_BYTES {
        return Err(format!(
            "symbolic link {} has an oversized target",
            parts.join("/")
        ));
    }
    String::from_utf8(target)
        .map_err(|_| format!("symbolic link {} has a non-UTF-8 target", parts.join("/")))
}

/// `true` when `path` (inside `root`) resolves to an executable regular file
/// inside `root`.
pub fn executable_inside(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let root = fs::canonicalize(root).map_err(|e| format!("cannot resolve package root: {e}"))?;
    let candidate = root.join(relative);
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!("{relative} is not inside the package"));
    }
    let resolved = fs::canonicalize(&candidate)
        .map_err(|_| format!("{relative} does not exist in the package"))?;
    if !resolved.starts_with(&root) {
        return Err(format!("{relative} resolves outside the package"));
    }
    let meta = fs::metadata(&resolved).map_err(|e| format!("cannot inspect {relative}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{relative} is not a file"));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{relative} is not executable"));
    }
    Ok(candidate)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write as _;

    /// The modification time of every test archive entry.
    pub const TEST_MTIME: u64 = 1_700_000_000;

    pub enum Item<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8], u32),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Fifo(&'a str),
    }

    fn raw_header(kind: EntryType, name: &str, link: &str, mode: u32, size: u64) -> tar::Header {
        let mut header = tar::Header::new_old();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_size(size);
        header.set_mtime(TEST_MTIME);
        {
            let old = header.as_old_mut();
            old.name[..name.len()].copy_from_slice(name.as_bytes());
            old.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        header.set_cksum();
        header
    }

    fn write_tar<W: io::Write>(writer: W, items: &[Item<'_>]) -> W {
        let mut builder = tar::Builder::new(writer);
        for item in items {
            match item {
                Item::Dir(name) => {
                    let h = raw_header(EntryType::Directory, name, "", 0o755, 0);
                    builder.append(&h, io::empty()).unwrap();
                }
                Item::File(name, data, mode) => {
                    let h = raw_header(EntryType::Regular, name, "", *mode, data.len() as u64);
                    builder.append(&h, *data).unwrap();
                }
                Item::Symlink(name, target) => {
                    let h = raw_header(EntryType::Symlink, name, target, 0o777, 0);
                    builder.append(&h, io::empty()).unwrap();
                }
                Item::Hardlink(name, target) => {
                    let h = raw_header(EntryType::Link, name, target, 0o644, 0);
                    builder.append(&h, io::empty()).unwrap();
                }
                Item::Fifo(name) => {
                    let h = raw_header(EntryType::Fifo, name, "", 0o644, 0);
                    builder.append(&h, io::empty()).unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    /// Build a test archive with raw (possibly malicious) names and targets.
    pub fn build(path: &Path, items: &[Item<'_>]) {
        let file = fs::File::create(path).unwrap();
        write_tar(GzEncoder::new(file, Compression::fast()), items)
            .finish()
            .unwrap();
    }

    /// Build a test xz-compressed archive with the same entries as [`build`].
    pub fn build_tar_xz(path: &Path, items: &[Item<'_>]) {
        use xz2::write::XzEncoder;
        let file = fs::File::create(path).unwrap();
        write_tar(XzEncoder::new(file, 6), items).finish().unwrap();
    }

    fn run(items: &[Item<'_>], strip: u32) -> (tempfile::TempDir, Result<ExtractStats, String>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        build(&archive, items);
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let result = extract_tar_gz(&archive, &root, strip, u64::MAX, &mut |_| {}, &|| false);
        (dir, result)
    }

    /// `.tar.xz` artifacts (Clang, Flutter, Zig) extract with exactly the same
    /// path/link/permission rules as `.tar.gz`, including via the dispatcher.
    #[test]
    fn extracts_xz_layout_with_strip_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.xz");
        build_tar_xz(
            &archive,
            &[
                Item::Dir("pkg-1.0/"),
                Item::Dir("pkg-1.0/bin/"),
                Item::File("pkg-1.0/bin/tool-1.0", b"#!/bin/sh\necho 1.0\n", 0o4755),
                Item::Symlink("pkg-1.0/bin/tool", "tool-1.0"),
                Item::Dir("pkg-1.0/lib/"),
                Item::File("pkg-1.0/lib/data.txt", b"data", 0o666),
            ],
        );
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let mut progress_calls = 0u32;
        let stats = extract_archive(
            &archive,
            &root,
            "tar.xz",
            1,
            u64::MAX,
            &mut |_| progress_calls += 1,
            &|| false,
        )
        .unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.links, 1);
        assert!(progress_calls > 0);
        let mode = fs::metadata(root.join("bin/tool-1.0"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid and write bits dropped");
        assert!(executable_inside(&root, "bin/tool").is_ok());
        // File times come from the archive (tools compare them).
        let mtime = |p: &str| {
            fs::metadata(root.join(p))
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(mtime("bin/tool-1.0"), TEST_MTIME);
        assert_eq!(mtime("lib/data.txt"), TEST_MTIME);
        assert_eq!(archive_time(0), None, "no time: keep the extraction time");
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400;
        assert_eq!(archive_time(future), None, "future times are not kept");

        // Escapes are refused exactly like in the gzip path.
        let evil = dir.path().join("evil.tar.xz");
        build_tar_xz(
            &evil,
            &[Item::Dir("pkg/"), Item::Symlink("pkg/l", "../../etc")],
        );
        let evil_root = dir.path().join("evil-root");
        fs::create_dir(&evil_root).unwrap();
        let err = extract_archive(
            &evil,
            &evil_root,
            "tar.xz",
            1,
            u64::MAX,
            &mut |_| {},
            &|| false,
        )
        .unwrap_err();
        assert!(err.contains("outside the package"), "{err}");

        // Unknown formats keep failing closed.
        let err = extract_archive(
            &archive,
            &root,
            "tar.zst",
            1,
            u64::MAX,
            &mut |_| {},
            &|| false,
        )
        .unwrap_err();
        assert!(err.contains("not supported"), "{err}");
    }

    #[test]
    fn refuses_oversized_name_records_before_buffering_them() {
        // Every record type the tar reader buffers whole.
        for kind in [
            EntryType::GNULongName,
            EntryType::GNULongLink,
            EntryType::XHeader,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join("a.tar.gz");
            {
                let file = fs::File::create(&archive).unwrap();
                let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
                let body = vec![b'a'; (MAX_EXTENSION_BYTES as usize) * 4];
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(kind);
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"././@LongLink");
                h.set_cksum();
                builder.append(&h, body.as_slice()).unwrap();
                let h = raw_header(EntryType::Regular, "file", "", 0o644, 1);
                builder.append(&h, &b"x"[..]).unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            let root = dir.path().join("root");
            fs::create_dir(&root).unwrap();
            let err =
                extract_tar_gz(&archive, &root, 0, u64::MAX, &mut |_| {}, &|| false).unwrap_err();
            assert!(err.contains("name/extension record"), "{kind:?}: {err}");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        }
    }

    #[test]
    fn stops_when_the_disk_would_fill() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        build(&archive, &[Item::File("big", &[0u8; 4096], 0o644)]);
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let err = extract_tar_gz(&archive, &root, 0, 1024, &mut |_| {}, &|| false).unwrap_err();
        assert!(err.contains("free disk space"), "{err}");
    }

    #[test]
    fn extracts_runtime_layout_with_strip_and_links() {
        let (dir, result) = run(
            &[
                Item::Dir("pkg-1.0/"),
                Item::Dir("pkg-1.0/bin/"),
                Item::File("pkg-1.0/bin/tool-1.0", b"#!/bin/sh\necho 1.0\n", 0o4755),
                Item::Symlink("pkg-1.0/bin/tool", "tool-1.0"),
                Item::Dir("pkg-1.0/lib/"),
                Item::File("pkg-1.0/lib/data.txt", b"data", 0o666),
                Item::Symlink("pkg-1.0/bin/data", "../lib/data.txt"),
                Item::Hardlink("pkg-1.0/lib/copy.txt", "pkg-1.0/lib/data.txt"),
            ],
            1,
        );
        let stats = result.unwrap();
        let root = dir.path().join("root");
        assert_eq!(stats.links, 2);
        let mode = fs::metadata(root.join("bin/tool-1.0"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid and write bits dropped");
        let mode = fs::metadata(root.join("lib/data.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o644);
        assert!(executable_inside(&root, "bin/tool").is_ok());
        assert!(
            executable_inside(&root, "lib/data.txt").is_err(),
            "not executable"
        );
        assert!(executable_inside(&root, "bin/missing").is_err());
    }

    #[test]
    fn refuses_escapes_and_special_files() {
        let cases: Vec<(&str, Vec<Item<'_>>)> = vec![
            (
                "dotdot path",
                vec![Item::File("pkg/../../evil", b"x", 0o644)],
            ),
            (
                "absolute link",
                vec![Item::Dir("pkg/"), Item::Symlink("pkg/l", "/etc/passwd")],
            ),
            (
                "escaping link",
                vec![Item::Dir("pkg/"), Item::Symlink("pkg/l", "../../etc")],
            ),
            (
                "write through link",
                vec![
                    Item::Dir("pkg/"),
                    Item::Dir("pkg/d/"),
                    Item::Symlink("pkg/l", "d"),
                    Item::File("pkg/l/x", b"x", 0o644),
                ],
            ),
            (
                "physical escape via dot link",
                vec![
                    Item::Dir("pkg/"),
                    Item::Symlink("pkg/s", "."),
                    Item::Symlink("pkg/esc", "s/.."),
                ],
            ),
            ("fifo", vec![Item::Dir("pkg/"), Item::Fifo("pkg/f")]),
            (
                "dangling",
                vec![Item::Dir("pkg/"), Item::Symlink("pkg/l", "missing")],
            ),
            (
                "hardlink outside",
                vec![Item::Dir("pkg/"), Item::Hardlink("pkg/h", "../etc/passwd")],
            ),
        ];
        for (name, items) in cases {
            let (_dir, result) = run(&items, 1);
            assert!(result.is_err(), "{name} accepted");
        }
    }

    #[test]
    fn lexical_resolution() {
        let parent = vec!["bin".to_string()];
        assert_eq!(
            lexical_target(&parent, "../lib/x").unwrap(),
            vec!["lib".to_string(), "x".to_string()]
        );
        assert!(lexical_target(&parent, "../../x").is_none());
        assert!(lexical_target(&parent, "/abs").is_none());
    }

    // ------------------------------------------------------------------ zip

    pub enum ZipItem<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8], u32),
        Symlink(&'a str, &'a str),
        Fifo(&'a str),
    }

    /// Build a test ZIP archive (including entries a safe extractor must
    /// refuse, which a normal writer would never produce).
    pub fn build_zip(path: &Path, items: &[ZipItem<'_>]) {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let file = fs::File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        for item in items {
            match item {
                ZipItem::Dir(name) => {
                    writer
                        .add_directory(*name, SimpleFileOptions::default().unix_permissions(0o755))
                        .unwrap();
                }
                ZipItem::File(name, data, mode) => {
                    writer
                        .start_file(*name, SimpleFileOptions::default().unix_permissions(*mode))
                        .unwrap();
                    writer.write_all(data).unwrap();
                }
                ZipItem::Symlink(name, target) => {
                    writer
                        .add_symlink(*name, *target, SimpleFileOptions::default())
                        .unwrap();
                }
                ZipItem::Fifo(name) => {
                    writer
                        .start_file(
                            *name,
                            SimpleFileOptions::default().external_attributes(0o010644 << 16),
                        )
                        .unwrap();
                }
            }
        }
        writer.finish().unwrap();
    }

    fn run_zip(
        items: &[ZipItem<'_>],
        strip: u32,
    ) -> (tempfile::TempDir, Result<ExtractStats, String>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        build_zip(&archive, items);
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let result = extract_zip(&archive, &root, strip, u64::MAX, &mut |_| {}, &|| false);
        (dir, result)
    }

    #[test]
    fn extracts_zip_layout_with_strip_and_links() {
        let (dir, result) = run_zip(
            &[
                ZipItem::Dir("pkg-1.0/"),
                ZipItem::Dir("pkg-1.0/bin/"),
                ZipItem::File("pkg-1.0/bin/tool-1.0", b"#!/bin/sh\necho 1.0\n", 0o4755),
                ZipItem::Symlink("pkg-1.0/bin/tool", "tool-1.0"),
                ZipItem::Dir("pkg-1.0/lib/"),
                ZipItem::File("pkg-1.0/lib/data.txt", b"data", 0o666),
                ZipItem::Symlink("pkg-1.0/bin/data", "../lib/data.txt"),
            ],
            1,
        );
        let stats = result.unwrap();
        let root = dir.path().join("root");
        assert_eq!(stats.files, 2);
        assert_eq!(stats.links, 2);
        let mode = fs::metadata(root.join("bin/tool-1.0"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid and write bits dropped");
        let mode = fs::metadata(root.join("lib/data.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o644);
        assert!(executable_inside(&root, "bin/tool").is_ok());
        assert!(executable_inside(&root, "lib/data.txt").is_err());
    }

    #[test]
    fn zip_refuses_escapes_and_special_files() {
        let cases: Vec<(&str, Vec<ZipItem<'_>>)> = vec![
            (
                "dotdot path",
                vec![ZipItem::File("pkg/../../evil", b"x", 0o644)],
            ),
            (
                "absolute path",
                vec![ZipItem::File("/etc/passwd", b"x", 0o644)],
            ),
            (
                "absolute link",
                vec![
                    ZipItem::Dir("pkg/"),
                    ZipItem::Symlink("pkg/l", "/etc/passwd"),
                ],
            ),
            (
                "escaping link",
                vec![ZipItem::Dir("pkg/"), ZipItem::Symlink("pkg/l", "../../etc")],
            ),
            (
                "write through link",
                vec![
                    ZipItem::Dir("pkg/"),
                    ZipItem::Dir("pkg/d/"),
                    ZipItem::Symlink("pkg/l", "d"),
                    ZipItem::File("pkg/l/x", b"x", 0o644),
                ],
            ),
            ("fifo", vec![ZipItem::Dir("pkg/"), ZipItem::Fifo("pkg/f")]),
            (
                "dangling",
                vec![ZipItem::Dir("pkg/"), ZipItem::Symlink("pkg/l", "missing")],
            ),
        ];
        for (name, items) in cases {
            let (_dir, result) = run_zip(&items, 1);
            assert!(result.is_err(), "{name} accepted");
        }
    }

    #[test]
    fn zip_stops_when_the_disk_would_fill() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        build_zip(&archive, &[ZipItem::File("big", &[0u8; 4096], 0o644)]);
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let err = extract_zip(&archive, &root, 0, 1024, &mut |_| {}, &|| false).unwrap_err();
        assert!(err.contains("free disk space"), "{err}");
    }

    #[test]
    fn extract_archive_dispatches_by_declared_format() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        build_zip(&archive, &[ZipItem::File("bin/tool", b"x", 0o755)]);
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        extract_archive(&archive, &root, "zip", 0, u64::MAX, &mut |_| {}, &|| false).unwrap();
        assert!(root.join("bin/tool").is_file());
        let err = extract_archive(&archive, &root, "rar", 0, u64::MAX, &mut |_| {}, &|| false)
            .unwrap_err();
        assert!(err.contains("not supported"), "{err}");
    }
}
