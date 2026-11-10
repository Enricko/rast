//! Storage subsystem: logical path resolution, quota accounting, recycle bin,
//! filesystem operations and shared-folder ACLs.
//!
//! The storage layer is the security boundary between untrusted client input
//! (raw uploads, logical paths, shared-folder names) and the host filesystem.
//! Nothing outside the configured storage root is ever touched:
//!
//! ```text
//! <storage_dir>/
//!   homes/<username>/...        logical: /
//!   shared/<container>/...      logical: /shared/<share name>
//!   .trash/<user_id>/<uuid>     recycle bin payloads
//! ```
//!
//! All filesystem access is streamed (upload/download/copy use fixed-size
//! buffers) and quota reservations are persisted in `storage_usage` inside a
//! single atomic SQL statement, so concurrent uploads or copies can never
//! overshoot a user's quota.
//!
//! OWNER: teammate `backend-storage`.

pub mod ops;
pub mod quota;
pub mod safe_path;
pub mod shares;
pub mod trash;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::{
    auth::AuthUser,
    config::Config,
    error::{AppError, AppResult},
};

pub use safe_path::{Access, LogicalPath, Perm, ResolvedPath, RootKind, StorageRoot};

pub use safe_path::{MAX_LOGICAL_PATH_BYTES, MAX_NAME_BYTES, TEMP_PREFIX};

/// Default and maximum page sizes for listings.
pub const DEFAULT_PAGE_SIZE: i64 = 100;
pub const MAX_PAGE_SIZE: i64 = 500;
/// Search is bounded so it can never pin a runtime thread for minutes.
pub const MAX_SEARCH_RESULTS: usize = 500;
pub const MAX_SEARCH_DEPTH: usize = 12;
/// Maximum number of paths accepted by a single copy/move/delete request.
pub const MAX_BATCH_PATHS: usize = 100;
/// Maximum number of tree entries a single copy/move operation will enumerate.
pub const MAX_TREE_ENTRIES: usize = 1_000_000;
/// Maximum sizes accepted by the text preview endpoint.
pub const MAX_TEXT_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_TEXT_PREVIEW_BYTES: u64 = 256 * 1024;

/// Filesystem storage engine.
#[derive(Debug, Clone)]
pub struct Storage {
    /// Canonical storage root (`<storage_dir>`).
    pub storage_root: PathBuf,
    /// Canonical user home root (`<storage_dir>/homes`).
    pub homes_root: PathBuf,
    /// Canonical shared folder root (`<storage_dir>/shared`).
    pub shared_root: PathBuf,
    /// Canonical recycle bin root (`<storage_dir>/.trash`).
    pub trash_root: PathBuf,
    /// Maximum size of a single uploaded file.
    pub max_upload_bytes: u64,
}

impl Storage {
    /// Build the storage engine, creating the directory layout if missing.
    ///
    /// Keeps the signature used by `AppState`; the roots are canonicalised so
    /// containment checks later compare resolved paths, not user input.
    pub fn new(cfg: &Config) -> AppResult<Self> {
        let storage_root = canonical_root(&cfg.storage_dir)?;
        let homes_root = canonical_root(&cfg.homes_dir())?;
        let shared_root = canonical_root(&cfg.shared_dir())?;
        let trash_root = canonical_root(&cfg.trash_dir())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Recycle bin payloads are private to the service account.
            let _ = std::fs::set_permissions(&trash_root, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self {
            storage_root,
            homes_root,
            shared_root,
            trash_root,
            max_upload_bytes: cfg.max_upload_bytes,
        })
    }

    pub fn as_arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Path of a user's home directory. `username` must already be a valid
    /// single path component (validated by the user creation flow).
    pub fn home_dir(&self, username: &str) -> PathBuf {
        self.homes_root.join(username.to_ascii_lowercase())
    }

    /// Path of a shared folder container.
    pub fn shared_container_dir(&self, container_path: &str) -> PathBuf {
        self.shared_root.join(container_path)
    }

    /// Per-user recycle bin directory.
    pub fn user_trash_dir(&self, user_id: i64) -> PathBuf {
        self.trash_root.join(user_id.to_string())
    }

    /// Resolve the namespace root a logical path belongs to, enforcing the
    /// shared-folder ACL (admins bypass it; non-members get a 404 so hidden
    /// shares are indistinguishable from missing ones).
    pub async fn resolve_root(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        logical: &LogicalPath,
    ) -> AppResult<StorageRoot> {
        if let Some((share_name, _rest)) = logical.shared_parts() {
            let share = shares::find_by_name(pool, share_name)
                .await?
                .ok_or(AppError::NotFound("path not found"))?;
            let perm = shares::effective_perm(pool, user, &share.id)
                .await?
                .ok_or(AppError::NotFound("path not found"))?;
            let container = shares::validated_container(&share.container_path)?;
            let physical = self.shared_root.join(container);
            check_root_dir(&physical)?;
            Ok(StorageRoot {
                physical,
                kind: RootKind::Share,
                perm,
                in_home: false,
                share_id: Some(share.id),
                share_name: Some(share.name),
            })
        } else if logical.segments().first().map(String::as_str) == Some("shared") {
            // `/shared` is a namespace mount point, not a listable directory.
            Err(AppError::NotFound("path not found"))
        } else {
            let username = user.username.to_ascii_lowercase();
            safe_path::validate_component(&username)
                .map_err(|_| AppError::internal("invalid user home directory"))?;
            let physical = self.homes_root.join(username);
            check_root_dir(&physical)?;
            Ok(StorageRoot {
                physical,
                kind: RootKind::Home,
                // A user always has full control over their own home.
                perm: Perm::Admin,
                in_home: true,
                share_id: None,
                share_name: None,
            })
        }
    }

    /// Resolve an existing logical path with the requested access level.
    pub async fn resolve(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        path: &str,
        access: Access,
    ) -> AppResult<ResolvedPath> {
        let logical = LogicalPath::parse(path)?;
        self.resolve_logical(pool, user, &logical, access).await
    }

    pub async fn resolve_logical(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        logical: &LogicalPath,
        access: Access,
    ) -> AppResult<ResolvedPath> {
        let root = self.resolve_root(pool, user, logical).await?;
        if access == Access::Write && !root.perm.allows_write() {
            return Err(AppError::Forbidden("write permission required"));
        }
        let physical = safe_path::physical_path(&root.physical, logical.relative_segments())?;
        let exists = safe_path::verify_no_symlinks(&root.physical, &physical)?;
        if !exists {
            return Err(AppError::NotFound("path not found"));
        }
        safe_path::verify_containment(&root.physical, &physical)?;
        Ok(ResolvedPath {
            logical: logical.clone(),
            physical,
            root_physical: root.physical,
            kind: root.kind,
            perm: root.perm,
            in_home: root.in_home,
            share_id: root.share_id,
            share_name: root.share_name,
        })
    }

    /// Build (and validate) a not-yet-existing child of an already resolved
    /// directory. Returns the logical path, physical path and whether an object
    /// currently exists at that location.
    pub fn child_target(
        &self,
        parent: &ResolvedPath,
        name: &str,
    ) -> AppResult<(LogicalPath, PathBuf, bool)> {
        let logical = parent.logical.child(name)?;
        let physical = parent.physical.join(name);
        let exists = safe_path::verify_no_symlinks(&parent.root_physical, &physical)?;
        safe_path::verify_containment(&parent.root_physical, &physical)?;
        Ok((logical, physical, exists))
    }

    /// Ensure the per-user recycle bin directory exists (0700).
    pub fn ensure_trash_dir(&self, user_id: i64) -> AppResult<PathBuf> {
        let dir = self.user_trash_dir(user_id);
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(dir)
    }
}

/// Create (if needed) and canonicalise a storage root directory.
fn canonical_root(path: &Path) -> AppResult<PathBuf> {
    std::fs::create_dir_all(path).map_err(|e| {
        AppError::internal(format!(
            "cannot create storage directory {}: {e}",
            path.display()
        ))
    })?;
    std::fs::canonicalize(path).map_err(|e| {
        AppError::internal(format!(
            "cannot resolve storage directory {}: {e}",
            path.display()
        ))
    })
}

/// Validate a namespace root directory exists, is a real directory and is not
/// itself a symlink.
fn check_root_dir(path: &Path) -> AppResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(AppError::Forbidden("symbolic links are not supported"))
        }
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(AppError::NotFound("path not found")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(AppError::NotFound("path not found"))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};

    pub(crate) fn test_config(dir: &Path) -> Config {
        Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                ..Default::default()
            },
            |_| None,
        )
        .expect("test config")
    }

    #[test]
    fn builds_directory_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let storage = Storage::new(&cfg).unwrap();
        assert!(storage.homes_root.is_dir());
        assert!(storage.shared_root.is_dir());
        assert!(storage.trash_root.is_dir());
        assert!(storage.homes_root.starts_with(&storage.storage_root));
        assert!(storage.home_dir("Alice").ends_with("homes/alice"));
        assert!(storage.user_trash_dir(7).ends_with(".trash/7"));
    }

    #[test]
    fn root_paths_are_inside_the_storage_root() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&test_config(dir.path())).unwrap();
        for root in [
            &storage.homes_root,
            &storage.shared_root,
            &storage.trash_root,
        ] {
            assert!(root.starts_with(&storage.storage_root));
        }
    }
}
