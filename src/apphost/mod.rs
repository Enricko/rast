//! Package Center apps (`type: "app"` packages, see
//! [`crate::packages::manifest::AppSpec`]).
//!
//! For each installed app package (its default version) RAST:
//!
//! * runs the app's server program as a child process, restarting it when it
//!   stops (1 s, 2 s, 4 s … at most 60 s apart), with a cleared environment:
//!   `RAST_APP_SOCKET` is where it serves HTTP, `RAST_HOST_SOCKET` where RAST
//!   answers the host API ([`host_api`]), `RAST_APP_TOKEN` the secret both
//!   sides send, `RAST_APP_DATA` its own data folder;
//! * forwards requests below the app's `apiPrefix` to it ([`proxy`]) once its
//!   own sign-in, CSRF and app access checks passed, saying who asks;
//! * serves the app's window files for the desktop to load ([`proxy::files`]).
//!
//! Everything an app keeps lives in `<data_dir>/apps/<app id>/` (0700):
//! `app.sock`, `host.sock` and `data/`. Installing, updating, switching or
//! removing the package restarts or stops the app (on `package.changed`).

pub mod host_api;
pub mod migrate;
pub mod proxy;

use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use rand::RngCore;
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::watch,
};

use crate::{
    db::now_ms,
    packages::{
        manifest::{AppSpec, PackageIcon},
        store,
    },
    state::AppState,
};

/// What RAST knows of a running app's server.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    /// `starting`, `running`, `restarting`, `stopped` or `none` (window only).
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub since: i64,
}

/// An installed app package, as RAST runs it.
pub struct App {
    pub app_id: String,
    pub package_id: String,
    pub name: String,
    pub description: String,
    pub icon: Option<PackageIcon>,
    pub version: String,
    pub revision: u32,
    /// The installed version's folder.
    pub root: PathBuf,
    pub spec: AppSpec,
    /// `<data_dir>/apps/<app id>`.
    pub dir: PathBuf,
    pub token: String,
    stop: watch::Sender<bool>,
    status: Mutex<ServerStatus>,
}

impl App {
    pub fn app_socket(&self) -> PathBuf {
        self.dir.join("app.sock")
    }
    pub fn host_socket(&self) -> PathBuf {
        self.dir.join("host.sock")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.dir.join("data")
    }
    pub fn status(&self) -> ServerStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_status(&self, change: impl FnOnce(&mut ServerStatus)) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut status);
    }
    /// The API path below which requests go to this app, if it has a server.
    pub fn api_prefix(&self) -> Option<&str> {
        self.spec.server.as_ref().map(|s| s.api_prefix.as_str())
    }
    /// Whether `path` is the app's API prefix or below it.
    pub fn serves(&self, path: &str) -> bool {
        self.api_prefix().is_some_and(|prefix| {
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
    fn same_install(&self, other: &Installed) -> bool {
        self.package_id == other.package_id
            && self.version == other.version
            && self.revision == other.revision
            && self.root == other.root
    }
}

/// An installed app package version (from the package store).
struct Installed {
    package_id: String,
    name: String,
    description: String,
    icon: Option<PackageIcon>,
    version: String,
    revision: u32,
    root: PathBuf,
    spec: AppSpec,
}

/// The running apps, by app id.
#[derive(Clone, Default)]
pub struct AppHost {
    apps: Arc<RwLock<BTreeMap<String, Arc<App>>>>,
    sync: Arc<tokio::sync::Mutex<()>>,
}

impl AppHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> Vec<Arc<App>> {
        self.apps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, app_id: &str) -> Option<Arc<App>> {
        self.apps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(app_id)
            .cloned()
    }

    /// The app whose API `path` is under.
    pub fn for_path(&self, path: &str) -> Option<Arc<App>> {
        self.list().into_iter().find(|app| app.serves(path))
    }

    /// Start, restart or stop apps to match what is installed: the default
    /// version of every app package.
    pub async fn sync(&self, state: &AppState) {
        let _one_at_a_time = self.sync.lock().await;
        let installed = match installed_apps(state).await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(%error, "cannot read installed app packages");
                return;
            }
        };
        let mut wanted: BTreeMap<String, Installed> = BTreeMap::new();
        for app in installed {
            // Two packages naming the same app: the first (by package id) runs.
            wanted.entry(app.spec.id.clone()).or_insert(app);
        }
        let mut stopping = Vec::new();
        let mut starting = Vec::new();
        {
            let mut apps = self.apps.write().unwrap_or_else(|e| e.into_inner());
            let current: Vec<String> = apps.keys().cloned().collect();
            for id in current {
                let keep = wanted.get(&id).is_some_and(|w| apps[&id].same_install(w));
                if !keep {
                    if let Some(old) = apps.remove(&id) {
                        stopping.push(old);
                    }
                }
            }
            for (id, install) in wanted {
                if apps.contains_key(&id) {
                    continue;
                }
                let (stop, _) = watch::channel(false);
                let app = Arc::new(App {
                    dir: state.config.data_dir.join("apps").join(&id),
                    app_id: id.clone(),
                    package_id: install.package_id,
                    name: install.name,
                    description: install.description,
                    icon: install.icon,
                    version: install.version,
                    revision: install.revision,
                    root: install.root,
                    spec: install.spec,
                    token: new_token(),
                    stop,
                    status: Mutex::new(ServerStatus {
                        state: "starting".into(),
                        since: now_ms(),
                        ..Default::default()
                    }),
                });
                apps.insert(id, app.clone());
                starting.push(app);
            }
        }
        for app in stopping {
            tracing::info!(app = %app.app_id, version = %app.version, "stopping app");
            let _ = app.stop.send(true);
            // Wait for the old server to leave before the new one binds.
            let mut waited = 0;
            while app.status().state != "stopped" && waited < 150 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += 1;
            }
        }
        for app in starting {
            tracing::info!(app = %app.app_id, version = %app.version, "starting app");
            tokio::spawn(supervise(state.clone(), app));
        }
    }

    /// Start installed apps now and follow package changes.
    pub fn spawn(state: AppState) {
        tokio::spawn(async move {
            let mut events = state.events.subscribe();
            state.apps.sync(&state).await;
            loop {
                match events.recv().await {
                    Ok(event) if event.kind == "package.changed" => {
                        // Several changes in a row: one sync after they settle.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        while events.try_recv().is_ok() {}
                        state.apps.sync(&state).await;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        state.apps.sync(&state).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Stop every app (RAST is shutting down).
    pub async fn stop_all(&self) {
        let apps: Vec<Arc<App>> = {
            let mut map = self.apps.write().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *map).into_values().collect()
        };
        for app in &apps {
            let _ = app.stop.send(true);
        }
        for app in &apps {
            let mut waited = 0;
            while app.status().state != "stopped" && waited < 120 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += 1;
            }
        }
    }
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

async fn installed_apps(state: &AppState) -> crate::error::AppResult<Vec<Installed>> {
    let packages = store::packages(&state.db).await?;
    let versions = store::versions(&state.db, None).await?;
    let mut out = Vec::new();
    for package in packages {
        let Some(default) = package.default_version.as_deref() else {
            continue;
        };
        let Some(row) = versions
            .iter()
            .find(|v| v.package_id == package.id && v.version == default)
        else {
            continue;
        };
        let Some(spec) = row.manifest.app.clone() else {
            continue;
        };
        // App packages come from the official catalog only.
        if package.source != "official" {
            continue;
        }
        out.push(Installed {
            package_id: package.id.clone(),
            name: row.manifest.name.clone(),
            description: row.manifest.description.clone(),
            icon: row.manifest.icon.clone(),
            version: row.version.clone(),
            revision: row.revision,
            root: PathBuf::from(&row.install_path),
            spec,
        });
    }
    Ok(out)
}

fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("data"))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(dir.join("data"), std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Run one app: its host API and (when it has one) its server, until it is
/// stopped.
async fn supervise(state: AppState, app: Arc<App>) {
    let mut stop = app.stop.subscribe();
    if let Err(error) = prepare_dir(&app.dir) {
        tracing::error!(app = %app.app_id, %error, "cannot prepare the app folder");
        app.set_status(|s| {
            s.state = "stopped".into();
            s.last_error = Some(format!("cannot prepare {}: {error}", app.dir.display()));
        });
        return;
    }
    let host = match host_api::serve(state.clone(), app.clone(), app.stop.subscribe()).await {
        Ok(task) => Some(task),
        Err(error) => {
            tracing::error!(app = %app.app_id, %error, "cannot open the host API socket");
            None
        }
    };
    let Some(server) = app.spec.server.clone() else {
        app.set_status(|s| {
            s.state = "none".into();
            s.since = now_ms();
        });
        let _ = stop.wait_for(|stopped| *stopped).await;
        finish(&app, host).await;
        return;
    };
    let program = app.root.join(&server.executable);
    let mut failures: u32 = 0;
    loop {
        if *stop.borrow() {
            break;
        }
        let _ = std::fs::remove_file(app.app_socket());
        let mut command = Command::new(&program);
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", app.data_dir())
            .env("LANG", "C.UTF-8")
            .env("TMPDIR", std::env::temp_dir())
            .env("RAST_VERSION", crate::VERSION)
            .env("RAST_HOST_API", server.host_api.to_string())
            .env("RAST_APP_ID", &app.app_id)
            .env("RAST_APP_VERSION", &app.version)
            .env("RAST_APP_DATA", app.data_dir())
            .env("RAST_APP_SOCKET", app.app_socket())
            .env("RAST_HOST_SOCKET", app.host_socket())
            .env("RAST_APP_TOKEN", &app.token)
            .current_dir(app.data_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Ok(filter) = std::env::var("RUST_LOG") {
            command.env("RUST_LOG", filter);
        }
        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures = failures.saturating_add(1);
                tracing::error!(app = %app.app_id, %error, "cannot start the app");
                app.set_status(|s| {
                    s.state = "restarting".into();
                    s.pid = None;
                    s.last_error = Some(format!("cannot start {}: {error}", program.display()));
                    s.since = now_ms();
                });
                if wait_or_stop(&mut stop, backoff(failures)).await {
                    break;
                }
                continue;
            }
        };
        let pid = child.id();
        app.set_status(|s| {
            s.state = "running".into();
            s.pid = pid;
            s.since = now_ms();
        });
        for (stream, is_err) in [
            (
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                false,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                true,
            ),
        ] {
            let Some(stream) = stream else { continue };
            let id = app.app_id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if is_err {
                        tracing::warn!(target: "rast::app", app = %id, "{line}");
                    } else {
                        tracing::info!(target: "rast::app", app = %id, "{line}");
                    }
                }
            });
        }
        let exit = tokio::select! {
            exit = child.wait() => Some(exit),
            _ = stop.wait_for(|stopped| *stopped) => None,
        };
        match exit {
            None => {
                terminate(&mut child).await;
                break;
            }
            Some(exit) => {
                let detail = match exit {
                    Ok(code) => format!("the app stopped ({code})"),
                    Err(error) => format!("the app stopped: {error}"),
                };
                if started.elapsed() > Duration::from_secs(300) {
                    failures = 0;
                }
                failures = failures.saturating_add(1);
                tracing::warn!(app = %app.app_id, "{detail}; starting it again");
                app.set_status(|s| {
                    s.state = "restarting".into();
                    s.pid = None;
                    s.restarts = s.restarts.saturating_add(1);
                    s.last_error = Some(detail);
                    s.since = now_ms();
                });
                if wait_or_stop(&mut stop, backoff(failures)).await {
                    break;
                }
            }
        }
    }
    finish(&app, host).await;
}

async fn finish(app: &App, host: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = host {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    let _ = std::fs::remove_file(app.app_socket());
    let _ = std::fs::remove_file(app.host_socket());
    app.set_status(|s| {
        s.state = "stopped".into();
        s.pid = None;
        s.since = now_ms();
    });
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

/// Wait `delay`; true when the app was stopped meanwhile.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::time::timeout(delay, stop.wait_for(|stopped| *stopped))
        .await
        .is_ok()
}

/// SIGTERM, then SIGKILL after 10 s.
async fn terminate(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on the child we started and still own.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarts_slow_down_to_a_minute() {
        let waits: Vec<u64> = (1..=9).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
