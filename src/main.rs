//! RAST command-line interface and server entry point.
//!
//! Commands: `serve`, `status`, `doctor`, `user create-admin`, `setup-token`,
//! `backup`, `update`, `version`, `package` (authoring), `worker` (internal),
//! `native-helper` (internal), `restore`.

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
    /// App Center administration
    Packages {
        #[command(subcommand)]
        cmd: PackagesCommand,
    },
    /// Author or verify a package repository release (no server needed)
    Package {
        #[command(subcommand)]
        cmd: PackageCommand,
    },
    /// Discover, verify, assemble, sign and check the official catalog
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCommand,
    },
    /// Internal: privileged administrative worker (Docker operations)
    #[command(hide = true)]
    Worker,
    /// Internal: privileged native Linux app helper (Xpra sessions, host APT)
    #[command(hide = true)]
    NativeHelper {
        /// Validate the environment and exit 0/1 without binding the socket
        #[arg(long)]
        check: bool,
    },
    /// Internal: per-user terminal agent (PTY shell, runs as the mapped identity)
    #[command(hide = true)]
    TerminalAgent {
        /// Unix socket the RAST server connects to.
        #[arg(long)]
        socket: PathBuf,
        /// One-time token file written by the helper (deleted after use).
        #[arg(long = "token-file")]
        token_file: PathBuf,
        /// Approved shell to execute.
        #[arg(long)]
        shell: PathBuf,
        /// Working directory for the shell.
        #[arg(long)]
        cwd: PathBuf,
    },
    /// Internal: root file agent (File Station's root file system)
    #[command(hide = true)]
    RootfsAgent {
        /// Unix socket the RAST server connects to.
        #[arg(long)]
        socket: PathBuf,
        /// Token file written by the helper.
        #[arg(long = "token-file")]
        token_file: PathBuf,
        /// Exit after this many seconds without a request.
        #[arg(long = "idle-seconds", default_value_t = 900)]
        idle_seconds: u64,
    },
    /// Internal: per-user file agent (File Station for one HOME, ADR 0005)
    #[command(hide = true)]
    FileAgent {
        /// The HOME to serve (`/` in File Station).
        #[arg(long)]
        home: PathBuf,
        /// Root of the shared folders (`~/Shared/<name>` must point there).
        #[arg(long = "shared-root")]
        shared_root: PathBuf,
        /// Only this uid may connect (the RAST service).
        #[arg(long = "peer-uid")]
        peer_uid: Option<u32>,
        /// Bind this socket instead of the one systemd passes (development).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Token file (default: the systemd credential `token`).
        #[arg(long = "token-file")]
        token_file: Option<PathBuf>,
        /// Exit after this many seconds without a request.
        #[arg(long = "idle-seconds", default_value_t = 900)]
        idle_seconds: u64,
    },
    /// Move existing data to the one-HOME layout (ADR 0005)
    Migrate {
        #[command(subcommand)]
        cmd: MigrateCommand,
    },
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
enum PackageCommand {
    /// Validate rast-package.json (optionally against local archives)
    Validate {
        /// Manifest to check (default: rast-package.json)
        manifest: Option<PathBuf>,
        /// Directory that holds the release archives
        #[arg(long, value_name = "DIR")]
        dist: Option<PathBuf>,
    },
    /// Embed an SVG/PNG icon into a package manifest (default rast-package.json)
    EmbedIcon {
        /// SVG or PNG icon to embed (max 64 KiB)
        image: PathBuf,
        /// Manifest to update (default: rast-package.json)
        #[arg(long, value_name = "FILE")]
        manifest: Option<PathBuf>,
    },
    /// Write rast-package.json for a release tag from archives in dist/
    Manifest {
        /// Repository package.json with the manifest template
        #[arg(long, value_name = "FILE", default_value = "package.json")]
        package: PathBuf,
        /// Release tag, e.g. v5.4.9-1
        #[arg(long, value_name = "TAG")]
        tag: String,
        /// GitHub repository, owner/name
        #[arg(long, value_name = "OWNER/NAME")]
        repo: String,
        /// Directory with the built archives (default: dist)
        #[arg(long, value_name = "DIR", default_value = "dist")]
        dist: PathBuf,
        /// Attestation URL per target, as TARGET=URL (repeatable)
        #[arg(long, value_name = "TARGET=URL")]
        attestation: Vec<String>,
        /// Skip targets whose archive has not been built
        #[arg(long)]
        allow_missing: bool,
        /// Output file (default: rast-package.json)
        #[arg(long, value_name = "FILE", default_value = "rast-package.json")]
        out: PathBuf,
    },
    /// Extract an archive like the installer and run the manifest checks
    Smoke {
        /// Built archive to test
        archive: PathBuf,
        /// Repository package.json with the manifest template
        #[arg(long, value_name = "FILE", default_value = "package.json")]
        package: PathBuf,
    },
    /// Cross-check a published GitHub release like Install from GitHub does
    CheckRelease {
        /// GitHub repository, owner/name
        #[arg(long, value_name = "OWNER/NAME")]
        repo: String,
        /// Release tag, e.g. v5.4.9-1
        #[arg(long, value_name = "TAG")]
        tag: String,
    },
}

#[derive(Subcommand, Debug)]
enum CatalogCommand {
    /// Fetch and verify the published catalog (absent on the first run)
    Previous {
        #[arg(long, value_name = "DIR", default_value = "work/previous")]
        out: PathBuf,
    },
    /// Discover upstream versions and write the catalog plan
    Discover {
        #[arg(long, value_name = "DIR", default_value = "work/previous")]
        previous: PathBuf,
        #[arg(long, value_name = "FILE", default_value = "work/plan.json")]
        out: PathBuf,
        /// Versions to withhold, comma separated (e.g. go@1.26.11,nodejs@24.22.2)
        #[arg(long, value_name = "LIST", default_value = "")]
        withhold: String,
        /// Re-verify versions that are already in the catalog
        #[arg(long)]
        force: bool,
        #[arg(long, value_name = "FILE")]
        github_output: Option<PathBuf>,
    },
    /// Publish-ready catalog that lists new versions as detected
    Interim {
        #[arg(long, value_name = "FILE", default_value = "work/plan.json")]
        plan: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "work/previous")]
        previous: PathBuf,
        #[arg(long, value_name = "FILE", default_value = "work/out/catalog.json")]
        out: PathBuf,
    },
    /// Download, verify and smoke-test upstream artifacts for one target
    Verify {
        #[arg(long, value_name = "FILE", default_value = "work/plan.json")]
        plan: PathBuf,
        #[arg(long, value_name = "TARGET")]
        target: String,
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Smoke-test a CI-built PHP archive and record the result
    PhpResult {
        #[arg(long, value_name = "FILE")]
        archive: PathBuf,
        #[arg(long, value_name = "VERSION")]
        version: String,
        #[arg(long, value_name = "TARGET")]
        target: String,
        #[arg(long, value_name = "URL", default_value = "")]
        attestation_url: String,
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Final catalog: carried versions plus newly verified ones
    Assemble {
        #[arg(long, value_name = "FILE", default_value = "work/plan.json")]
        plan: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "work/previous")]
        previous: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "work/results")]
        results: PathBuf,
        /// JSON object of PHP release tags per version, or a file containing it
        #[arg(long, value_name = "JSON|FILE", default_value = "")]
        php_releases: String,
        #[arg(long, value_name = "FILE", default_value = "work/out/catalog.json")]
        out: PathBuf,
    },
    /// Ed25519 signature (catalog.json.sig) for a catalog
    Sign {
        #[arg(value_name = "CATALOG")]
        catalog: PathBuf,
    },
    /// Validate every manifest and verify the signature
    Check {
        #[arg(value_name = "CATALOG")]
        catalog: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "")]
        previous: String,
        #[arg(long, value_name = "new|all", value_parser = ["new", "all"])]
        download: Option<String>,
    },
    /// Print the PHP build inputs for a workflow step
    PhpInputs {
        #[arg(long, value_name = "FILE", default_value = "work/plan.json")]
        plan: PathBuf,
        #[arg(long, value_name = "VERSION")]
        version: String,
        #[arg(long, value_name = "FILE")]
        github_output: Option<PathBuf>,
    },
    /// Publish CI-built PHP archives as releases and report their tags
    PhpPublish {
        #[arg(long, value_name = "DIR")]
        results: PathBuf,
        #[arg(long, value_name = "DIR")]
        archives: PathBuf,
        #[arg(long, value_name = "FILE")]
        github_output: Option<PathBuf>,
    },
    /// Collect downloaded workflow artifacts into a work directory
    Collect {
        #[arg(long, value_name = "DIR")]
        artifacts: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "work")]
        work: PathBuf,
        #[arg(long, value_name = "RESULT", default_value = "")]
        verify_result: String,
    },
    /// Print a catalog's sequence number
    Sequence {
        #[arg(value_name = "CATALOG")]
        catalog: PathBuf,
    },
    /// Build a static PHP CLI for one target with static-php-cli in Docker
    BuildPhp {
        #[arg(long, value_name = "VERSION")]
        version: String,
        #[arg(long, value_name = "TARGET")]
        target: String,
        #[arg(long, value_name = "SHA256")]
        source_sha256: String,
        #[arg(long, value_name = "LIST")]
        extensions: String,
        #[arg(long, value_name = "VERSION")]
        spc_version: String,
        #[arg(long, value_name = "IMAGE")]
        builder: String,
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum PackagesCommand {
    /// App Center defaults on every login shell's PATH
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
    /// Show whether login shells, sudo and services use the App Center
    /// defaults
    Status,
    /// Link the defaults into /usr/local/bin now (run as root by the
    /// rast-packages-links watcher after every change)
    Links,
}

#[derive(Subcommand, Debug)]
enum MigrateCommand {
    /// Shared folders: Linux groups, group-owned containers and ACLs. Run
    /// as the `rast` user, before `rast migrate home`; apply and rollback
    /// need the server stopped.
    #[command(group(clap::ArgGroup::new("selection").required(true).args(["share", "all"])))]
    Shares {
        /// Only this shared folder (by name)
        #[arg(long, value_name = "NAME")]
        share: Option<String>,
        /// Every shared folder
        #[arg(long)]
        all: bool,
        /// Report what would change, change nothing
        #[arg(long, conflicts_with = "rollback")]
        dry_run: bool,
        /// Restore owners, modes and ACLs from the migration journal
        #[arg(long)]
        rollback: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Accounts: move a legacy account into one-HOME mode (its HOME, app
    /// profiles and recycle bin become its Linux identity's). Run as the
    /// `rast` user, after `rast migrate shares`; apply and rollback need the
    /// server stopped.
    #[command(group(clap::ArgGroup::new("selection").required(true).args(["user", "all"])))]
    Home {
        /// Only this account (by RAST username)
        #[arg(long, value_name = "NAME")]
        user: Option<String>,
        /// Every account
        #[arg(long)]
        all: bool,
        /// Report what would change, change nothing
        #[arg(long, conflicts_with = "rollback")]
        dry_run: bool,
        /// Undo the migration from its journal
        #[arg(long)]
        rollback: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
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
        Command::Package { cmd } => package_command(cmd),
        Command::Catalog { cmd } => catalog_command(cmd),
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
        // The per-user file agent runs as the user's Linux account, which
        // cannot read the server configuration; it needs none.
        Command::FileAgent {
            home,
            shared_root,
            peer_uid,
            socket,
            token_file,
            idle_seconds,
        } => {
            init_tracing(log_level.as_deref().unwrap_or("info"));
            // Before the token is read: the user's own processes must not
            // be able to trace the agent and take it over.
            rast::homefs::agent::forbid_tracing()
                .map_err(|error| format!("file agent cannot protect itself: {error}"))?;
            let token = rast::homefs::agent::token_from(token_file.as_deref())?;
            rast::homefs::agent::run(rast::homefs::agent::RunOptions {
                home,
                shared_root,
                listen: match socket {
                    Some(path) => rast::homefs::agent::Listen::Path(path),
                    None => rast::homefs::agent::Listen::Systemd,
                },
                token,
                peer_uid,
                idle: std::time::Duration::from_secs(idle_seconds.max(30)),
                stay_while_user_runs: true,
            })
            .await
            .map(|_| ExitCode::SUCCESS)
            .map_err(|error| -> Box<dyn std::error::Error> {
                format!("file agent failed: {error}").into()
            })
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
                Command::Migrate { cmd } => migrate_command(cfg, cmd).await,
                Command::Update { version, check } => {
                    rast::update::run(cfg, version.as_deref(), check).await
                }
                Command::Packages {
                    cmd: PackagesCommand::Path { cmd },
                } => packages_path(&cfg, cmd),
                Command::Worker => rast::worker::run(cfg).await,
                Command::NativeHelper { check } => rast::native::run(cfg, check).await,
                Command::TerminalAgent {
                    socket,
                    token_file,
                    shell,
                    cwd,
                } => rast::terminal::agent::run(socket, token_file, shell, cwd)
                    .await
                    .map(|_| ExitCode::SUCCESS)
                    .map_err(|error| -> Box<dyn std::error::Error> {
                        format!("terminal agent failed: {error}").into()
                    }),
                Command::RootfsAgent {
                    socket,
                    token_file,
                    idle_seconds,
                } => rast::rootfs::agent::run(
                    socket,
                    token_file,
                    std::time::Duration::from_secs(idle_seconds.max(30)),
                )
                .await
                .map(|_| ExitCode::SUCCESS)
                .map_err(|error| -> Box<dyn std::error::Error> {
                    format!("root file agent failed: {error}").into()
                }),
                Command::Serve { .. }
                | Command::FileAgent { .. }
                | Command::Version
                | Command::Package { .. }
                | Command::Catalog { .. } => unreachable!(),
            }
        }
    }
}

fn package_command(cmd: PackageCommand) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use rast::packages::authoring;
    use std::collections::BTreeMap;
    match cmd {
        PackageCommand::Validate { manifest, dist } => {
            let path = manifest.unwrap_or_else(|| PathBuf::from("rast-package.json"));
            let value = authoring::validate_file(&path, dist.as_deref())?;
            println!(
                "{}: valid ({} {} rev {})",
                path.display(),
                value.id,
                value.version,
                value.revision
            );
        }
        PackageCommand::EmbedIcon { image, manifest } => {
            let path = manifest.unwrap_or_else(|| PathBuf::from("rast-package.json"));
            let package = authoring::embed_icon(&path, &image)?;
            let icon = package.icon.as_ref().expect("embed_icon sets the icon");
            println!(
                "{}: embedded {} icon ({} base64 chars) for {} {}",
                path.display(),
                icon.media_type,
                icon.data.len(),
                package.id,
                package.version
            );
        }
        PackageCommand::Manifest {
            package,
            tag,
            repo,
            dist,
            attestation,
            allow_missing,
            out,
        } => {
            let template = authoring::load_template(&package)?;
            let mut attestations = BTreeMap::new();
            for entry in attestation {
                let (target, url) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("--attestation expects TARGET=URL, got {entry:?}"))?;
                attestations.insert(target.trim().to_string(), url.trim().to_string());
            }
            let run_url = std::env::var("GITHUB_RUN_ID").ok().map(|id| {
                format!(
                    "{}/{}/actions/runs/{id}",
                    std::env::var("GITHUB_SERVER_URL")
                        .unwrap_or_else(|_| "https://github.com".into()),
                    std::env::var("GITHUB_REPOSITORY").unwrap_or_default()
                )
            });
            let manifest = authoring::build_manifest(
                &template,
                &tag,
                &repo,
                &dist,
                &attestations,
                allow_missing,
                run_url.as_deref(),
            )?;
            let bytes = serde_json::to_vec_pretty(&manifest)?;
            std::fs::write(&out, [bytes.as_slice(), b"\n"].concat())?;
            println!(
                "wrote {}: {} {} revision {}, targets {:?}",
                out.display(),
                manifest.id,
                manifest.version,
                manifest.revision,
                manifest.targets()
            );
        }
        PackageCommand::Smoke { archive, package } => {
            let template = authoring::load_template(&package)?;
            for line in authoring::smoke(&archive, &template)? {
                println!("{line}");
            }
        }
        PackageCommand::CheckRelease { repo, tag } => {
            let token = std::env::var("GH_TOKEN")
                .or_else(|_| std::env::var("GITHUB_TOKEN"))
                .ok();
            for line in authoring::check_release(&repo, &tag, token.as_deref())? {
                println!("{line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn catalog_command(cmd: CatalogCommand) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use rast::packages::catalog_cli::{self, Command as PortCommand, DownloadMode};
    let command = match cmd {
        CatalogCommand::Previous { out } => PortCommand::Previous { out },
        CatalogCommand::Discover {
            previous,
            out,
            withhold,
            force,
            github_output,
        } => PortCommand::Discover {
            previous,
            out,
            withhold,
            force,
            github_output,
        },
        CatalogCommand::Interim {
            plan,
            previous,
            out,
        } => PortCommand::Interim {
            plan,
            previous,
            out,
        },
        CatalogCommand::Verify { plan, target, out } => PortCommand::Verify { plan, target, out },
        CatalogCommand::PhpResult {
            archive,
            version,
            target,
            attestation_url,
            out,
        } => PortCommand::PhpResult {
            archive,
            version,
            target,
            attestation_url,
            out,
        },
        CatalogCommand::Assemble {
            plan,
            previous,
            results,
            php_releases,
            out,
        } => PortCommand::Assemble {
            plan,
            previous,
            results,
            php_releases: (!php_releases.is_empty()).then(|| PathBuf::from(php_releases)),
            out,
        },
        CatalogCommand::Sign { catalog } => PortCommand::Sign { catalog },
        CatalogCommand::Check {
            catalog,
            previous,
            download,
        } => PortCommand::Check {
            catalog,
            previous: (!previous.is_empty()).then(|| PathBuf::from(previous)),
            download: match download.as_deref() {
                Some("new") => Some(DownloadMode::New),
                Some("all") => Some(DownloadMode::All),
                _ => None,
            },
        },
        CatalogCommand::PhpInputs {
            plan,
            version,
            github_output,
        } => PortCommand::PhpInputs {
            plan,
            version,
            github_output,
        },
        CatalogCommand::PhpPublish {
            results,
            archives,
            github_output,
        } => PortCommand::PhpPublish {
            results,
            archives,
            github_output,
        },
        CatalogCommand::Collect {
            artifacts,
            work,
            verify_result,
        } => PortCommand::Collect {
            artifacts,
            work,
            verify_result,
        },
        CatalogCommand::Sequence { catalog } => PortCommand::Sequence { catalog },
        CatalogCommand::BuildPhp {
            version,
            target,
            source_sha256,
            extensions,
            spc_version,
            builder,
            out,
        } => PortCommand::BuildPhp {
            version,
            target,
            source_sha256,
            extensions,
            spc_version,
            builder,
            out,
        },
    };
    catalog_cli::run(command).map_err(std::io::Error::other)?;
    Ok(ExitCode::SUCCESS)
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
        "on: login shells run the App Center defaults first"
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
        "on: sudo, services and scripts run the App Center defaults".to_string()
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
    // The database and the key of an older release move into the private
    // folder (ADR 0006) before anything opens them.
    let moved = db::move_into_private(&cfg, &_instance).map_err(|e| {
        format!(
            "cannot move the database and the key into {}: {e}",
            cfg.private_dir().display()
        )
    })?;
    if !moved.is_empty() {
        tracing::info!(
            files = ?moved,
            dir = %cfg.private_dir().display(),
            "moved the database and the key into the private data folder"
        );
    }

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
    let mut user_ids = std::collections::BTreeSet::new();
    let mut share_containers: Vec<(String, String)> = Vec::new();
    match db::init(&cfg).await {
        Ok(pool) => {
            user_ids = sqlx::query_scalar::<_, i64>("SELECT id FROM users")
                .fetch_all(&pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            share_containers = rast::storage::shares::list_records(&pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|record| (record.name, record.container_path))
                .collect();
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

    // The private data folder and hard-link protection (ADR 0006).
    let (lines, private_warnings, private_failures) = db::private_report(
        &cfg,
        std::fs::read_to_string("/proc/sys/fs/protected_hardlinks")
            .ok()
            .as_deref(),
    );
    for line in lines {
        println!("{line}");
    }
    warnings += private_warnings;
    failures += private_failures;

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
            // The helper runs the same binary: after an update it must have
            // been restarted too, or newer Docker actions are refused.
            let status = rast::docker::DockerManager::new(&cfg)
                .refresh_status()
                .await;
            if status["workerOutdated"] == true {
                warnings += 1;
                println!(
                    "WARN: {}",
                    status["reason"]
                        .as_str()
                        .unwrap_or("the Docker helper is outdated")
                );
            } else if status["available"] == true {
                println!(
                    "ok (rast-worker.service {} + {})",
                    status["workerVersion"].as_str().unwrap_or("?"),
                    cfg.worker_socket.display()
                );
            } else {
                println!(
                    "ok (rast-worker.service + {}; version not checked: {})",
                    cfg.worker_socket.display(),
                    status["reason"].as_str().unwrap_or("no answer")
                );
            }
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

    // Native Linux apps (optional; replaces the Docker backend for desktop
    // apps). Warnings only: a missing Xpra or helper never breaks the rest of
    // RAST, and with the native backend disabled there is nothing to report.
    let native_report = rast::native::doctor_report(&cfg).await;
    for line in &native_report.lines {
        println!("{line}");
    }
    warnings += native_report.warnings;
    failures += native_report.failures;
    let (quota_lines, quota_warnings) = rast::quota_sync::doctor_report(&cfg).await;
    for line in &quota_lines {
        println!("{line}");
    }
    warnings += quota_warnings;
    if cfg.native_enabled && !user_ids.is_empty() {
        let identities = rast::native::doctor_identities(&cfg, &user_ids).await;
        for line in &identities.lines {
            println!("{line}");
        }
        warnings += identities.warnings;
    }
    if cfg.native_enabled {
        let (lines, trash_warnings) =
            rast::share_groups::doctor_share_trash(&cfg, &share_containers);
        for line in &lines {
            println!("{line}");
        }
        warnings += trash_warnings;
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

async fn migrate_command(
    cfg: Config,
    cmd: MigrateCommand,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use rast::migrate_shares::{self, Mode};
    let (share, dry_run, rollback, json) = match cmd {
        MigrateCommand::Shares {
            share,
            all: _,
            dry_run,
            rollback,
            json,
        } => (share, dry_run, rollback, json),
        MigrateCommand::Home {
            user,
            all: _,
            dry_run,
            rollback,
            json,
        } => return migrate_home_command(cfg, user, dry_run, rollback, json).await,
    };
    let mode = match (dry_run, rollback) {
        (true, _) => Mode::DryRun,
        (_, true) => Mode::Rollback,
        _ => Mode::Apply,
    };
    if !cfg.native_enabled {
        return Err("shared folder groups need the native backend (native_enabled = true)".into());
    }
    // Apply and rollback change permissions under a running server's feet
    // (D9): they need the server stopped.
    let _instance = if mode == Mode::DryRun {
        None
    } else {
        Some(db::InstanceLock::try_acquire(&cfg)?.ok_or_else(|| {
            format!(
                "the RAST server is running on {}; stop it first (systemctl stop rast)",
                cfg.data_dir.display()
            )
        })?)
    };
    let pool = db::init(&cfg).await?;
    let native = rast::native::client::NativeClient::from_config(&cfg);
    let outcomes = migrate_shares::run(&pool, &native, mode, share.as_deref()).await?;
    let failed = outcomes.iter().filter(|outcome| !outcome.ok).count();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "mode": match mode {
                    Mode::DryRun => "dry-run",
                    Mode::Apply => "apply",
                    Mode::Rollback => "rollback",
                },
                "shares": outcomes,
                "failed": failed,
            }))?
        );
    } else {
        if outcomes.is_empty() {
            println!("There are no shared folders.");
        }
        for outcome in &outcomes {
            for line in migrate_shares::describe(mode, outcome) {
                println!("{line}");
            }
        }
        if failed > 0 {
            eprintln!("rast: {failed} shared folder(s) failed; the others are done");
        }
    }
    Ok(if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn migrate_home_command(
    cfg: Config,
    user: Option<String>,
    dry_run: bool,
    rollback: bool,
    json: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use rast::migrate_home::{self, Env, Mode, Selection};
    let mode = match (dry_run, rollback) {
        (true, _) => Mode::DryRun,
        (_, true) => Mode::Rollback,
        _ => Mode::Apply,
    };
    let selection = match user {
        Some(name) => Selection::User(name),
        None => Selection::All,
    };
    if !cfg.native_enabled {
        return Err("one-HOME mode needs the native backend (native_enabled = true)".into());
    }
    // Apply and rollback change owners under a running server's feet (D9).
    let _instance = if mode == Mode::DryRun {
        None
    } else {
        Some(db::InstanceLock::try_acquire(&cfg)?.ok_or_else(|| {
            format!(
                "the RAST server is running on {}; stop it first (systemctl stop rast)",
                cfg.data_dir.display()
            )
        })?)
    };
    let pool = db::init(&cfg).await?;
    let native = rast::native::client::NativeClient::from_config(&cfg);
    let env = Env {
        cfg: &cfg,
        pool: &pool,
        native: &native,
    };
    let outcomes = migrate_home::run(&env, mode, &selection).await?;
    let failed = outcomes.iter().filter(|outcome| !outcome.ok).count();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "mode": mode.as_str(),
                "users": outcomes,
                "failed": failed,
            }))?
        );
    } else {
        if outcomes.is_empty() {
            println!("There are no accounts.");
        }
        for outcome in &outcomes {
            for line in migrate_home::describe(mode, outcome) {
                println!("{line}");
            }
        }
        if failed > 0 {
            eprintln!("rast: {failed} account(s) failed; the others are done");
        }
    }
    Ok(if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
            // An administrator created on the server itself owns the server.
            users::set_super_admin(&pool, user.id, true).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<MigrateCommand, clap::Error> {
        let mut argv = vec!["rast", "migrate"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv)?;
        match cli.command {
            Some(Command::Migrate { cmd }) => Ok(cmd),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn migrations_name_one_share_or_account_or_all_of_them() {
        // No selection is an error: a run never covers everything by
        // default.
        for args in [
            &["shares"][..],
            &["shares", "--dry-run"],
            &["shares", "--rollback"],
            &["home"],
            &["home", "--dry-run"],
            &["home", "--rollback"],
            &["shares", "--share", "team", "--all"],
            &["home", "--user", "t-alice", "--all"],
            &["home", "--user", "t-alice", "--dry-run", "--rollback"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        for mode in [None, Some("--dry-run"), Some("--rollback")] {
            let with = |args: &[&'static str]| {
                let mut args = args.to_vec();
                args.extend(mode);
                args
            };
            let cmd = parse(&with(&["shares", "--share", "team"])).unwrap();
            assert!(matches!(
                cmd,
                MigrateCommand::Shares { share: Some(ref name), all: false, dry_run, rollback, .. }
                    if name == "team"
                        && dry_run == (mode == Some("--dry-run"))
                        && rollback == (mode == Some("--rollback"))
            ));
            let cmd = parse(&with(&["shares", "--all"])).unwrap();
            assert!(matches!(
                cmd,
                MigrateCommand::Shares {
                    share: None,
                    all: true,
                    ..
                }
            ));
            let cmd = parse(&with(&["home", "--user", "t-alice", "--json"])).unwrap();
            assert!(matches!(
                cmd,
                MigrateCommand::Home { user: Some(ref name), all: false, json: true, dry_run, rollback }
                    if name == "t-alice"
                        && dry_run == (mode == Some("--dry-run"))
                        && rollback == (mode == Some("--rollback"))
            ));
            let cmd = parse(&with(&["home", "--all"])).unwrap();
            assert!(matches!(
                cmd,
                MigrateCommand::Home {
                    user: None,
                    all: true,
                    ..
                }
            ));
        }
    }
}
