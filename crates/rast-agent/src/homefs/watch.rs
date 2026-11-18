//! Realtime events of one HOME (ADR 0005, section 2): `GET /api/files/watch`
//! streams the logical paths that changed, one JSON object per line.
//!
//! Only the visible tree is watched: HOME and every directory below it whose
//! name does not start with a dot, one non-recursive inotify watch each, at
//! most [`MAX_WATCHED_DIRS`]. Folders created later get their own watch.
//! Changes are debounced, and a flood becomes one `resync` line. The stream
//! ends when the agent stops; it never keeps the agent alive.
//!
//! Lines: `{"ready":true}` once the watches are in place, then
//! `{"paths":["/docs/a.txt",…]}` and `{"resync":true}`.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::Response;
use notify::event::{ModifyKind, RemoveKind};
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::fileagent::{blocking, Agent};
use crate::storage::LogicalPath;

use super::agent::HomeSpace;

/// Most directories one stream watches.
pub const MAX_WATCHED_DIRS: usize = 8192;
/// How long changes are collected before they are sent.
pub const DEBOUNCE: Duration = Duration::from_millis(400);
/// Most paths in one line.
pub const MAX_PATHS_PER_LINE: usize = 100;
/// Pending paths beyond this become one `resync`.
const MAX_PENDING: usize = 4096;
/// At most one `resync` line this often.
const RESYNC_INTERVAL: Duration = Duration::from_secs(2);
const CHANNEL: usize = 4096;
const TICK: Duration = Duration::from_millis(100);

/// The logical path of `path`, when it is in the visible tree below `home`.
pub fn visible_logical(home: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(home).ok()?;
    let mut segments = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_str()?;
        if name.starts_with('.') {
            return None;
        }
        segments.push(name);
    }
    if segments.is_empty() {
        return None;
    }
    LogicalPath::parse(&format!("/{}", segments.join("/")))
        .ok()
        .map(|logical| logical.as_string())
}

/// One non-recursive watch per visible directory.
struct Dirs<W: Watcher> {
    watcher: W,
    home: PathBuf,
    count: usize,
    full: bool,
}

impl<W: Watcher> Dirs<W> {
    /// Watch `dir` and the visible directories below it.
    fn add_tree(&mut self, dir: &Path) {
        let walker = walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|item| {
                item.file_type().is_dir()
                    && (item.depth() == 0
                        || item
                            .file_name()
                            .to_str()
                            .is_some_and(|name| !name.starts_with('.')))
            });
        for item in walker.flatten() {
            if self.count >= MAX_WATCHED_DIRS {
                if !self.full {
                    self.full = true;
                    tracing::warn!(
                        home = %self.home.display(),
                        limit = MAX_WATCHED_DIRS,
                        "too many folders to watch; later changes may not appear at once"
                    );
                }
                return;
            }
            if self
                .watcher
                .watch(item.path(), RecursiveMode::NonRecursive)
                .is_ok()
            {
                self.count += 1;
            }
        }
    }

    fn removed(&mut self, folders: usize) {
        self.count = self.count.saturating_sub(folders);
    }
}

fn line(value: Value) -> Bytes {
    let mut text = value.to_string();
    text.push('\n');
    Bytes::from(text)
}

fn relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(_)
            | EventKind::Any
            | EventKind::Other
    )
}

/// `GET /api/files/watch`.
pub async fn stream(State(agent): State<Arc<Agent<HomeSpace>>>) -> AppResult<Response> {
    let home = agent.space.home().to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::channel::<notify::Event>(CHANNEL);
    let overflow = Arc::new(AtomicBool::new(false));
    let watcher = notify::recommended_watcher({
        let overflow = overflow.clone();
        move |result: notify::Result<notify::Event>| match result {
            Ok(event) if relevant(&event.kind) => {
                // Never block the watcher thread: a full channel is a resync.
                if tx.try_send(event).is_err() {
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(error = %error, "file watcher error"),
        }
    })
    .map_err(|error| AppError::internal(format!("cannot watch the home folder: {error}")))?;
    let dirs = blocking({
        let home = home.clone();
        move || {
            let mut dirs = Dirs {
                watcher,
                home: home.clone(),
                count: 0,
                full: false,
            };
            dirs.add_tree(&home);
            Ok(dirs)
        }
    })
    .await?;
    let (out, lines) = tokio::sync::mpsc::channel::<Bytes>(64);
    tokio::spawn(pump(home, dirs, rx, overflow, out, agent.stopping()));
    let body = Body::from_stream(futures::stream::unfold(lines, |mut lines| async move {
        lines
            .recv()
            .await
            .map(|bytes| (Ok::<_, std::io::Error>(bytes), lines))
    }));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn pump<W: Watcher>(
    home: PathBuf,
    mut dirs: Dirs<W>,
    mut events: tokio::sync::mpsc::Receiver<notify::Event>,
    overflow: Arc<AtomicBool>,
    out: tokio::sync::mpsc::Sender<Bytes>,
    stopping: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(stopping);
    if out.send(line(json!({"ready": true}))).await.is_err() {
        return;
    }
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_resync: Option<Instant> = None;
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let mut send: Option<Vec<Bytes>> = None;
        tokio::select! {
            _ = &mut stopping => return,
            _ = out.closed() => return,
            event = events.recv() => {
                let Some(event) = event else { return };
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => {
                        for path in &event.paths {
                            let folder = std::fs::symlink_metadata(path)
                                .is_ok_and(|meta| meta.is_dir());
                            if folder && visible_logical(&home, path).is_some() {
                                dirs.add_tree(path);
                            }
                        }
                    }
                    EventKind::Remove(RemoveKind::Folder) => dirs.removed(event.paths.len()),
                    _ => {}
                }
                for path in &event.paths {
                    if let Some(logical) = visible_logical(&home, path) {
                        pending.insert(logical);
                    }
                }
                if pending.len() > MAX_PENDING {
                    overflow.store(true, Ordering::Relaxed);
                }
                flush_at = Some(Instant::now() + DEBOUNCE);
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                if overflow.load(Ordering::Relaxed) {
                    let allowed = last_resync
                        .is_none_or(|last| now.duration_since(last) >= RESYNC_INTERVAL);
                    if allowed {
                        overflow.store(false, Ordering::Relaxed);
                        pending.clear();
                        flush_at = None;
                        last_resync = Some(now);
                        send = Some(vec![line(json!({"resync": true}))]);
                    }
                } else if flush_at.is_some_and(|deadline| now >= deadline) {
                    flush_at = None;
                    let paths: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                    send = Some(
                        paths
                            .chunks(MAX_PATHS_PER_LINE)
                            .map(|chunk| line(json!({"paths": chunk})))
                            .collect(),
                    );
                }
            }
        }
        for bytes in send.into_iter().flatten() {
            if out.send(bytes).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_visible_tree_has_logical_paths() {
        let home = Path::new("/srv/rast/homes/alice");
        assert_eq!(
            visible_logical(home, &home.join("docs/a.txt")).as_deref(),
            Some("/docs/a.txt")
        );
        for hidden in [
            ".bashrc",
            ".cache/x",
            "docs/.git/HEAD",
            "docs/.rast-tmp-1",
            "",
        ] {
            assert_eq!(visible_logical(home, &home.join(hidden)), None, "{hidden}");
        }
        assert_eq!(visible_logical(home, Path::new("/etc/passwd")), None);
        assert_eq!(visible_logical(home, &home.join("a/../b")), None);
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_agent_stops() {
        use futures::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let agent = Agent::new("t".repeat(64), HomeSpace::new(&home, &shared).unwrap());
        let response = stream(State(agent.clone())).await.unwrap();
        let mut body = response.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"{\"ready\":true}\n");
        agent.stop();
        let end = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("the stream must end when the agent stops");
        assert!(end.is_none(), "{end:?}");
    }
}
