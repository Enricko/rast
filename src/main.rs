//! RAST command-line interface and server entry point.
//!
//! Commands: `serve`, `status`, `doctor`, `user create-admin`, `setup-token`,
//! `backup`, `update`, `version`, `worker` (internal), `restore`.

use std::{
    io::{IsTerminal, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};
use serde_json::json;

use rast::{backup, config::Config, db, error::AppResult, state::AppState, users, VERSION};

#[derive(Parser, Debug)]
#[command(
    name = "rast",
    version,
    about = "RAST — Resource Administration & Storage Toolkit",
    long_about = "RAST is a self-hosted server administration toolkit with a desktop-like web UI: \
file station, application deployment, resource monitoring, control panel, backups and audit logs."
)]
struct Cli {
    /// Path to the configuration file (default: /etc/rast/rast.toml, env RAST_CONFIG)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace (env RAST_LOG_LEVEL)
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the HTTP server (default)
    Serve {
        /// Override listen address, e.g. 127.0.0.1:8720
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
    },
    /// Show service and health status
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Diagnose configuration, permissions, database and dependencies
    Doctor,
    /// Manage user accounts
    User {
        #[command(subcommand)]
        cmd: UserCommand,
    },
    /// Generate a one-shot setup token for the web setup page
    SetupToken,
    /// Create a backup archive
    Backup {
        /// Output directory (default: <data_dir>/backups)
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Comma separated scopes: db,config,users,apps (default: db,config)
        #[arg(long, value_name = "SCOPES", default_value = "db,config")]
        scope: String,
        /// Free-form note stored with the backup
        #[arg(long, value_name = "TEXT", default_value = "")]
        note: String,
    },
    /// Check for or install a new release
    Update {
        /// Target version, e.g. 1.2.3 (default: latest)
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Only check for a newer release
        #[arg(long)]
        check: bool,
    },
    /// Print version information
    Version,
    /// Package Center administration
    Packages {
        #[command(subcommand)]
        cmd: PackagesCommand,
    },
    /// Internal: privileged administrative worker (Docker operations)
    #[command(hide = true)]
    Worker,
    /// Restore a database from a backup archive (offline operation)
    Restore {
        /// Backup archive produced by `rast backup`
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
        /// Verify the archive without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PackagesCommand {
    /// Package Center defaults on every login shell's PATH
    Path {
        #[command(subcommand)]
        cmd: PathCommand,
    },
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum PathCommand {
    /// Write or remove /etc/profile.d/rast-packages.sh to match
    /// packages_shell_path, set the runtime permissions and, with
    /// packages_system_wide, install the watcher that links the defaults
    /// into /usr/local/bin (needs root)
    Sync,
    /// Show whether login shells, sudo and services use the Package Center
    /// defaults
    Status,
    /// Link the defaults into /usr/local/bin now (run as root by the
    /// rast-packages-links watcher after every change)
    Links,
}

#[derive(Subcommand, Debug)]
enum UserCommand {
    /// Create the first administrator account
    CreateAdmin {
        /// Login name
        username: String,
        /// Read the password from stdin instead of prompting
        #[arg(long)]
        password_stdin: bool,
        /// Password supplied directly (prefer --password-stdin; visible in process list)
        #[arg(long, value_name = "PASSWORD", hide = true)]
        password: Option<String>,
        /// Display name shown in the UI
        #[arg(long, value_name = "NAME")]
        display_name: Option<String>,
        /// Storage quota in bytes (0 = unlimited)
        #[arg(long, value_name = "BYTES")]
        quota_bytes: Option<i64>,
    },
    /// List accounts
    List,
    /// Reset an account password
    ResetPassword {
        username: String,
        #[arg(long)]
        password_stdin: bool,
        #[arg(long, value_name = "PASSWORD", hide = true)]
        password: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve { bind: None });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads())
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rast: cannot start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli.config, cli.log_level, command));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rast: {e}");
            ExitCode::FAILURE
        }
    }
}

fn worker_threads() -> usize {
    // Keep idle RSS low: 2 workers are enough for the expected request rate.
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 4))
        .unwrap_or(2)
}

async fn run(
    config_path: Option<PathBuf>,
    log_level: Option<String>,
    command: Command,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match command {
        Command::Serve { bind } => serve(config_path, log_level, bind).await,
        Command::Version => {
            // Render once and tolerate a closed stdout: release pipelines and
            // shell recipes routinely run `rast version | head -n 1`, which is
            // a broken pipe for the remaining lines. A closed pipe must never
            // turn a successful version query into a panic/exit code 134.
            use std::io::Write;
            let banner = format!(
                "{} {} ({})\n{}\nhttps://github.com/Enricko/rast\n",
                rast::NAME,
                VERSION,
                env!("RAST_GIT_SHA"),
                rast::DESCRIPTION
            );
            let _ = std::io::stdout().write_all(banner.as_bytes());
            Ok(ExitCode::SUCCESS)
        }
        other => {
            let mut cfg = Config::load(config_path.as_deref())?;
            if let Some(level) = log_level {
                cfg.log_level = level;
            }
            init_tracing(&cfg.log_level);
            match other {
                Command::Status { json } => status(cfg, json).await,
                Command::Doctor => doctor(cfg).await,
                Command::User { cmd } => user_command(cfg, cmd).await,
                Command::SetupToken => {
                    let pool = db::init(&cfg).await?;
                    let token = rast::api::auth::issue_setup_token(&pool).await?;
                    println!("Setup token (valid for 60 minutes, single use):\n\n  {token}\n");
                    println!(
                        "Open {} and paste it into the first-run setup form.",
                        cfg.public_url
                            .clone()
                            .unwrap_or_else(|| format!("http://{}", cfg.bind))
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Command::Backup { out, scope, note } => {
                    let scopes: Vec<String> = scope
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let pool = db::init(&cfg).await?;
                    eprintln!("creating backup (scopes: {})...", scopes.join(","));
                    let progress: rast::backup::ProgressFn = {
                        let last =
                            std::sync::Arc::new(std::sync::Mutex::new((0u64, String::new())));
                        std::sync::Arc::new(move |done, total, message| {
                            let mut state = last.lock().unwrap_or_else(|e| e.into_inner());
                            if state.0 != done || state.1 != message {
                                state.0 = done;
                                state.1 = message.clone();
                                eprintln!("  [{done:>3}/{total}] {message}");
                            }
                        })
                    };
                    let meta = backup::create(
                        &cfg,
                        Some(&pool),
                        &scopes,
                        &note,
                        out.as_deref(),
                        Some(progress),
                    )
                    .await?;
                    println!("{}", serde_json::to_string_pretty(&meta)?);
                    Ok(ExitCode::SUCCESS)
                }
                Command::Restore { from, dry_run, yes } => restore(cfg, from, dry_run, yes).await,
                Command::Update { version, check } => {
                    rast::update::run(cfg, version.as_deref(), check).await
                }
                Command::Packages {
                    cmd: PackagesCommand::Path { cmd },
                } => packages_path(&cfg, cmd),
                Command::Worker => rast::worker::run(cfg).await,
                Command::Serve { .. } | Command::Version => unreachable!(),
            }
        }
    }
}

fn packages_path(cfg: &Config, cmd: PathCommand) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use rast::packages::{install::Layout, links, shell};
    use std::path::Path;
    let layout = Layout::new(cfg.packages_dir());
    let bin_dir = layout.bin_dir();
    let profile = Path::new(shell::PROFILE_SCRIPT);
    let system_bin = Path::new(links::SYSTEM_BIN);
    let unit_dir = Path::new(links::UNIT_DIR);
    let links_on = cfg.packages_links_enabled();
    if cmd != PathCommand::Status {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(format!(
                "changing {} and {} needs root: run `{}`",
                shell::PROFILE_SCRIPT,
                links::SYSTEM_BIN,
                shell::SYNC_COMMAND
            )
            .into());
        }
    }
    if cmd == PathCommand::Links {
        // Shims can change while this runs (an install activating several
        // commands): repeat until a pass changes nothing.
        for _ in 0..5 {
            let done = links::sync(links_on, &bin_dir, system_bin)?;
            for line in &done {
                println!("{line}");
            }
            if done.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if cmd == PathCommand::Sync {
        for line in shell::sync(cfg.packages_shell_path, &cfg.data_dir, &layout, profile)? {
            println!("{line}");
        }
        let config = cfg
            .loaded_from
            .as_deref()
            .filter(|p| *p != Path::new(rast::config::DEFAULT_CONFIG_PATH));
        let units = links::units(rast::update::BIN_PATH, &bin_dir, &cfg.data_dir, config);
        let systemctl = |args: &[&str]| -> Result<(), String> {
            let out = std::process::Command::new("systemctl")
                .args(args)
                .output()
                .map_err(|e| format!("cannot run systemctl: {e}"))?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "systemctl {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        };
        // Links first when turning off (the watcher would only redo them),
        // the watcher first when turning on.
        if links_on {
            for line in links::sync_units(true, unit_dir, &units, &systemctl)? {
                println!("{line}");
            }
        }
        for line in links::sync(links_on, &bin_dir, system_bin)? {
            println!("{line}");
        }
        if !links_on {
            for line in links::sync_units(false, unit_dir, &units, &systemctl)? {
                println!("{line}");
            }
        }
    }
    let st = shell::status(cfg.packages_shell_path, &bin_dir, profile);
    let state = if st.active {
        "on: login shells run the Package Center defaults first"
    } else if !st.enabled {
        "off (packages_shell_path = false)"
    } else if st.installed {
        "outdated: the profile script names another directory"
    } else {
        "not set up yet"
    };
    println!("shell PATH: {state}");
    println!("  profile script: {}", st.profile);
    println!("  bin directory:  {}", st.bin_dir);
    let sys = links::status(links_on, &bin_dir, system_bin, unit_dir);
    let system = if !sys.enabled {
        "off (packages_system_wide = false)".to_string()
    } else if !sys.watcher {
        "not set up yet".to_string()
    } else if !sys.pending.is_empty() {
        format!("pending: {}", sys.pending.join(", "))
    } else if sys.linked.is_empty() {
        "on: nothing to link yet (no official defaults)".to_string()
    } else {
        "on: sudo, services and scripts run the Package Center defaults".to_string()
    };
    println!("system-wide: {system}");
    if !sys.linked.is_empty() {
        println!("  linked in {}: {}", sys.directory, sys.linked.join(", "));
    }
    if !sys.displaced.is_empty() {
        println!(
            "  moved aside ({}<name>): {}",
            links::DISPLACED_PREFIX,
            sys.displaced.join(", ")
        );
    }
    if (st.enabled && !st.active) || (sys.enabled && !sys.watcher) {
        println!("  run: {}", st.sync_command);
    }
    Ok(ExitCode::SUCCESS)
}

fn init_tracing(level: &str) {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| level.to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_new(&filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn serve(
    config_path: Option<PathBuf>,
    log_level: Option<String>,
    bind: Option<String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut cfg = Config::load(config_path.as_deref())?;
    if let Some(level) = log_level {
        cfg.log_level = level;
    }
    if let Some(bind) = bind {
        cfg.bind = bind
            .parse()
            .map_err(|e| format!("invalid --bind address {bind:?}: {e}"))?;
    }
    init_tracing(&cfg.log_level);

    tracing::info!(
        version = VERSION,
        bind = %cfg.bind,
        data_dir = %cfg.data_dir.display(),
        storage_dir = %cfg.storage_dir.display(),
        "starting RAST"
    );
    for warning in rast::config::warnings(&cfg) {
        tracing::warn!("{warning}");
    }

    // One server per data directory; also lets `rast restore` detect a live
    // server regardless of how it was started.
    cfg.ensure_dirs()
        .map_err(|e| format!("cannot create data directories: {e}"))?;
    let _instance = db::InstanceLock::try_acquire(&cfg)
        .map_err(|e| format!("cannot open the instance lock: {e}"))?
        .ok_or_else(|| {
            format!(
                "another RAST server is already running on data directory {} (lock {})",
                cfg.data_dir.display(),
                db::InstanceLock::path(&cfg).display()
            )
        })?;

    let state = AppState::new(cfg.clone()).await?;
    let app = rast::api::router(state.clone());

    let listener = tokio::net::TcpListener::bind(cfg.bind).await.map_err(|e| {
        format!(
            "cannot listen on {}: {e} (is another RAST instance running?)",
            cfg.bind
        )
    })?;
    tracing::info!("listening on http://{}", cfg.bind);
    println!("RAST {VERSION} listening on http://{}", cfg.bind);

    let events = state.events.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        // Graceful shutdown waits for open connections; SSE streams never end
        // on their own, so tell them to finish (clients reconnect + resync).
        tracing::info!("shutdown requested; closing live event streams");
        events.begin_shutdown();
    })
    .await?;

    tracing::info!("shutting down; checkpointing database");
    let _ = db::checkpoint(&state.db).await;
    Ok(ExitCode::SUCCESS)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn status(cfg: Config, as_json: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let addr = cfg.bind;
    let health = probe_health(addr).await;
    let service = service_state();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "product": rast::NAME,
                "version": VERSION,
                "gitSha": env!("RAST_GIT_SHA"),
                "bind": addr.to_string(),
                "publicUrl": cfg.public_url,
                "configFile": cfg.loaded_from.as_ref().map(|p| p.display().to_string()),
                "dataDir": cfg.data_dir.display().to_string(),
                "storageDir": cfg.storage_dir.display().to_string(),
                "database": cfg.db_path().display().to_string(),
                "service": service,
                "health": health,
            }))?
        );
    } else {
        println!("{} {} ({})", rast::NAME, VERSION, env!("RAST_GIT_SHA"));
        println!("bind:        {addr}");
        if let Some(url) = &cfg.public_url {
            println!("public url:  {url}");
        }
        println!("data dir:    {}", cfg.data_dir.display());
        println!("storage dir: {}", cfg.storage_dir.display());
        println!("database:    {}", cfg.db_path().display());
        println!("service:     {service}");
        match health {
            Some(h) => println!("health:      ok ({h})"),
            None => println!("health:      unreachable"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn service_state() -> String {
    std::process::Command::new("systemctl")
        .args(["is-active", "rast.service"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "not managed by systemd".to_string())
}

async fn probe_health(addr: SocketAddr) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = if addr.ip().is_unspecified() {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    } else {
        addr
    };
    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(target),
    )
    .await
    .ok()?
    .ok()?;
    let mut stream = stream;
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut buf))
        .await
        .ok()?;
    if buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200") {
        Some("GET /healthz 200".to_string())
    } else {
        None
    }
}

async fn doctor(cfg: Config) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut failures = 0;
    let mut warnings = 0;
    println!("RAST doctor — {} {}\n", rast::NAME, VERSION);

    println!("[{:4}] configuration file", "INFO");
    match &cfg.loaded_from {
        Some(p) => println!("        loaded {}", p.display()),
        None => println!("        defaults (no config file found)"),
    }
    for warning in rast::config::warnings(&cfg) {
        warnings += 1;
        println!("[WARN] {warning}");
    }

    // Directories
    print!("[    ] data directory writable ... ");
    match std::fs::create_dir_all(&cfg.data_dir).and_then(|_| {
        let probe = cfg.data_dir.join(".doctor-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)
    }) {
        Ok(()) => println!("ok ({})", cfg.data_dir.display()),
        Err(e) => {
            failures += 1;
            println!("FAIL: {e}");
        }
    }
    print!("[    ] storage directory writable ... ");
    match std::fs::create_dir_all(&cfg.storage_dir).and_then(|_| {
        let probe = cfg.storage_dir.join(".doctor-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)
    }) {
        Ok(()) => println!("ok ({})", cfg.storage_dir.display()),
        Err(e) => {
            failures += 1;
            println!("FAIL: {e}");
        }
    }

    // Database
    print!("[    ] database ... ");
    match db::init(&cfg).await {
        Ok(pool) => {
            let version = db::schema_version(&pool).await;
            println!("ok (schema v{version}, {})", cfg.db_path().display());
            let snapshot = sqlx::query("PRAGMA journal_mode")
                .fetch_one(&pool)
                .await
                .ok();
            if let Some(row) = snapshot {
                use sqlx::Row;
                let mode: String = row.get(0);
                if !mode.eq_ignore_ascii_case("wal") {
                    warnings += 1;
                    println!("[WARN] database journal mode is {mode}, expected wal");
                }
            }
        }
        Err(e) => {
            failures += 1;
            println!("FAIL: {e}");
        }
    }

    // Bind availability. If RAST itself is running, the address being in use is
    // the expected state rather than a failure.
    print!("[    ] listen address ... ");
    match tokio::net::TcpListener::bind(cfg.bind).await {
        Ok(listener) => {
            drop(listener);
            println!("available ({})", cfg.bind);
        }
        Err(e) => {
            if service_state() == "active" {
                println!("in use by rast.service ({})", cfg.bind);
            } else {
                failures += 1;
                println!("FAIL: {} ({e})", cfg.bind);
            }
        }
    }

    // Docker (optional). The web service intentionally has no Docker socket
    // access: administration is delegated to rast-worker.service, which owns the
    // socket. Check the worker rather than the (unprivileged) CLI.
    print!("[    ] docker worker ... ");
    if cfg.docker_enabled {
        let worker_unit = std::process::Command::new("systemctl")
            .args(["is-active", "rast-worker.service"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let socket_ok = cfg.worker_socket.exists();
        if worker_unit == "active" && socket_ok {
            println!("ok (rast-worker.service + {})", cfg.worker_socket.display());
        } else if socket_ok {
            warnings += 1;
            println!(
                "WARN: socket {} exists but rast-worker.service is {}",
                cfg.worker_socket.display(),
                if worker_unit.is_empty() {
                    "not installed"
                } else {
                    worker_unit.as_str()
                }
            );
        } else {
            warnings += 1;
            println!(
                "WARN: docker_enabled=true but worker socket {} is missing (start rast-worker.service)",
                cfg.worker_socket.display()
            );
        }
    } else {
        println!("disabled (optional)");
    }

    print!("[    ] time drift (system clock) ... ");
    println!(
        "{}",
        if db::now_ms() > 1_600_000_000_000 {
            "ok"
        } else {
            "FAIL: clock looks wrong"
        }
    );

    // Reverse proxy hints
    if cfg.public_url.is_some() && cfg.trusted_proxies.is_empty() {
        warnings += 1;
        println!(
            "[WARN] public_url is set but no trusted_proxies; client IPs will be the proxy address"
        );
    }

    println!();
    if failures > 0 {
        println!("doctor: {failures} failure(s), {warnings} warning(s)");
        Ok(ExitCode::FAILURE)
    } else {
        println!("doctor: ok, {warnings} warning(s)");
        Ok(ExitCode::SUCCESS)
    }
}

async fn user_command(
    cfg: Config,
    cmd: UserCommand,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let pool = db::init(&cfg).await?;
    match cmd {
        UserCommand::CreateAdmin {
            username,
            password_stdin,
            password,
            display_name,
            quota_bytes,
        } => {
            if db::has_admin(&pool).await {
                eprintln!(
                    "rast: an administrator already exists; use the web UI to add users \
                     (or `rast user reset-password`)"
                );
                return Ok(ExitCode::FAILURE);
            }
            let pw = read_password(password, password_stdin)?;
            let user = users::create(
                &pool,
                &cfg,
                &username,
                &pw,
                rast::auth::Role::Admin,
                display_name.as_deref(),
                quota_bytes,
            )
            .await?;
            rast::audit::record(
                &pool,
                rast::audit::AuditLog::new("user.create_admin")
                    .anonymous(&user.username)
                    .object("user", user.id.to_string())
                    .path(&user.username)
                    .success()
                    .detail("created via CLI"),
            )
            .await;
            println!(
                "Administrator '{}' created. Sign in at {}",
                user.username,
                cfg.public_url
                    .clone()
                    .unwrap_or_else(|| format!("http://{}", cfg.bind))
            );
            Ok(ExitCode::SUCCESS)
        }
        UserCommand::List => {
            let (rows, total) = users::list(&pool, None, 1, 100).await?;
            println!(
                "{:<4} {:<24} {:<8} {:<7} QUOTA",
                "ID", "USERNAME", "ROLE", "ACTIVE"
            );
            for u in rows {
                println!(
                    "{:<4} {:<24} {:<8} {:<7} {}",
                    u.id,
                    u.username,
                    u.role,
                    if u.active { "yes" } else { "no" },
                    u.quota_bytes
                );
            }
            println!("\n{total} account(s)");
            Ok(ExitCode::SUCCESS)
        }
        UserCommand::ResetPassword {
            username,
            password_stdin,
            password,
        } => {
            let user = users::find_by_username(&pool, &username)
                .await?
                .ok_or_else(|| format!("user {username:?} not found"))?;
            let pw = read_password(password, password_stdin)?;
            users::set_password(&pool, user.id, &pw, false, None).await?;
            rast::audit::record(
                &pool,
                rast::audit::AuditLog::new("user.password_reset_cli")
                    .anonymous(&user.username)
                    .object("user", user.id.to_string())
                    .success()
                    .detail("reset via CLI"),
            )
            .await;
            println!(
                "Password updated for '{}'. All sessions revoked.",
                user.username
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn read_password(
    direct: Option<String>,
    from_stdin: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(pw) = direct {
        return Ok(pw);
    }
    if let Ok(pw) = std::env::var("RAST_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    if from_stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        let pw = buf.trim_end_matches(['\r', '\n']).to_string();
        if pw.is_empty() {
            return Err("empty password supplied on stdin".into());
        }
        return Ok(pw);
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Err("no terminal available: pass --password-stdin or set RAST_PASSWORD".into());
    }
    eprint!("Password: ");
    std::io::stderr().flush()?;
    let mut pw = String::new();
    std::io::stdin().read_line(&mut pw)?;
    let pw = pw.trim_end_matches(['\r', '\n']).to_string();
    if pw.is_empty() {
        return Err("password cannot be empty".into());
    }
    Ok(pw)
}

async fn restore(
    cfg: Config,
    from: PathBuf,
    dry_run: bool,
    yes: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !from.exists() {
        return Err(format!("backup archive {} not found", from.display()).into());
    }
    let info = backup::verify(&from).await?;
    println!("{}", serde_json::to_string_pretty(&info)?);
    if dry_run {
        println!("\ndry run: no changes made");
        return Ok(ExitCode::SUCCESS);
    }
    if !yes {
        eprintln!(
            "\nRestoring replaces the live database. Stop rast.service first, then re-run with --yes."
        );
        return Ok(ExitCode::FAILURE);
    }
    let restored = backup::restore_database(&cfg, &from).await?;
    println!("{}", serde_json::to_string_pretty(&restored)?);
    Ok(ExitCode::SUCCESS)
}

#[allow(dead_code)]
fn _assert_traits() {
    // Compile-time check that Config is Send + Sync for tokio::spawn.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Config>();
    assert_send_sync::<AppResult<()>>();
}
