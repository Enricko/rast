//! Logical paths, storage namespaces and hardened physical path helpers.
//!
//! The File Station exposes a small virtual namespace:
//!
//! * `/` is the caller's home directory (`<storage>/homes/<username>`);
//! * `/shared/<name>/...` is a shared folder (`<storage>/shared/<container>`).
//!
//! Every client supplied path is parsed into a [`LogicalPath`] first. Only
//! forward-slash separated normal components are accepted; `..`, absolute
//! filesystem paths, NUL bytes, control characters and backslashes are
//! rejected before any filesystem call. Physical paths are then *rebuilt* from
//! a canonical storage root plus validated components and re-checked with
//! `symlink_metadata` at every component, so a symlink planted inside a home or
//! shared folder can never redirect an operation outside of the storage root.
//!
//! Nothing in this module performs I/O on files: it only inspects metadata
//! (`lstat`) of the roots and path prefixes, which is cheap and safe to call
//! from an async context.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Temporary upload/copy file prefix. Never listed, searchable or addressable.
pub const TEMP_PREFIX: &str = ".rast-tmp-";

/// Maximum length of a single path component in bytes.
pub const MAX_NAME_BYTES: usize = 255;
/// Maximum length of a logical path in bytes.
pub const MAX_LOGICAL_PATH_BYTES: usize = 4096;

/// Permission level on a resolved object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Perm {
    Read,
    Write,
    Admin,
}

impl Perm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Reading is permitted at every level (a caller that can resolve a share
    /// at all can read it).
    pub fn allows_read(self) -> bool {
        true
    }

    pub fn allows_write(self) -> bool {
        self >= Self::Write
    }
}

/// What kind of namespace a path resolves into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Home,
    Share,
}

/// Access level requested from the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// Validate a single path component (a file or directory name that already
/// exists somewhere; accepts dotfiles but not `.`/`..`).
pub fn validate_component(part: &str) -> AppResult<()> {
    if part.is_empty() {
        return Err(AppError::bad_request("empty path component"));
    }
    if part.len() > MAX_NAME_BYTES {
        return Err(AppError::bad_request("path component is too long"));
    }
    if part == "." || part == ".." {
        return Err(AppError::bad_request("path traversal is not allowed"));
    }
    if part.contains('/') || part.contains('\\') {
        return Err(AppError::bad_request("invalid character in path"));
    }
    if part.chars().any(|c| c.is_control()) {
        return Err(AppError::bad_request("invalid character in path"));
    }
    Ok(())
}

/// Validate a name supplied for a *new* filesystem object: like
/// [`validate_component`] but reserved internal prefixes are refused.
pub fn validate_new_name(part: &str) -> AppResult<()> {
    validate_component(part)?;
    if part.starts_with(TEMP_PREFIX) {
        return Err(AppError::bad_request("reserved file name prefix"));
    }
    Ok(())
}

/// A parsed, normalised logical path (`/`, `/a/b`, `/shared/team/x`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalPath {
    segments: Vec<String>,
}

impl std::fmt::Display for LogicalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl LogicalPath {
    /// Parse and normalise a logical path. Rejects traversal, backslashes,
    /// NUL/control characters and non-absolute input.
    pub fn parse(input: &str) -> AppResult<Self> {
        if input.is_empty() {
            return Err(AppError::bad_request("path is required"));
        }
        if input.len() > MAX_LOGICAL_PATH_BYTES {
            return Err(AppError::bad_request("path is too long"));
        }
        if !input.starts_with('/') {
            return Err(AppError::bad_request("path must be absolute"));
        }
        if input.contains('\0') || input.contains('\\') {
            return Err(AppError::bad_request("invalid character in path"));
        }
        let mut segments = Vec::new();
        for raw in input.split('/') {
            if raw.is_empty() {
                continue; // collapse `//`
            }
            if raw == "." || raw == ".." {
                return Err(AppError::bad_request("path traversal is not allowed"));
            }
            validate_component(raw)?;
            segments.push(raw.to_string());
        }
        Ok(Self { segments })
    }

    /// The root path `/`.
    pub fn root() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Canonical string form, always starting with `/`.
    pub fn as_string(&self) -> String {
        if self.segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.segments.join("/"))
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    pub fn parent(&self) -> Option<Self> {
        if self.segments.is_empty() {
            None
        } else {
            Some(Self {
                segments: self.segments[..self.segments.len() - 1].to_vec(),
            })
        }
    }

    /// Append a validated child name.
    pub fn child(&self, name: &str) -> AppResult<Self> {
        validate_new_name(name)?;
        let mut segments = self.segments.clone();
        segments.push(name.to_string());
        Ok(Self { segments })
    }

    /// Append already-validated segments (used when walking directory trees).
    pub fn with_segments(&self, extra: &[String]) -> AppResult<Self> {
        let mut segments = self.segments.clone();
        for part in extra {
            validate_component(part)?;
            segments.push(part.clone());
        }
        Ok(Self { segments })
    }

    /// True when this path is below `/shared/<name>`.
    pub fn is_in_shared(&self) -> bool {
        self.shared_parts().is_some()
    }

    /// Split `/shared/<name>/rest` into `("<name>", ["rest"...])`.
    pub fn shared_parts(&self) -> Option<(&str, &[String])> {
        if self.segments.len() >= 2 && self.segments[0] == "shared" {
            Some((self.segments[1].as_str(), &self.segments[2..]))
        } else {
            None
        }
    }

    /// Segments relative to the storage root this logical path belongs to.
    /// For home paths that is every segment; for shared paths the
    /// `shared/<name>` prefix is removed.
    pub fn relative_segments(&self) -> &[String] {
        if self.shared_parts().is_some() {
            &self.segments[2..]
        } else {
            &self.segments
        }
    }
}

/// A namespace root (`homes/<user>` or `shared/<container>`).
#[derive(Debug, Clone)]
pub struct StorageRoot {
    pub physical: PathBuf,
    pub kind: RootKind,
    pub perm: Perm,
    pub in_home: bool,
    pub share_id: Option<String>,
    pub share_name: Option<String>,
}

/// A resolved logical path guaranteed to live inside its storage root.
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub logical: LogicalPath,
    pub physical: PathBuf,
    /// Canonical physical root the path was resolved against.
    pub root_physical: PathBuf,
    pub kind: RootKind,
    pub perm: Perm,
    pub in_home: bool,
    pub share_id: Option<String>,
    pub share_name: Option<String>,
}

impl ResolvedPath {
    /// Last logical component (empty for a namespace root).
    pub fn name(&self) -> &str {
        self.logical.name().unwrap_or("")
    }

    /// True for `/` and for `/shared/<name>` (namespace roots cannot be
    /// renamed, copied, moved or deleted).
    pub fn is_storage_root(&self) -> bool {
        self.logical.is_root() || (self.kind == RootKind::Share && self.logical.segments().len() == 2)
    }

    pub fn allows_write(&self) -> bool {
        self.perm.allows_write()
    }
}

/// Join validated logical segments onto a physical root.
pub fn physical_path(root: &Path, segments: &[String]) -> AppResult<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in segments {
        validate_component(segment)?;
        out.push(segment);
    }
    Ok(out)
}

/// Walk the path components between `root` (inclusive) and `physical`
/// (inclusive) rejecting symlinks. Returns whether the full path exists.
///
/// This is the primary symlink-escape defence: even if an attacker manages to
/// create a symlink through another channel, no operation will follow it.
pub fn verify_no_symlinks(root: &Path, physical: &Path) -> AppResult<bool> {
    let relative = physical
        .strip_prefix(root)
        .map_err(|_| AppError::Forbidden("path escapes the permitted storage area"))?;
    let mut current = root.to_path_buf();
    // Check the root itself too: a symlinked root would poison every child.
    match std::fs::symlink_metadata(&current) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(AppError::Forbidden("symbolic links are not supported"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(AppError::bad_request("invalid path component"));
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(AppError::Forbidden("symbolic links are not supported"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

/// Canonicalise the nearest existing ancestor of `physical` and verify it
/// still lives below the (already canonical) storage `root`. This catches
/// exotic escapes (bind mounts, mount points) that `lstat` alone cannot.
pub fn verify_containment(root: &Path, physical: &Path) -> AppResult<()> {
    if !physical.starts_with(root) {
        return Err(AppError::Forbidden("path escapes the permitted storage area"));
    }
    let mut probe = physical.to_path_buf();
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(real) => {
                if !real.starts_with(root) {
                    return Err(AppError::Forbidden("path escapes the permitted storage area"));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match probe.parent() {
                Some(parent) if parent.starts_with(root) => probe = parent.to_path_buf(),
                _ => return Err(AppError::Forbidden("path escapes the permitted storage area")),
            },
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalises_paths() {
        let root = LogicalPath::parse("/").unwrap();
        assert!(root.is_root());
        assert_eq!(root.as_string(), "/");
        let nested = LogicalPath::parse("//a/b/").unwrap();
        assert_eq!(nested.as_string(), "/a/b");
        assert_eq!(nested.name(), Some("b"));
        assert_eq!(nested.parent().unwrap().as_string(), "/a");
        assert_eq!(nested.child("c").unwrap().as_string(), "/a/b/c");
        let shared = LogicalPath::parse("/shared/team/docs").unwrap();
        assert!(shared.is_in_shared());
        assert_eq!(shared.shared_parts().unwrap().0, "team");
        assert_eq!(shared.relative_segments(), ["docs"]);
        assert_eq!(root.relative_segments().len(), 0);
    }

    #[test]
    fn rejects_traversal_and_invalid_paths() {
        for bad in [
            "",
            "relative/path",
            "/../etc/passwd",
            "/a/../../etc",
            "/a/..",
            "a/b",
            "/a\\b",
            "/a\0b",
            "/a\nb",
            "/a/./b",
        ] {
            assert!(
                LogicalPath::parse(bad).is_err(),
                "path {bad:?} should have been rejected"
            );
        }
        let long = format!("/{}", "x".repeat(MAX_NAME_BYTES + 1));
        assert!(LogicalPath::parse(&long).is_err());
        let long_path = format!("/{}", vec!["abc"; 2000].join("/"));
        assert!(LogicalPath::parse(&long_path).is_err());
    }

    #[test]
    fn rejects_reserved_new_names() {
        assert!(validate_new_name("normal.txt").is_ok());
        assert!(validate_new_name(".hidden").is_ok());
        assert!(validate_new_name(".rast-tmp-abc").is_err());
        assert!(validate_new_name("..").is_err());
        assert!(validate_new_name("a/b").is_err());
        assert!(validate_new_name("").is_err());
        assert!(validate_new_name("a\\b").is_err());
        assert!(validate_new_name("a\u{7}b").is_err());
    }

    #[test]
    fn perm_ordering() {
        assert!(Perm::Admin.allows_write());
        assert!(Perm::Write.allows_write());
        assert!(!Perm::Read.allows_write());
        assert_eq!(Perm::parse("read"), Some(Perm::Read));
        assert_eq!(Perm::parse("write"), Some(Perm::Write));
        assert_eq!(Perm::parse("admin"), Some(Perm::Admin));
        assert_eq!(Perm::parse("owner"), None);
    }

    #[test]
    fn containment_is_component_wise_not_prefix_wise() {
        assert!(Path::new("/tmp/store/home").starts_with("/tmp/store"));
        // Prefix collision: a sibling whose name starts with the same bytes
        // must not be considered inside the root.
        assert!(!Path::new("/tmp/store-evil/home").starts_with("/tmp/store"));
        assert!(!Path::new("/tmp/store").starts_with("/tmp/store-evil"));
    }

    #[test]
    fn symlink_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let inside = root.join("inside");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("ok.txt"), b"ok").unwrap();

        // Ordinary paths pass.
        assert!(verify_no_symlinks(&root, &inside.join("ok.txt")).unwrap());

        // A symlink to an existing directory outside the root is rejected.
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let err = verify_no_symlinks(&root, &root.join("escape/secret.txt")).unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);

        // A symlinked file is rejected as final component too.
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("leak.txt")).unwrap();
        assert!(verify_no_symlinks(&root, &root.join("leak.txt")).is_err());

        // A nonexistent tail simply reports "does not exist".
        assert!(!verify_no_symlinks(&root, &root.join("inside/missing.txt")).unwrap());

        // Containment check passes for real children and fails for others.
        assert!(verify_containment(&root, &inside).is_ok());
        assert!(verify_containment(&root, &outside).is_err());
    }

    #[test]
    fn physical_path_rebuilds_safely() {
        let root = Path::new("/srv/rast/homes/alice");
        let path = physical_path(root, &["docs".to_string(), "a.txt".to_string()]).unwrap();
        assert_eq!(path, Path::new("/srv/rast/homes/alice/docs/a.txt"));
        assert!(physical_path(root, &["..".to_string()]).is_err());
        assert!(physical_path(root, &["/etc".to_string()]).is_err());
    }
}
