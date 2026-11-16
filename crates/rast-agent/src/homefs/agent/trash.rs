//! The recycle bin of a one-HOME account: the freedesktop.org Trash
//! specification (ADR 0005, section 5), shared with every Linux app of the
//! user.
//!
//! * HOME: `~/.local/share/Trash/{files,info}`;
//! * a shared folder: `<container>/.Trash-<uid>/{files,info}` (the
//!   specification's top directory layout), so a deletion stays inside the
//!   share and only the person who deleted it sees it.
//!
//! A trash belongs to the user: every app and terminal of the user can write
//! into it, so the `.trashinfo` files (and `directorysizes`) are untrusted
//! input. An entry is restored only to a plain path below HOME or below the
//! container of a shared folder that the server named for this request
//! (`xdg_trash::home_origin`, `xdg_trash::share_origin`); anything else goes
//! back to the top of the root that holds the trash. Every step goes through
//! the descriptor layer (`fsat`): no link is followed, folders that are
//! made are reopened and checked, and names are moved with
//! `RENAME_NOREPLACE`.

use std::collections::{BTreeMap, HashSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use rustix::fs::{self as rfs, Mode, OFlags};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{exists_as, page_params, HomeAgent, HomeSpace, Node, NsRoot};
use crate::error::{AppError, AppResult};
use crate::fileagent::{blocking, check, CancelOnDrop};
use crate::homefs::fsat::{self, fs_error, Info, Kind, Resolve};
use crate::homefs::{ShareRoots, SHARED_DIR};
use crate::storage::trash::TrashEntryDto;
use crate::storage::xdg_trash::{
    self, entry_name, format_directory_sizes, format_info, home_origin, info_name,
    is_trash_dir_name, parse_directory_sizes, parse_info, share_origin, share_trash_name, DirSize,
    HomeOrigin, TrashId, DIRECTORY_SIZES, HOME_TRASH, INFO_SUFFIX, MAX_DIRECTORY_SIZES_BYTES,
    MAX_INFO_BYTES,
};
use crate::storage::{LogicalPath, RootKind, MAX_BATCH_PATHS, MAX_NAME_BYTES, TEMP_PREFIX};

/// Info files read from one trash at most.
pub const MAX_TRASH_ENTRIES: usize = 20_000;
/// Expired entries removed per listing.
const EXPIRED_BATCH: usize = 50;
/// Names tried for a new entry (`name`, `name.2`, …).
const MAX_NAME_ATTEMPTS: u32 = 1000;
/// Original paths an empty reports at most (for realtime events).
const MAX_REPORTED_PATHS: usize = 500;

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Which trash.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Home,
    /// The trash of the shared folder the server named `path_name`.
    Share {
        share_id: String,
        path_name: String,
        container: String,
    },
}

/// An open trash: its folder, `files/` and `info/` (readable descriptors),
/// and the namespace root that holds it.
struct Trash {
    place: Place,
    root: NsRoot,
    top: OwnedFd,
    files: OwnedFd,
    info: OwnedFd,
    /// The file system of `files/`: removals never leave it.
    dev: u64,
}

/// A readable descriptor of a folder held as a handle.
fn readable(handle: BorrowedFd<'_>) -> AppResult<OwnedFd> {
    rfs::openat(
        handle,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| fs_error(error.into()))
}

/// HOME, opened for reading (the usage scan).
pub(super) fn readable_root(space: &HomeSpace) -> AppResult<OwnedFd> {
    readable(space.home_root.as_fd())
}

/// A trash folder must belong to this user (the specification's check).
fn owned(fd: &OwnedFd) -> AppResult<Info> {
    let info = fsat::stat_fd(fd).map_err(fs_error)?;
    if !info.is_dir() || info.uid != euid() {
        return Err(AppError::Forbidden(
            "the recycle bin folder belongs to someone else",
        ));
    }
    Ok(info)
}

/// `files/` or `info/` below the trash folder `top`.
fn part(top: BorrowedFd<'_>, name: &str, create: bool) -> AppResult<Option<OwnedFd>> {
    let segments = [name.to_string()];
    let handle = if create {
        fsat::ensure_dirs(top, &segments, Resolve::SameFs, 0o700)?
    } else {
        match fsat::open_dir_resolved(top, &segments, Resolve::SameFs) {
            Ok((fd, _)) => fd,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => return Err(fs_error(error)),
        }
    };
    let fd = readable(handle.as_fd())?;
    owned(&fd)?;
    Ok(Some(fd))
}

impl HomeSpace {
    fn home_ns(&self) -> NsRoot {
        NsRoot {
            fd: self.home_root.clone(),
            info: self.home_info,
            kind: RootKind::Home,
            share_name: None,
        }
    }

    /// The trash of `place`; with `create`, made where it is missing.
    /// `Ok(None)`: it does not exist (and `create` is off).
    fn trash(&self, place: &Place, shares: &ShareRoots, create: bool) -> AppResult<Option<Trash>> {
        let (root, top) = match place {
            Place::Home => {
                let root = self.home_ns();
                let segments: Vec<String> = HOME_TRASH.iter().map(|s| s.to_string()).collect();
                let handle = if create {
                    fsat::ensure_dirs(root.fd.as_fd(), &segments, Resolve::Beneath, 0o700)?
                } else {
                    match fsat::open_dir_resolved(root.fd.as_fd(), &segments, Resolve::Beneath) {
                        Ok((fd, _)) => fd,
                        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                            return Ok(None)
                        }
                        Err(error) => return Err(fs_error(error)),
                    }
                };
                let top = readable(handle.as_fd())?;
                owned(&top)?;
                (root, top)
            }
            Place::Share { path_name, .. } => {
                let root = self.share_root(path_name, shares)?;
                let name = share_trash_name(euid());
                let top = match fsat::stat_at(root.fd.as_fd(), &name).map_err(fs_error)? {
                    Some(info) if info.is_dir() => {
                        let handle = fsat::open_dir_at(root.fd.as_fd(), &name, &info)?;
                        readable(handle.as_fd())?
                    }
                    Some(_) => {
                        return Err(AppError::Forbidden(
                            "the recycle bin folder of this shared folder is not a folder",
                        ))
                    }
                    None if !create => return Ok(None),
                    None => {
                        match fsat::mkdir_mode_at(root.fd.as_fd(), &name, 0o700) {
                            Ok(()) => {}
                            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                            Err(error) => return Err(fs_error(error)),
                        }
                        fsat::open_created_dir(root.fd.as_fd(), name.as_str())?
                    }
                };
                let info = owned(&top)?;
                if create {
                    // Closed to the other members: no inherited ACL, 0700
                    // (set-gid kept, so new entries keep the share's group).
                    fsat::remove_acls(top.as_fd()).map_err(fs_error)?;
                    let mode = 0o700 | (info.mode & 0o2000);
                    if info.mode & 0o7777 != mode {
                        fsat::set_dir_mode(top.as_fd(), mode).map_err(fs_error)?;
                    }
                }
                (root, top)
            }
        };
        let Some(files) = part(top.as_fd(), "files", create)? else {
            return Ok(None);
        };
        let Some(info) = part(top.as_fd(), "info", create)? else {
            return Ok(None);
        };
        if create && matches!(place, Place::Share { .. }) {
            fsat::remove_acls(files.as_fd()).map_err(fs_error)?;
            fsat::remove_acls(info.as_fd()).map_err(fs_error)?;
        }
        let dev = fsat::stat_fd(&files).map_err(fs_error)?.dev;
        Ok(Some(Trash {
            place: place.clone(),
            root,
            top,
            files,
            info,
            dev,
        }))
    }

    /// The home trash and the trashes of every shared folder the request
    /// names that exist.
    fn all_trashes(&self, shares: &ShareRoots) -> AppResult<Vec<Trash>> {
        let mut out = Vec::new();
        for place in places(shares) {
            match self.trash(&place, shares, false) {
                Ok(Some(trash)) => out.push(trash),
                Ok(None) => {}
                // A share that is not set up (or not reachable) has no trash
                // to show; HOME's must work.
                Err(error) if place != Place::Home => {
                    tracing::debug!(error = %error, "a shared folder's recycle bin is not readable");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(out)
    }

    /// The place of a trash id, from the shares the server named.
    fn place_of(&self, id: &TrashId, shares: &ShareRoots) -> AppResult<Place> {
        match id.share_id() {
            None => Ok(Place::Home),
            Some(share_id) => shares
                .iter()
                .find(|(_, root)| root.id == share_id)
                .map(|(path_name, root)| Place::Share {
                    share_id: root.id.clone(),
                    path_name: path_name.clone(),
                    container: root.container.clone(),
                })
                .ok_or(AppError::NotFound("recycle bin entry not found")),
        }
    }
}

/// HOME first, then every shared folder of the request.
fn places(shares: &ShareRoots) -> Vec<Place> {
    let mut out = vec![Place::Home];
    out.extend(shares.iter().map(|(path_name, root)| Place::Share {
        share_id: root.id.clone(),
        path_name: path_name.clone(),
        container: root.container.clone(),
    }));
    out
}

/// Where an entry goes back to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Home(Vec<String>),
    Share {
        path_name: String,
        segments: Vec<String>,
    },
}

impl Target {
    fn logical(&self) -> AppResult<LogicalPath> {
        let text = match self {
            Self::Home(segments) => format!("/{}", segments.join("/")),
            Self::Share {
                path_name,
                segments,
            } => format!("/shared/{path_name}/{}", segments.join("/")),
        };
        let logical = LogicalPath::parse(&text)?;
        // A HOME path that the namespace would read as a shared folder or
        // the root file system is not a HOME path.
        if let Self::Home(segments) = self {
            let first = segments.first().map(String::as_str);
            if matches!(
                first,
                Some("shared") | Some(crate::storage::safe_path::RESERVED_NAME)
            ) {
                return Err(AppError::bad_request("not a HOME path"));
            }
        }
        Ok(logical)
    }
}

/// The top of the root that holds the trash, under the entry's own name:
/// where an entry goes whose recorded origin is not usable.
fn fallback(place: &Place, name: &str) -> Target {
    let safe = if matches!(name, "shared") || name == crate::storage::safe_path::RESERVED_NAME {
        format!("{name} (restored)")
    } else {
        name.to_string()
    };
    match place {
        Place::Home => Target::Home(vec![safe]),
        Place::Share { path_name, .. } => Target::Share {
            path_name: path_name.clone(),
            segments: vec![safe],
        },
    }
}

/// Where the entry `name` of `place`, whose info file says `path`, goes
/// back to. The recorded path is untrusted; only plain paths below HOME or
/// below a container the server named are used.
fn target_of(
    space: &HomeSpace,
    place: &Place,
    name: &str,
    path: &str,
    shares: &ShareRoots,
) -> Target {
    let home_trash: Vec<String> = HOME_TRASH.iter().map(|s| s.to_string()).collect();
    let home = space.home.to_string_lossy();
    let shared_root = space.shared_root.to_string_lossy();
    let found = match place {
        Place::Home => match home_origin(path, &home, &shared_root) {
            Some(HomeOrigin::Home(segments)) => {
                // Deleted through a `~/Shared/<name>` link.
                if segments.len() > 2
                    && segments[0] == SHARED_DIR
                    && shares.get(&segments[1]).is_some()
                {
                    Some(Target::Share {
                        path_name: segments[1].clone(),
                        segments: segments[2..].to_vec(),
                    })
                } else if segments.starts_with(&home_trash) {
                    None
                } else {
                    Some(Target::Home(segments))
                }
            }
            Some(HomeOrigin::Share {
                container,
                segments,
            }) => shares
                .iter()
                .find(|(_, root)| root.container == container)
                .map(|(path_name, _)| Target::Share {
                    path_name: path_name.clone(),
                    segments,
                }),
            None => None,
        },
        Place::Share {
            path_name,
            container,
            ..
        } => {
            let absolute = space.shared_root.join(container);
            share_origin(path, &absolute.to_string_lossy())
                .filter(|segments| !is_trash_dir_name(&segments[0]))
                .map(|segments| Target::Share {
                    path_name: path_name.clone(),
                    segments,
                })
        }
    };
    found
        .filter(|target| target.logical().is_ok())
        .unwrap_or_else(|| fallback(place, name))
}

/// One entry of a trash.
struct Item {
    id: TrashId,
    name: String,
    target: Target,
    original_path: String,
    deleted_ms: i64,
    /// The entry in `files/`.
    file: Info,
    size: u64,
}

impl Item {
    fn dto(&self) -> TrashEntryDto {
        TrashEntryDto {
            id: self.id.encode(),
            original_path: self.original_path.clone(),
            size_bytes: i64::try_from(self.size).unwrap_or(i64::MAX),
            kind: if self.file.is_dir() { "dir" } else { "file" }.to_string(),
            deleted_at: self.deleted_ms,
        }
    }
}

impl Trash {
    fn id(&self, name: &str) -> TrashId {
        match &self.place {
            Place::Home => TrashId::Home(name.to_string()),
            Place::Share { share_id, .. } => TrashId::Share {
                share_id: share_id.clone(),
                name: name.to_string(),
            },
        }
    }

    fn sizes(&self) -> BTreeMap<String, DirSize> {
        fsat::read_small(self.top.as_fd(), DIRECTORY_SIZES, MAX_DIRECTORY_SIZES_BYTES)
            .ok()
            .flatten()
            .map(|bytes| parse_directory_sizes(&bytes))
            .unwrap_or_default()
    }

    fn save_sizes(&self, sizes: &BTreeMap<String, DirSize>) {
        let result = if sizes.is_empty() {
            match fsat::unlink_at(self.top.as_fd(), DIRECTORY_SIZES) {
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
                other => other,
            }
        } else {
            fsat::write_replace(
                self.top.as_fd(),
                DIRECTORY_SIZES,
                format_directory_sizes(sizes).as_bytes(),
            )
        };
        if let Err(error) = result {
            tracing::debug!(error = %error, "cannot update the recycle bin's directorysizes");
        }
    }

    /// The entry `name`, if its info file and its payload are there.
    fn item(
        &self,
        space: &HomeSpace,
        name: &str,
        shares: &ShareRoots,
        sizes: &mut BTreeMap<String, DirSize>,
        dirty: &mut bool,
    ) -> AppResult<Option<Item>> {
        let info_file = info_name(name);
        let Some(meta) = fsat::stat_at(self.info.as_fd(), &info_file).map_err(fs_error)? else {
            return Ok(None);
        };
        if meta.kind != Kind::File {
            return Ok(None);
        }
        let Some(bytes) =
            fsat::read_small(self.info.as_fd(), &info_file, MAX_INFO_BYTES).map_err(fs_error)?
        else {
            return Ok(None);
        };
        let Some(parsed) = parse_info(&bytes) else {
            return Ok(None);
        };
        let Some(file) = fsat::stat_at(self.files.as_fd(), name).map_err(fs_error)? else {
            return Ok(None);
        };
        let info_mtime = meta.modified_ms.div_euclid(1000);
        let size = match file.kind {
            Kind::File => file.len,
            Kind::Dir => match sizes.get(name) {
                Some(cached) if cached.mtime == info_mtime => cached.bytes,
                _ => {
                    let bytes =
                        fsat::tree_bytes(self.files.as_fd(), name, &file, "", Some(self.dev));
                    sizes.insert(
                        name.to_string(),
                        DirSize {
                            bytes,
                            mtime: info_mtime,
                        },
                    );
                    *dirty = true;
                    bytes
                }
            },
            _ => 0,
        };
        let target = target_of(space, &self.place, name, &parsed.path, shares);
        let original_path = target.logical()?.as_string();
        Ok(Some(Item {
            id: self.id(name),
            name: name.to_string(),
            target,
            original_path,
            deleted_ms: parsed.deleted_ms.unwrap_or(meta.modified_ms),
            file,
            size,
        }))
    }

    /// Every entry (at most [`MAX_TRASH_ENTRIES`] info files are read).
    fn items(&self, space: &HomeSpace, shares: &ShareRoots) -> AppResult<Vec<Item>> {
        let mut sizes = self.sizes();
        let mut dirty = false;
        let mut out = Vec::new();
        let (names, truncated) =
            fsat::names_limited(self.info.as_fd(), MAX_TRASH_ENTRIES).map_err(fs_error)?;
        if truncated {
            tracing::warn!(
                limit = MAX_TRASH_ENTRIES,
                "a recycle bin holds more entries than are listed"
            );
        }
        let mut seen = HashSet::new();
        for info_file in names {
            let Some(name) = entry_name(&info_file) else {
                continue;
            };
            if !seen.insert(name.to_string()) {
                continue;
            }
            if let Some(item) = self.item(space, name, shares, &mut sizes, &mut dirty)? {
                out.push(item);
            }
        }
        // Drop cached sizes of entries that are gone.
        let before = sizes.len();
        sizes.retain(|name, _| seen.contains(name));
        if dirty || sizes.len() != before {
            self.save_sizes(&sizes);
        }
        Ok(out)
    }

    /// Remove the entry `name` for good: its payload (never leaving the
    /// trash's file system), then its info file.
    fn remove(&self, name: &str) -> AppResult<()> {
        fsat::remove_at(self.files.as_fd(), name, Some(self.dev))?;
        match fsat::unlink_at(self.info.as_fd(), &info_name(name)) {
            Err(error) if error.raw_os_error() != Some(libc::ENOENT) => Err(fs_error(error)),
            _ => Ok(()),
        }
    }

    fn forget_sizes(&self, names: &[String]) {
        let mut sizes = self.sizes();
        let before = sizes.len();
        for name in names {
            sizes.remove(name);
        }
        if sizes.len() != before {
            self.save_sizes(&sizes);
        }
    }
}

/// `name` with the attempt number: `name`, `name.2`, … (shortened so that
/// the info file name still fits).
fn candidate(name: &str, attempt: u32) -> String {
    let limit = MAX_NAME_BYTES - INFO_SUFFIX.len();
    let suffix = if attempt <= 1 {
        String::new()
    } else {
        format!(".{attempt}")
    };
    let mut base = name.to_string();
    while base.len() + suffix.len() > limit {
        base.pop();
    }
    format!("{base}{suffix}")
}

/// A new, unused name in `trash` with its info file created (empty, 0600),
/// so no other process (app or agent) can take the same name.
fn reserve_name(trash: &Trash, name: &str) -> AppResult<(String, std::fs::File)> {
    for attempt in 1..=MAX_NAME_ATTEMPTS {
        let candidate = candidate(name, attempt);
        if crate::storage::safe_path::validate_component(&candidate).is_err() {
            break;
        }
        if fsat::stat_at(trash.files.as_fd(), &candidate)
            .map_err(fs_error)?
            .is_some()
        {
            continue;
        }
        match fsat::create_at(trash.info.as_fd(), &info_name(&candidate), 0o600) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(error) => return Err(fs_error(error)),
        }
    }
    Err(AppError::conflict(
        "the recycle bin already holds too many entries with this name",
    ))
}

/// An entry moved into the recycle bin.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Trashed {
    id: String,
    original_path: String,
    size_bytes: i64,
    kind: &'static str,
}

/// Move one existing entry into the trash of its root.
fn move_to_trash(
    space: &HomeSpace,
    node: &Node,
    shares: &ShareRoots,
    cancel: &AtomicBool,
) -> AppResult<Trashed> {
    let Some((dir, name)) = &node.at else {
        return Err(AppError::bad_request("storage roots cannot be deleted"));
    };
    let relative = node.logical.relative_segments();
    let place = match node.root.kind {
        RootKind::Home => {
            let home_trash: Vec<String> = HOME_TRASH.iter().map(|s| s.to_string()).collect();
            if relative.starts_with(&home_trash) {
                return Err(AppError::bad_request(
                    "this item is already in the recycle bin; remove it from there",
                ));
            }
            if home_trash.starts_with(relative) {
                return Err(AppError::bad_request("this folder holds the recycle bin"));
            }
            Place::Home
        }
        RootKind::Share => {
            let (path_name, _) = node
                .logical
                .shared_parts()
                .ok_or_else(|| AppError::internal("a shared folder path without a name"))?;
            let root = shares
                .get(path_name)
                .ok_or(AppError::NotFound("path not found"))?;
            Place::Share {
                share_id: root.id.clone(),
                path_name: path_name.to_string(),
                container: root.container.clone(),
            }
        }
    };
    let trash = space
        .trash(&place, shares, true)?
        .ok_or_else(|| AppError::internal("the recycle bin could not be made"))?;
    let size = fsat::tree_bytes(
        dir.as_fd(),
        name,
        &node.info,
        TEMP_PREFIX,
        node.root.tree_fs(),
    );
    let (entry, mut info_file) = reserve_name(&trash, name)?;
    let undo_info = || {
        let _ = fsat::unlink_at(trash.info.as_fd(), &info_name(&entry));
    };
    let recorded = match place {
        Place::Home => format!("{}/{}", space.home.display(), relative.join("/")),
        Place::Share { .. } => relative.join("/"),
    };
    let now = crate::db::now_ms();
    {
        use std::io::Write;
        if let Err(error) = info_file
            .write_all(format_info(&recorded, now).as_bytes())
            .and_then(|_| info_file.sync_data())
        {
            undo_info();
            return Err(fs_error(error));
        }
    }
    match fsat::rename_noreplace(dir.as_fd(), name, trash.files.as_fd(), &entry) {
        Ok(()) => {}
        // HOME may hold the user's own mounts: copy, then remove.
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            let mut copied = fsat::Copied::default();
            let mut created = None;
            let result = fsat::copy_at(
                dir.as_fd(),
                name,
                &node.info,
                node.root.tree_fs(),
                trash.files.as_fd(),
                false,
                cancel,
                TEMP_PREFIX,
                &mut copied,
                &mut created,
            );
            if let Err(error) = result {
                if let Some(made) = created {
                    fsat::remove_created(trash.files.as_fd(), &entry, &made, Some(trash.dev));
                }
                undo_info();
                return Err(error);
            }
            if matches!(fsat::stat_at(dir.as_fd(), name), Ok(Some(info)) if info.same_inode(&node.info))
            {
                fsat::remove_at(dir.as_fd(), name, node.root.tree_fs())?;
            }
        }
        Err(error) => {
            undo_info();
            return Err(match error.raw_os_error() {
                Some(libc::EINVAL) => AppError::bad_request("a folder cannot be moved into itself"),
                _ => exists_as(format!("\"{entry}\" already exists in the recycle bin"))(error),
            });
        }
    }
    if node.info.is_dir() {
        let mtime = fsat::stat_at(trash.info.as_fd(), &info_name(&entry))
            .ok()
            .flatten()
            .map_or(now / 1000, |info| info.modified_ms.div_euclid(1000));
        let mut sizes = trash.sizes();
        sizes.insert(entry.clone(), DirSize { bytes: size, mtime });
        trash.save_sizes(&sizes);
    }
    Ok(Trashed {
        id: trash.id(&entry).encode(),
        original_path: node.logical.as_string(),
        size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
        kind: if node.info.is_dir() { "dir" } else { "file" },
    })
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct DeleteRequest {
    paths: Vec<String>,
}

/// POST /api/files/delete: move entries into the recycle bin (the server
/// has checked write access and runs this as a job).
pub(super) async fn delete(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<DeleteRequest>,
) -> AppResult<Json<Value>> {
    if body.paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    if body.paths.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many paths in one request"));
    }
    let shares = ShareRoots::from_headers(&headers)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let space = agent.space.clone();
    blocking(move || {
        let mut seen = HashSet::new();
        let mut nodes = Vec::with_capacity(body.paths.len());
        for raw in &body.paths {
            let node = space.node_at(raw, &shares)?;
            if node.is_storage_root() {
                return Err(AppError::bad_request("storage roots cannot be deleted"));
            }
            if !seen.insert(node.logical.as_string()) {
                return Err(AppError::bad_request("duplicate paths in request"));
            }
            nodes.push(node);
        }
        let mut items = Vec::with_capacity(nodes.len());
        let mut bytes = 0i64;
        for node in &nodes {
            check(&cancel)?;
            let trashed = move_to_trash(&space, node, &shares, &cancel)?;
            bytes = bytes.saturating_add(trashed.size_bytes);
            items.push(trashed);
        }
        Ok(Json(json!({
            "deleted": items.len(),
            "bytes": bytes,
            "trashed": true,
            "items": items,
        })))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub(super) struct ListQuery {
    page: Option<i64>,
    #[serde(rename = "perPage", alias = "per_page")]
    per_page: Option<i64>,
    /// Entries deleted more than this many days ago are removed first.
    #[serde(rename = "retentionDays")]
    retention_days: Option<i64>,
}

/// Remove up to [`EXPIRED_BATCH`] entries older than `cutoff`, oldest first.
fn purge(trashes: &[Trash], items: &mut Vec<(usize, Item)>, cutoff: i64) -> usize {
    items.sort_by_key(|(_, item)| item.deleted_ms);
    let mut purged: Vec<(usize, String)> = Vec::new();
    for (index, item) in items.iter() {
        if purged.len() >= EXPIRED_BATCH || item.deleted_ms >= cutoff {
            break;
        }
        match trashes[*index].remove(&item.name) {
            Ok(()) => purged.push((*index, item.name.clone())),
            Err(error) => {
                tracing::debug!(error = %error, "cannot remove an expired recycle bin entry")
            }
        }
    }
    for (index, trash) in trashes.iter().enumerate() {
        let names: Vec<String> = purged
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, name)| name.clone())
            .collect();
        if !names.is_empty() {
            trash.forget_sizes(&names);
        }
    }
    items.retain(|(index, item)| {
        !purged
            .iter()
            .any(|(i, name)| i == index && *name == item.name)
    });
    purged.len()
}

/// The open trashes of a request and their entries (with the index of the
/// trash each is in).
type Gathered = (Vec<Trash>, Vec<(usize, Item)>);

/// Every entry of every trash of the request, with the trash it is in.
fn gather(space: &HomeSpace, shares: &ShareRoots) -> AppResult<Gathered> {
    let trashes = space.all_trashes(shares)?;
    let mut items = Vec::new();
    for (index, trash) in trashes.iter().enumerate() {
        for item in trash.items(space, shares)? {
            items.push((index, item));
        }
    }
    Ok((trashes, items))
}

/// GET /api/files/trash
pub(super) async fn list(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let space = agent.space.clone();
    blocking(move || {
        let (trashes, mut items) = gather(&space, &shares)?;
        let purged = match query.retention_days {
            Some(days) => {
                let days = days.clamp(1, 3650);
                purge(
                    &trashes,
                    &mut items,
                    crate::db::now_ms() - days * 86_400_000,
                )
            }
            None => 0,
        };
        items.sort_by(|(_, a), (_, b)| {
            b.deleted_ms
                .cmp(&a.deleted_ms)
                .then_with(|| a.original_path.cmp(&b.original_path))
        });
        let bytes: i64 = items
            .iter()
            .map(|(_, item)| i64::try_from(item.size).unwrap_or(i64::MAX))
            .fold(0i64, i64::saturating_add);
        let dtos: Vec<TrashEntryDto> = items.iter().map(|(_, item)| item.dto()).collect();
        let (page, per_page) = page_params(query.page, query.per_page);
        let per_page = per_page.min(500);
        let (items, page, per_page, total) = crate::storage::ops::paginate(dtos, page, per_page);
        Ok(Json(json!({
            "items": items,
            "page": page,
            "perPage": per_page,
            "total": total,
            "bytes": bytes,
            "purged": purged,
        })))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub(super) struct IdsRequest {
    ids: Vec<String>,
}

/// The entries `ids` names, each with its open trash (checked before
/// anything changes: a missing entry fails the whole request).
fn resolve_ids(
    space: &HomeSpace,
    ids: &[String],
    shares: &ShareRoots,
) -> AppResult<Vec<(Arc<Trash>, Item)>> {
    if ids.is_empty() {
        return Err(AppError::bad_request("at least one trash id is required"));
    }
    if ids.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many ids in one request"));
    }
    let mut open: Vec<Arc<Trash>> = Vec::new();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in ids {
        if !seen.insert(raw.clone()) {
            continue;
        }
        let id = TrashId::parse(raw).ok_or(AppError::NotFound("recycle bin entry not found"))?;
        let place = space.place_of(&id, shares)?;
        let trash = match open.iter().find(|trash| trash.place == place) {
            Some(trash) => trash.clone(),
            None => {
                let trash = Arc::new(
                    space
                        .trash(&place, shares, false)?
                        .ok_or(AppError::NotFound("recycle bin entry not found"))?,
                );
                open.push(trash.clone());
                trash
            }
        };
        let mut sizes = trash.sizes();
        let item = trash
            .item(space, id.name(), shares, &mut sizes, &mut false)?
            .ok_or(AppError::NotFound("recycle bin entry not found"))?;
        out.push((trash, item));
    }
    Ok(out)
}

/// The folder a target goes into (made where missing) and its root.
fn target_parent(
    space: &HomeSpace,
    target: &Target,
    shares: &ShareRoots,
    create: bool,
) -> AppResult<(NsRoot, Option<OwnedFd>, String)> {
    let (root, segments) = match target {
        Target::Home(segments) => (space.home_ns(), segments.as_slice()),
        Target::Share {
            path_name,
            segments,
        } => (space.share_root(path_name, shares)?, segments.as_slice()),
    };
    let (name, parents) = segments
        .split_last()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let parent = if create {
        Some(fsat::ensure_dirs(
            root.fd.as_fd(),
            parents,
            root.resolve(),
            0o777,
        )?)
    } else {
        match fsat::open_dir_resolved(root.fd.as_fd(), parents, root.resolve()) {
            Ok((fd, _)) => Some(fd),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => None,
            Err(error) => return Err(fs_error(error)),
        }
    };
    Ok((root, parent, name.clone()))
}

/// POST /api/files/trash/restore
pub(super) async fn restore(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<IdsRequest>,
) -> AppResult<Json<Value>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let space = agent.space.clone();
    blocking(move || {
        let plan = resolve_ids(&space, &body.ids, &shares)?;
        // Every destination first: a conflict restores nothing.
        let mut targets = HashSet::new();
        for (_, item) in &plan {
            if !targets.insert(item.original_path.clone()) {
                return Err(AppError::conflict(format!(
                    "\"{}\" is restored twice in one request",
                    item.original_path
                )));
            }
            let (_, parent, name) = target_parent(&space, &item.target, &shares, false)?;
            if let Some(parent) = parent {
                if fsat::stat_at(parent.as_fd(), &name)
                    .map_err(fs_error)?
                    .is_some()
                {
                    return Err(AppError::conflict(format!(
                        "\"{}\" already exists; rename or remove it before restoring",
                        item.original_path
                    )));
                }
            }
        }
        let mut restored = Vec::with_capacity(plan.len());
        let mut home_bytes = 0i64;
        for (trash, item) in &plan {
            check(&cancel)?;
            let (root, parent, name) = target_parent(&space, &item.target, &shares, true)?;
            let parent = parent.ok_or_else(|| AppError::internal("no destination folder"))?;
            let conflict = || {
                AppError::conflict(format!(
                    "\"{}\" already exists; rename or remove it before restoring",
                    item.original_path
                ))
            };
            let same_root = trash.root.info.same_inode(&root.info);
            let renamed = if same_root {
                match fsat::rename_noreplace(trash.files.as_fd(), &item.name, parent.as_fd(), &name)
                {
                    Ok(()) => true,
                    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => false,
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                        return Err(conflict())
                    }
                    Err(error) => return Err(fs_error(error)),
                }
            } else {
                false
            };
            if !renamed {
                // Into another namespace (a file an app moved from a shared
                // folder into the home trash) or another file system: a copy
                // takes the destination's group and bits, then the payload
                // goes.
                let mut copied = fsat::Copied::default();
                let mut created = None;
                let result = fsat::copy_at(
                    trash.files.as_fd(),
                    &item.name,
                    &item.file,
                    Some(trash.dev),
                    parent.as_fd(),
                    root.kind == RootKind::Share,
                    &cancel,
                    "",
                    &mut copied,
                    &mut created,
                );
                if let Err(error) = result {
                    if let Some(made) = created {
                        fsat::remove_created(parent.as_fd(), &name, &made, root.tree_fs());
                    }
                    return Err(match error {
                        AppError::Conflict(_) => conflict(),
                        other => other,
                    });
                }
                fsat::remove_at(trash.files.as_fd(), &item.name, Some(trash.dev))?;
                if trash.place == Place::Home && root.kind == RootKind::Share {
                    home_bytes =
                        home_bytes.saturating_sub(i64::try_from(item.size).unwrap_or(i64::MAX));
                }
            }
            let _ = fsat::unlink_at(trash.info.as_fd(), &info_name(&item.name));
            if item.file.is_dir() {
                trash.forget_sizes(std::slice::from_ref(&item.name));
            }
            restored.push(json!({"id": item.id.encode(), "path": item.original_path}));
        }
        Ok(Json(json!({
            "ok": true,
            "restored": restored,
            "homeBytes": home_bytes,
        })))
    })
    .await
}

/// POST /api/files/trash/delete: remove entries for good.
pub(super) async fn delete_permanent(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<IdsRequest>,
) -> AppResult<Json<Value>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let space = agent.space.clone();
    blocking(move || {
        let plan = resolve_ids(&space, &body.ids, &shares)?;
        let mut deleted = 0i64;
        let mut bytes = 0i64;
        let mut home_bytes = 0i64;
        let mut paths = Vec::with_capacity(plan.len());
        for (trash, item) in &plan {
            trash.remove(&item.name)?;
            if item.file.is_dir() {
                trash.forget_sizes(std::slice::from_ref(&item.name));
            }
            let size = i64::try_from(item.size).unwrap_or(i64::MAX);
            deleted += 1;
            bytes = bytes.saturating_add(size);
            if trash.place == Place::Home {
                home_bytes = home_bytes.saturating_sub(size);
            }
            paths.push(item.original_path.clone());
        }
        Ok(Json(json!({
            "ok": true,
            "deleted": deleted,
            "bytes": bytes,
            "originalPaths": paths,
            "homeBytes": home_bytes,
        })))
    })
    .await
}

/// DELETE /api/files/trash: empty every trash of the request (HOME and
/// the shared folders the server named), info files without payload and
/// payloads without info file included.
pub(super) async fn empty(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let space = agent.space.clone();
    blocking(move || {
        let (trashes, items) = gather(&space, &shares)?;
        let mut deleted = 0i64;
        let mut bytes = 0i64;
        let mut home_bytes = 0i64;
        let mut paths = Vec::new();
        for (index, item) in &items {
            let size = i64::try_from(item.size).unwrap_or(i64::MAX);
            deleted += 1;
            bytes = bytes.saturating_add(size);
            if trashes[*index].place == Place::Home {
                home_bytes = home_bytes.saturating_sub(size);
            }
            if paths.len() < MAX_REPORTED_PATHS {
                paths.push(item.original_path.clone());
            }
        }
        for trash in &trashes {
            let (names, _) =
                fsat::names_limited(trash.files.as_fd(), usize::MAX).map_err(fs_error)?;
            for name in names {
                // Kept: what is on another file system (a mount).
                if let Err(error) = fsat::remove_at(trash.files.as_fd(), &name, Some(trash.dev)) {
                    tracing::warn!(error = %error, "cannot remove a recycle bin entry");
                }
            }
            let (names, _) =
                fsat::names_limited(trash.info.as_fd(), usize::MAX).map_err(fs_error)?;
            for name in names.iter().filter(|name| name.ends_with(INFO_SUFFIX)) {
                let _ = fsat::unlink_at(trash.info.as_fd(), name);
            }
            trash.save_sizes(&BTreeMap::new());
        }
        Ok(Json(json!({
            "ok": true,
            "deleted": deleted,
            "bytes": bytes,
            "originalPaths": paths,
            "homeBytes": home_bytes,
        })))
    })
    .await
}

/// Bytes in the trashes of the shared folders of a request (for the usage
/// view; HOME's trash is part of the HOME scan).
pub(super) fn share_trash_bytes(space: &HomeSpace, shares: &ShareRoots) -> u64 {
    let mut total = 0u64;
    for place in places(shares).into_iter().skip(1) {
        if let Ok(Some(trash)) = space.trash(&place, shares, false) {
            if let Ok(items) = trash.items(space, shares) {
                total = items
                    .iter()
                    .fold(total, |sum, item| sum.saturating_add(item.size));
            }
        }
    }
    total
}

/// Whether an entry at the top of a shared folder is a trash folder (never
/// listed or served).
pub(super) fn hidden_in_share(name: &str) -> bool {
    xdg_trash::is_trash_dir_name(name)
}
