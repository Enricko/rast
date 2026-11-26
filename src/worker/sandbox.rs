//! Terminal sandboxes, the worker's side (see [`crate::sandbox`] for the
//! design). The worker owns Docker, so it builds the image, keeps one
//! container per account, and starts each shell as `docker exec` on a pseudo
//! terminal owned by the shared terminal helper ([`crate::terminal::helper`]).
//!
//! Nothing the web server sends becomes a path or a Docker argument without
//! being checked here: the home folder is `<homes>/<name>` for a validated
//! name, must be a real directory (no symlinks) owned by an ordinary account,
//! and every other argument is fixed.

use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, net::UnixListener, process::Command, time::Instant};

use super::{
    apply_socket_permissions, docker_command, prepare_socket_path, run_docker, stderr_summary,
    WorkerOptions,
};
use crate::{
    config::Config,
    sandbox::{
        image_ref, socket_path, valid_name, BUILD_TIMEOUT, CONTAINER_PREFIX, CPUS, DOCKERFILE,
        IDLE_REMOVE, IMAGE_REPO, LABEL, LABEL_SPEC, LABEL_USER, LIMITS, MEMORY_MB, PIDS, TMP_MB,
    },
    terminal::helper::{self, Helper, Launcher, Refusal},
};

const QUICK: Duration = Duration::from_secs(30);
const START: Duration = Duration::from_secs(60);
/// How often idle containers are looked for.
const REAP_EVERY: Duration = Duration::from_secs(60);

type Build = Shared<BoxFuture<'static, Result<(), String>>>;

/// Image builds: at most one at a time, and the last failure.
#[derive(Default)]
struct Builds {
    running: Mutex<Option<Build>>,
    failed: Mutex<Option<String>>,
}

pub(super) struct SandboxLauncher {
    options: Arc<WorkerOptions>,
    homes_dir: PathBuf,
    packages_dir: PathBuf,
    /// Per-account passwd/group files (`<run dir>/shells/<id>/`).
    identities: PathBuf,
    image: String,
    builds: Arc<Builds>,
    /// One container start at a time.
    starting: tokio::sync::Mutex<()>,
}

/// A home folder that may go into a container.
struct Home {
    name: String,
    path: PathBuf,
    uid: u32,
    gid: u32,
}

impl SandboxLauncher {
    fn new(cfg: &Config, options: Arc<WorkerOptions>) -> Self {
        let identities = options
            .socket_path
            .parent()
            .map(|dir| dir.join("shells"))
            .unwrap_or_else(|| PathBuf::from("/run/rast/shells"));
        Self {
            options,
            homes_dir: cfg.homes_dir(),
            packages_dir: cfg.data_dir.join("packages"),
            identities,
            image: image_ref(),
            builds: Arc::new(Builds::default()),
            starting: tokio::sync::Mutex::new(()),
        }
    }

    async fn docker(
        &self,
        args: Vec<OsString>,
        limit: Duration,
    ) -> Result<super::CommandOutput, String> {
        run_docker(&self.options, args, None, limit, 256 * 1024)
            .await
            .map_err(|error| error.message)
    }

    async fn image_present(&self) -> bool {
        self.docker(
            args(&["image", "inspect", "--format", "{{.Id}}", &self.image]),
            QUICK,
        )
        .await
        .is_ok_and(|output| output.success)
    }

    fn building(&self) -> bool {
        self.builds
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Start building the image unless a build is running; never waits.
    fn start_build(&self) {
        let mut running = self
            .builds
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if running.is_some() {
            return;
        }
        let options = self.options.clone();
        let image = self.image.clone();
        let builds = self.builds.clone();
        let task = tokio::spawn(async move {
            tracing::info!(%image, "building the Terminal sandbox image");
            let result = build_image(&options, &image).await;
            match &result {
                Ok(()) => {
                    tracing::info!(%image, "built the Terminal sandbox image");
                    remove_old_images(&options, &image).await;
                }
                Err(error) => {
                    tracing::warn!(%image, %error, "building the Terminal sandbox image failed")
                }
            }
            *builds.failed.lock().unwrap_or_else(|p| p.into_inner()) = result.clone().err();
            *builds.running.lock().unwrap_or_else(|p| p.into_inner()) = None;
            result
        });
        let build: Build = async move { task.await.unwrap_or_else(|e| Err(e.to_string())) }
            .boxed()
            .shared();
        *running = Some(build);
        *self.builds.failed.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `ready`, `building`, `failed` (with the error) or `missing`.
    async fn image_state(&self) -> (&'static str, Option<String>) {
        if self.building() {
            return ("building", None);
        }
        if self.image_present().await {
            return ("ready", None);
        }
        let failed = self
            .builds
            .failed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        match failed {
            Some(error) => ("failed", Some(error)),
            None => ("missing", None),
        }
    }

    /// `<homes>/<name>`: a real directory owned by an ordinary account.
    fn home(&self, name: &str) -> Result<Home, Refusal> {
        if !valid_name(name) {
            return Err(Refusal::new(
                "invalid",
                "Terminal cannot be used with this account name",
            ));
        }
        let unusable = |why: &str| {
            Refusal::new(
                "unavailable",
                format!("Your home folder cannot be used by Terminal: {why}"),
            )
        };
        let homes = std::fs::canonicalize(&self.homes_dir)
            .map_err(|e| unusable(&format!("cannot read {}: {e}", self.homes_dir.display())))?;
        let path = homes.join(name);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| unusable(&e.to_string()))?;
        if !meta.file_type().is_dir() {
            return Err(unusable("it is not a folder"));
        }
        if meta.uid() == 0 || meta.gid() == 0 {
            return Err(unusable(
                "it belongs to root; an administrator can give it back to the RAST account",
            ));
        }
        // Docker's --mount list is comma separated.
        if [&path, &self.packages_dir, &self.identities]
            .iter()
            .any(|p| p.to_string_lossy().contains(','))
        {
            return Err(unusable("its path contains a comma"));
        }
        Ok(Home {
            name: name.to_string(),
            path,
            uid: meta.uid(),
            gid: meta.gid(),
        })
    }

    /// passwd and group files that name the account inside its container.
    fn write_identity(&self, owner: i64, home: &Home) -> Result<PathBuf, Refusal> {
        let dir = self.identities.join(owner.to_string());
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
            let (name, uid, gid) = (&home.name, home.uid, home.gid);
            let passwd = format!(
                "root:x:0:0:root:/root:/usr/sbin/nologin\n\
                 nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
                 {name}:x:{uid}:{gid}:{name}:/home/{name}:/bin/bash\n"
            );
            let mut group = "root:x:0:\nnogroup:x:65534:\n".to_string();
            if gid != 65534 {
                group.push_str(&format!("{name}:x:{gid}:\n"));
            }
            for (file, text) in [("passwd", passwd), ("group", group)] {
                let path = dir.join(file);
                std::fs::write(&path, text)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            }
            Ok(())
        };
        write().map_err(|e| {
            Refusal::new(
                "unavailable",
                format!("cannot prepare the terminal container: {e}"),
            )
        })?;
        Ok(dir)
    }

    fn runtimes(&self) -> Option<(PathBuf, PathBuf)> {
        let bin = self.packages_dir.join("bin");
        let runtimes = self.packages_dir.join("runtimes");
        (bin.is_dir() && runtimes.is_dir()).then_some((bin, runtimes))
    }

    /// Make sure the account's container runs, made as it should be now.
    async fn ensure_container(&self, owner: i64, home: &Home) -> Result<String, Refusal> {
        let _one_at_a_time = self.starting.lock().await;
        let name = format!("{CONTAINER_PREFIX}{owner}");
        let identity = self.write_identity(owner, home)?;
        let runtimes = self.runtimes();
        let mut spec = Sha256::new();
        for part in [
            self.image.clone(),
            home.path.display().to_string(),
            format!("{}:{}:{}", home.name, home.uid, home.gid),
            format!("{MEMORY_MB}/{CPUS}/{PIDS}/{TMP_MB}/tmp-exec"),
            identity.display().to_string(),
            format!("{runtimes:?}"),
        ] {
            spec.update(part.as_bytes());
            spec.update([0]);
        }
        let spec = hex::encode(spec.finalize())[..16].to_string();

        let state = self
            .docker(
                args(&[
                    "container",
                    "inspect",
                    "--format",
                    &format!(
                        "{{{{.State.Running}}}} {{{{index .Config.Labels \"{LABEL_SPEC}\"}}}}"
                    ),
                    &name,
                ]),
                QUICK,
            )
            .await;
        if matches!(&state, Ok(out) if out.success && out.stdout.trim() == format!("true {spec}")) {
            return Ok(name);
        }
        // Missing, stopped, or made differently: start it afresh (the home
        // folder is all that persists).
        let _ = self.docker(args(&["rm", "-f", &name]), QUICK).await;

        let mount = |source: &Path, target: &Path, readonly: bool| -> OsString {
            let mut spec = OsString::from("type=bind,source=");
            spec.push(source);
            spec.push(",target=");
            spec.push(target);
            if readonly {
                spec.push(",readonly");
            }
            spec
        };
        let home_target = PathBuf::from(format!("/home/{}", home.name));
        let mut run = args(&[
            "run",
            "--detach",
            "--name",
            &name,
            "--label",
            &format!("{LABEL}=1"),
            "--label",
            &format!("{LABEL_USER}={owner}"),
            "--label",
            &format!("{LABEL_SPEC}={spec}"),
            "--hostname",
            "rast",
            "--user",
            &format!("{}:{}", home.uid, home.gid),
            "--read-only",
            "--tmpfs",
            // exec: tools build into /tmp and run from there (`go run`, pip
            // builds); the account can already run files from its home.
            &format!("/tmp:rw,exec,nosuid,nodev,size={TMP_MB}m,mode=1777"),
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--network",
            "none",
            "--memory",
            &format!("{MEMORY_MB}m"),
            "--memory-swap",
            &format!("{MEMORY_MB}m"),
            "--cpus",
            CPUS,
            "--pids-limit",
            &PIDS.to_string(),
            "--init",
            "--restart",
            "no",
            "--log-driver",
            "none",
            "--workdir",
            &home_target.display().to_string(),
        ]);
        let mut mounts = vec![
            mount(&home.path, &home_target, false),
            mount(&identity.join("passwd"), Path::new("/etc/passwd"), true),
            mount(&identity.join("group"), Path::new("/etc/group"), true),
        ];
        if let Some((bin, runtimes)) = &runtimes {
            mounts.push(mount(bin, bin, true));
            mounts.push(mount(runtimes, runtimes, true));
        }
        for spec in mounts {
            run.push("--mount".into());
            run.push(spec);
        }
        run.push(self.image.clone().into());
        let output = self.docker(run, START).await.map_err(|e| {
            Refusal::new("unavailable", format!("could not start your terminal: {e}"))
        })?;
        if !output.success {
            return Err(Refusal::new(
                "unavailable",
                format!("could not start your terminal: {}", stderr_summary(&output)),
            ));
        }
        tracing::info!(user = owner, container = %name, "started a Terminal sandbox");
        Ok(name)
    }

    async fn remove(&self, owner: i64) {
        let name = format!("{CONTAINER_PREFIX}{owner}");
        if let Ok(output) = self.docker(args(&["rm", "-f", &name]), QUICK).await {
            if output.success {
                tracing::info!(user = owner, container = %name, "removed a Terminal sandbox");
            }
        }
        let _ = std::fs::remove_dir_all(self.identities.join(owner.to_string()));
    }

    /// `docker exec` into the account's container, up to the container name
    /// (returned): as the account, in its home folder, with a terminal or
    /// without one (no colours or pagers then).
    async fn exec(
        &self,
        owner: i64,
        owner_name: &str,
        tty: bool,
    ) -> Result<(Command, String), Refusal> {
        let home = self.home(owner_name)?;
        if !self.image_present().await {
            self.start_build();
            return Err(Refusal::new(
                "preparing",
                "Terminal is still being set up on this server; try again in a few minutes",
            ));
        }
        let container = self.ensure_container(owner, &home).await?;
        let home_target = format!("/home/{}", home.name);
        let mut command = docker_command(&self.options);
        command.args(if tty {
            &["exec", "-i", "-t", "-u"][..]
        } else {
            &["exec", "-i", "-u"][..]
        });
        command
            .arg(format!("{}:{}", home.uid, home.gid))
            .args(["-w", &home_target])
            .args(["-e", &format!("HOME={home_target}")])
            .args(["-e", &format!("USER={}", home.name)])
            .args(["-e", &format!("LOGNAME={}", home.name)])
            .args(["-e", &format!("RAST_USER={}", home.name)])
            .args(["-e", "SHELL=/bin/bash"])
            .args(["-e", "LANG=C.UTF-8"]);
        if tty {
            command.args(["-e", "TERM=xterm-256color", "-e", "COLORTERM=truecolor"]);
        } else {
            for setting in [
                "TERM=dumb",
                "PAGER=cat",
                "GIT_PAGER=cat",
                "NO_COLOR=1",
                "DEBIAN_FRONTEND=noninteractive",
            ] {
                command.args(["-e", setting]);
            }
        }
        if let Some((bin, _)) = self.runtimes() {
            let mut env = OsString::from("RAST_PACKAGES_BIN=");
            env.push(bin);
            command.arg("-e").arg(env);
        }
        Ok((command, container))
    }

    /// Accounts that have a sandbox container (running or not).
    async fn containers(&self) -> Vec<i64> {
        let filter = format!("label={LABEL}=1");
        let format = format!("{{{{.Label \"{LABEL_USER}\"}}}}");
        match self
            .docker(
                args(&["ps", "-a", "--filter", &filter, "--format", &format]),
                QUICK,
            )
            .await
        {
            Ok(output) if output.success => output
                .stdout
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl Launcher for SandboxLauncher {
    async fn status(&self) -> Value {
        let (image, error) = self.image_state().await;
        json!({
            "user": "sandbox",
            "image": image,
            "imageError": error,
            "memoryMb": MEMORY_MB,
            "cpus": CPUS,
            "network": false,
        })
    }

    async fn prepare(&self, _owner: i64, owner_name: &str) -> Result<Value, Refusal> {
        if !valid_name(owner_name) {
            return Err(Refusal::new(
                "invalid",
                "Terminal cannot be used with this account name",
            ));
        }
        if !self.building() && !self.image_present().await {
            self.start_build();
        }
        Ok(self.status().await)
    }

    async fn command(&self, owner: i64, owner_name: &str) -> Result<Command, Refusal> {
        let (mut command, container) = self.exec(owner, owner_name, true).await?;
        command
            .args([container.as_str(), "/bin/bash", "-l"])
            .env("TERM", "xterm-256color");
        Ok(command)
    }

    async fn run_command(
        &self,
        owner: i64,
        owner_name: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<Command, Refusal> {
        let (mut command, container) = self.exec(owner, owner_name, false).await?;
        // Killing `docker exec` leaves the command running in the
        // container: `timeout` stops it there.
        command
            .arg(container)
            .args(["timeout", "-k", "5"])
            .arg(timeout.as_secs().max(1).to_string())
            .args(["/bin/bash", "-l", "-c"])
            .arg(script);
        Ok(command)
    }

    fn shows_program(&self) -> bool {
        false
    }

    async fn forget(&self, owner: i64) {
        self.remove(owner).await;
    }
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// `docker build` with the recipe on stdin (no build context).
async fn build_image(options: &WorkerOptions, image: &str) -> Result<(), String> {
    let mut command = docker_command(options);
    command
        .args([
            "build",
            "--quiet",
            "--label",
            &format!("{LABEL}.image=1"),
            "--tag",
            image,
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(DOCKERFILE.as_bytes())
            .await
            .map_err(|e| format!("cannot send the image recipe to docker: {e}"))?;
    }
    let output = tokio::time::timeout(BUILD_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "building took longer than {} minutes",
                BUILD_TIMEOUT.as_secs() / 60
            )
        })?
        .map_err(|e| format!("docker build failed: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(super::truncate_message(
        &format!("docker build failed: {}", tail.join(" | ")),
        600,
    ))
}

/// Earlier sandbox images (a changed recipe); Docker keeps any still used.
async fn remove_old_images(options: &WorkerOptions, current: &str) {
    let listed = run_docker(
        options,
        args(&[
            "image",
            "ls",
            IMAGE_REPO,
            "--format",
            "{{.Repository}}:{{.Tag}}",
        ]),
        None,
        QUICK,
        64 * 1024,
    )
    .await;
    let Ok(listed) = listed else { return };
    for image in listed.stdout.lines().map(str::trim) {
        if !image.is_empty() && image != current && image.starts_with(&format!("{IMAGE_REPO}:")) {
            let _ = run_docker(options, args(&["image", "rm", image]), None, QUICK, 4096).await;
        }
    }
}

/// Remove containers that have had no shell for [`IDLE_REMOVE`].
async fn reap(helper: Arc<Helper>, launcher: Arc<SandboxLauncher>) {
    let mut idle_since: HashMap<i64, Instant> = HashMap::new();
    let mut ticker = tokio::time::interval(REAP_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let busy = helper.owners();
        let containers = launcher.containers().await;
        idle_since.retain(|owner, _| containers.contains(owner));
        for owner in containers {
            if busy.contains(&owner) {
                idle_since.remove(&owner);
                continue;
            }
            let since = *idle_since.entry(owner).or_insert_with(Instant::now);
            if since.elapsed() >= IDLE_REMOVE {
                launcher.remove(owner).await;
                idle_since.remove(&owner);
            }
        }
    }
}

/// The worker's sandbox shells, ready to serve.
pub(super) struct Shells {
    helper: Arc<Helper>,
    launcher: Arc<SandboxLauncher>,
    listener: UnixListener,
    socket: PathBuf,
    allowed_uid: u32,
}

/// Bind the sandbox socket next to the worker's.
pub(super) fn start(cfg: &Config, options: &Arc<WorkerOptions>) -> std::io::Result<Shells> {
    let socket = socket_path(&options.socket_path);
    prepare_socket_path(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    apply_socket_permissions(&socket)?;
    let launcher = Arc::new(SandboxLauncher::new(cfg, options.clone()));
    let helper = Helper::new(launcher.clone(), LIMITS);
    // SAFETY: geteuid never fails.
    let allowed_uid = options
        .allowed_peer_uid
        .unwrap_or_else(|| unsafe { libc::geteuid() });
    tracing::info!(socket = %socket.display(), image = %launcher.image, "Terminal sandboxes listening");
    Ok(Shells {
        helper,
        launcher,
        listener,
        socket,
        allowed_uid,
    })
}

impl Shells {
    /// Serve until `shutdown`; open shells end then (containers stay until
    /// they have been idle for a while).
    pub(super) async fn serve(self, shutdown: impl std::future::Future<Output = ()>) {
        let reaper = tokio::spawn(reap(self.helper.clone(), self.launcher.clone()));
        helper::serve(self.helper, self.listener, self.allowed_uid, shutdown).await;
        reaper.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launcher(dir: &Path) -> SandboxLauncher {
        let cfg = crate::storage::tests::test_config(dir);
        std::fs::create_dir_all(cfg.homes_dir()).unwrap();
        let mut options = WorkerOptions::from_config(&cfg);
        options.socket_path = dir.join("run/worker.sock");
        SandboxLauncher::new(&cfg, Arc::new(options))
    }

    #[test]
    fn only_real_home_folders_of_ordinary_accounts_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let sandboxes = launcher(&dir);
        let homes = sandboxes.homes_dir.clone();
        std::fs::create_dir(homes.join("alice")).unwrap();

        // SAFETY: getuid/getgid never fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        if uid != 0 {
            let home = sandboxes.home("alice").unwrap();
            assert_eq!(home.path, homes.canonicalize().unwrap().join("alice"));
            assert_eq!((home.uid, home.gid), (uid, gid));
        }
        // Missing, a file, a symlink, a bad name: refused.
        assert!(sandboxes.home("bob").is_err());
        std::fs::write(homes.join("carol"), b"").unwrap();
        assert!(sandboxes.home("carol").is_err());
        std::os::unix::fs::symlink("/etc", homes.join("dave")).unwrap();
        assert!(sandboxes.home("dave").is_err());
        for bad in ["../alice", "Alice", "a,b", ""] {
            assert_eq!(
                sandboxes.home(bad).err().map(|r| r.code),
                Some("invalid"),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_account_gets_a_name_inside_its_container() {
        let dir = tempfile::tempdir().unwrap();
        let sandboxes = launcher(dir.path());
        let home = Home {
            name: "alice".into(),
            path: dir.path().join("homes/alice"),
            uid: 993,
            gid: 980,
        };
        let identity = sandboxes.write_identity(7, &home).unwrap();
        assert!(identity.ends_with("run/shells/7"));
        let passwd = std::fs::read_to_string(identity.join("passwd")).unwrap();
        assert!(passwd.starts_with("root:x:0:0:"));
        assert!(passwd.contains("\nalice:x:993:980:alice:/home/alice:/bin/bash\n"));
        let group = std::fs::read_to_string(identity.join("group")).unwrap();
        assert!(group.ends_with("alice:x:980:\n"));
        let mode = std::fs::metadata(identity.join("passwd"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }
}
