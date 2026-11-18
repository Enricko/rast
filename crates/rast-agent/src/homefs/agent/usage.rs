//! What a one-HOME account's HOME holds (ADR 0005, section 6): the RAST
//! fallback quota counts it (`storage_usage.used_bytes`), and the usage view
//! shows where it goes.
//!
//! The scan counts allocated bytes (`st_blocks`) of every entry on HOME's
//! file system, each hard-linked inode once, dot folders included (apps,
//! caches and the trash count, D2), and never follows a link or enters a
//! mount. Locations follow the usage view: Files, Apps (per-user Flatpak,
//! `~/.var/app`, launcher trees, `~/.local/bin`, AppImages), Caches and
//! Trash. The per-type breakdown keeps its old meaning: apparent sizes of
//! the documents outside dot folders.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

use super::{HomeAgent, HomeSpace};
use crate::error::AppResult;
use crate::fileagent::blocking;
use crate::homefs::fsat::{self, fs_error, Info, Kind, Step};
use crate::homefs::ShareRoots;
use crate::storage::ops::classify_file;
use crate::storage::{MAX_TREE_ENTRIES, TEMP_PREFIX};

/// Internal route of the scan (not part of the File Station API).
pub const USAGE_ROUTE: &str = "/internal/usage";

/// Deepest folder the scan enters.
const MAX_SCAN_DEPTH: usize = 128;

/// Folders below HOME whose contents are apps.
const APP_TREES: [&[&str]; 4] = [
    &[".local", "share", "flatpak"],
    &[".var", "app"],
    &[".local", "bin"],
    &[".local", "share", "JetBrains"],
];

/// Where an entry below HOME belongs in the usage view.
pub fn location(path: &[String], name: &str, is_file: bool) -> &'static str {
    let starts = |prefix: &[&str]| {
        path.len() >= prefix.len() && path.iter().zip(prefix).all(|(a, b)| a == b)
    };
    if starts(&crate::storage::xdg_trash::HOME_TRASH) {
        "trash"
    } else if starts(&[".cache"]) {
        "caches"
    } else if APP_TREES.iter().any(|tree| starts(tree))
        || (is_file && name.to_ascii_lowercase().ends_with(".appimage"))
    {
        "apps"
    } else {
        "files"
    }
}

/// The totals of one scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Scan {
    pub allocated: u64,
    pub apparent: u64,
    pub by_location: BTreeMap<&'static str, u64>,
    pub by_type: BTreeMap<&'static str, u64>,
    pub entries: usize,
    pub truncated: bool,
}

/// Scan the tree below the folder `home` (opened for reading), whose own
/// `stat` is `info`.
pub fn scan(home: std::os::fd::OwnedFd, info: &Info) -> std::io::Result<Scan> {
    let mut out = Scan {
        allocated: info.allocated,
        ..Default::default()
    };
    for key in ["files", "apps", "caches", "trash"] {
        out.by_location.insert(key, 0);
    }
    *out.by_location.entry("files").or_default() += info.allocated;
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    fsat::walk(home, MAX_SCAN_DEPTH, Some(info.dev), |visit| {
        out.entries += 1;
        if out.entries > MAX_TREE_ENTRIES {
            out.truncated = true;
            return Step::Stop;
        }
        let entry = visit.info;
        if entry.nlink > 1 && entry.kind != Kind::Dir && !linked.insert((entry.dev, entry.ino)) {
            return Step::Over;
        }
        let is_file = entry.kind == Kind::File;
        let place = location(visit.path, visit.name, is_file);
        out.allocated = out.allocated.saturating_add(entry.allocated);
        let bucket = out.by_location.entry(place).or_default();
        *bucket = bucket.saturating_add(entry.allocated);
        if is_file {
            out.apparent = out.apparent.saturating_add(entry.len);
            let document = place == "files"
                && !visit.path.iter().any(|part| part.starts_with('.'))
                && !visit.name.starts_with(TEMP_PREFIX);
            if document {
                let bucket = out.by_type.entry(classify_file(visit.name)).or_default();
                *bucket = bucket.saturating_add(entry.len);
            }
        }
        if entry.kind == Kind::Symlink {
            Step::Over
        } else {
            Step::Into
        }
    })?;
    Ok(out)
}

/// GET /internal/usage
pub(super) async fn usage(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let space: HomeSpace = agent.space.clone();
    blocking(move || {
        let home = super::trash::readable_root(&space)?;
        let info = fsat::stat_fd(&home).map_err(fs_error)?;
        let found = scan(home, &info).map_err(fs_error)?;
        let shared_trash = super::trash::share_trash_bytes(&space, &shares);
        let home_trash = found.by_location.get("trash").copied().unwrap_or(0);
        Ok(Json(json!({
            "allocatedBytes": found.allocated,
            "apparentBytes": found.apparent,
            "byLocation": found.by_location,
            "byType": found.by_type,
            "trashBytes": home_trash.saturating_add(shared_trash),
            "entries": found.entries,
            "truncated": found.truncated,
        })))
    })
    .await
}
