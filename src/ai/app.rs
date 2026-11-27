//! AI Agent as its own program: the server of the Package Center app
//! `agent` (binary `rast-ai-agent`). RAST starts it with the app's sockets,
//! token and data folder in its environment (see `crate::apphost`), signs
//! people in, checks CSRF and app access, and forwards `/api/ai/*` here with
//! the account in `X-Rast-User`.
//!
//! Its data lives in `<RAST_APP_DATA>/agent.db` with its own key
//! (`secret.key`). On its first start it takes AI Agent's data from RAST
//! (dump and restore through the host API; RAST keeps its copy), and it
//! forgets the chats of accounts RAST removed.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::net::UnixListener;

use super::host::{Agent, Host, Remote};
use crate::{
    auth::{AuthUser, ClientIp, Role},
    db,
    error::AppError,
    secrets::Secrets,
};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations-agent");

/// AI Agent's own version: its App Center package (`ai-agent`), released by
/// tagging `ai-agent-v<VERSION>` (.github/workflows/ai-agent.yml checks it).
pub const VERSION: &str = "1.2.0";

/// Set once RAST's AI Agent data was taken (Unix ms, or `none`).
const TAKEN_OVER: &str = "agent.takenOverAt";
/// How often removed accounts are looked for.
const TIDY_EVERY: Duration = Duration::from_secs(600);

fn env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is not set (RAST starts this program)"))
}

/// The program's `main`.
pub fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        // RAST adds the time and its own colors when it logs these lines.
        .without_time()
        .with_ansi(false)
        .init();
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("rast-ai-agent {VERSION} (RAST {})", crate::VERSION);
        return ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rast-ai-agent: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let app_socket = PathBuf::from(env("RAST_APP_SOCKET")?);
    let host_socket = PathBuf::from(env("RAST_HOST_SOCKET")?);
    let token = env("RAST_APP_TOKEN")?;
    let data_dir = PathBuf::from(env("RAST_APP_DATA")?);
    std::fs::create_dir_all(&data_dir)
        .and_then(|_| std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("the data folder {} is not usable: {e}", data_dir.display()))?;

    let remote = Remote::connect(host_socket, token.clone()).await?;
    tracing::info!(
        rast = %remote.info.rast_version,
        "AI Agent {VERSION} starting"
    );
    let pool = db::connect_with(&data_dir.join("agent.db"), 8, &MIGRATOR)
        .await
        .map_err(|e| e.to_string())?;
    let secrets = Secrets::load_or_create(&data_dir.join("secret.key"))
        .map_err(|e| format!("the app's key could not be made: {e}"))?;
    let agent = Agent::remote(pool, Arc::new(secrets), data_dir, remote);

    if let Err(error) = take_over(&agent).await {
        // Started again, it tries again; RAST keeps its copy meanwhile.
        return Err(format!(
            "AI Agent's data could not be taken from RAST: {error}"
        ));
    }
    super::run::recover(&agent.db).await;
    super::files::sweep(&agent.db, agent.data_dir()).await;
    super::spawn_price_refresh(agent.db.clone(), agent.secrets.clone());
    let tidy = agent.clone();
    tokio::spawn(async move {
        loop {
            forget_removed_accounts(&tidy).await;
            tokio::time::sleep(TIDY_EVERY).await;
        }
    });

    let router = crate::api::ai::routes()
        .with_state(agent)
        .layer(middleware::from_fn_with_state(Arc::new(token), identify));
    let _ = std::fs::remove_file(&app_socket);
    let listener = UnixListener::bind(&app_socket)
        .map_err(|e| format!("cannot listen on {}: {e}", app_socket.display()))?;
    std::fs::set_permissions(&app_socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    tracing::info!("listening on {}", app_socket.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(stopped())
        .await
        .map_err(|e| e.to_string())
}

async fn stopped() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    id: i64,
    username: String,
    #[serde(default)]
    display_name: String,
    role: String,
}

/// Only RAST talks to this server: its token on each request, and the
/// signed-in account it checked.
async fn identify(State(token): State<Arc<String>>, mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    let from_rast = headers
        .get("x-rast-token")
        .is_some_and(|v| constant_time_eq(v.as_bytes(), token.as_bytes()));
    if !from_rast {
        return AppError::Unauthorized("requests come through RAST").into_response();
    }
    let identity: Option<Identity> = headers
        .get("x-rast-user")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| serde_json::from_str(v).ok());
    let Some(identity) = identity else {
        return AppError::Unauthorized("authentication required").into_response();
    };
    let ip = headers
        .get("x-rast-client-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<IpAddr>().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let user = AuthUser {
        id: identity.id,
        username: identity.username,
        display_name: identity.display_name,
        role: Role::parse(&identity.role).unwrap_or(Role::User),
        session_id: String::new(),
        quota_bytes: 0,
        must_change_password: false,
    };
    request.extensions_mut().insert(user);
    request.extensions_mut().insert(ClientIp(ip));
    next.run(request).await
}

// ---------------------------------------------------------- taking over

/// Columns RAST encrypted with its key: the app encrypts them with its own.
const SEALED: [(&str, &str); 2] = [("ai_providers", "api_key"), ("ai_providers", "sign_in")];

/// Take AI Agent's data from RAST, once.
async fn take_over(agent: &Agent) -> Result<(), String> {
    if db::get_setting(&agent.db, TAKEN_OVER).await.is_some() {
        return Ok(());
    }
    let Host::Remote(remote) = &agent.host else {
        return Ok(());
    };
    let dump = remote.dump().await?;
    if dump["available"] != true {
        tracing::info!("RAST has no AI Agent data to hand over");
        db::set_setting(&agent.db, TAKEN_OVER, "none")
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let mut tables = dump["tables"].as_object().cloned().unwrap_or_default();
    let mut settings = dump["settings"].as_object().cloned().unwrap_or_default();

    // Values sealed with RAST's key, opened by RAST and sealed again here.
    let mut sealed = Vec::new();
    for (table, column) in SEALED {
        for row in tables
            .get(table)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = row[column].as_str().filter(|t| !t.is_empty()) {
                sealed.push(text.to_string());
            }
        }
    }
    let mut search: Option<Value> = settings
        .get("ai.search")
        .and_then(Value::as_str)
        .and_then(|t| serde_json::from_str(t).ok());
    if let Some(key) = search
        .as_ref()
        .and_then(|s| s["key"].as_str())
        .filter(|k| !k.is_empty())
    {
        sealed.push(key.to_string());
    }
    let opened = remote.open_sealed(&sealed).await?;
    let reseal = |text: &str| -> Value {
        match sealed
            .iter()
            .position(|s| s == text)
            .and_then(|i| opened[i].as_str())
        {
            Some(plain) => agent
                .secrets
                .encrypt_str(plain)
                .map(Value::String)
                .unwrap_or_else(|_| json!("")),
            // RAST could not open it either: left empty (signed out, no key).
            None => json!(""),
        }
    };
    for (table, column) in SEALED {
        if let Some(rows) = tables.get_mut(table).and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(text) = row[column].as_str().filter(|t| !t.is_empty()) {
                    let fresh = reseal(text);
                    row[column] = fresh;
                }
            }
        }
    }
    if let Some(search) = search.as_mut() {
        if let Some(key) = search["key"].as_str().filter(|k| !k.is_empty()) {
            let fresh = reseal(key);
            search["key"] = fresh;
        }
        settings.insert("ai.search".into(), Value::String(search.to_string()));
    }

    // Chat files first: a row never points at a missing file.
    let from = dump["filesDir"].as_str().map(PathBuf::from);
    let files = tables
        .get("ai_files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(from) = from {
        let to = super::files::root(agent.data_dir());
        let copied = tokio::task::spawn_blocking(move || copy_files(&from, &to, &files))
            .await
            .map_err(|e| e.to_string())??;
        tracing::info!(files = copied, "chat files copied from RAST");
    }

    let mut tx = agent.db.begin().await.map_err(|e| e.to_string())?;
    let mut counts = Vec::new();
    for table in crate::apphost::migrate::AI_TABLES {
        let rows = tables.get(table).and_then(Value::as_array);
        let Some(rows) = rows else { continue };
        let columns: HashSet<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect();
        for row in rows {
            let Some(object) = row.as_object() else {
                continue;
            };
            let names: Vec<&String> = object.keys().filter(|k| columns.contains(*k)).collect();
            if names.is_empty() {
                continue;
            }
            // Table names are a fixed list; column names are agent.db's own.
            let sql = format!(
                "INSERT INTO \"{table}\" ({}) VALUES ({})",
                names
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                vec!["?"; names.len()].join(", ")
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for name in &names {
                query = match &object[*name] {
                    Value::Null => query.bind(None::<String>),
                    Value::Bool(b) => query.bind(i64::from(*b)),
                    Value::Number(n) => match n.as_i64() {
                        Some(i) => query.bind(i),
                        None => query.bind(n.as_f64()),
                    },
                    Value::String(s) => query.bind(s.clone()),
                    other => query.bind(other.to_string()),
                };
            }
            query
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("{table}: {e}"))?;
        }
        counts.push(format!("{table} {}", rows.len()));
    }
    for (key, value) in &settings {
        if let Some(value) = value.as_str() {
            sqlx::query(
                "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            )
            .bind(key)
            .bind(value)
            .bind(db::now_ms())
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)")
        .bind(TAKEN_OVER)
        .bind(db::now_ms().to_string())
        .bind(db::now_ms())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    remote.handed_over().await?;
    tracing::info!("took AI Agent's data from RAST: {}", counts.join(", "));
    Ok(())
}

/// Copy each listed chat file (`<from>/<userId>/<id>`); how many.
fn copy_files(from: &Path, to: &Path, files: &[Value]) -> Result<usize, String> {
    let mut copied = 0;
    for file in files {
        let (Some(user), Some(id)) = (file["user_id"].as_i64(), file["id"].as_str()) else {
            continue;
        };
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            continue;
        }
        let source = from.join(user.to_string()).join(id);
        if !source.is_file() {
            continue;
        }
        let dir = to.join(user.to_string());
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
            .and_then(|_| std::fs::copy(&source, dir.join(id)))
            .map_err(|e| format!("chat file {id}: {e}"))?;
        copied += 1;
    }
    if let Ok(meta) = std::fs::metadata(to) {
        if meta.is_dir() {
            let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(copied)
}

/// Remove what removed accounts left (RAST's own tables did that through
/// foreign keys): their chats, usage and files; providers they signed in
/// stay, unowned.
pub async fn forget_removed_accounts(agent: &Agent) {
    let accounts = match agent.host.accounts().await {
        Ok(accounts) if !accounts.is_empty() => accounts,
        // Never on an empty or failed answer.
        _ => return,
    };
    let known: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    match forget_all_but(&agent.db, agent.data_dir(), &known).await {
        Ok(gone) if !gone.is_empty() => {
            tracing::info!(accounts = ?gone, "forgot the AI Agent data of removed accounts")
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not forget removed accounts"),
    }
}

/// Remove the rows and chat files of every account not in `known`; the
/// accounts forgotten.
pub async fn forget_all_but(
    pool: &SqlitePool,
    data_dir: &Path,
    known: &[i64],
) -> Result<Vec<i64>, sqlx::Error> {
    let known = json!(known).to_string();
    let gone: Vec<i64> = sqlx::query_scalar(
        "SELECT user_id FROM ai_conversations WHERE user_id NOT IN (SELECT value FROM json_each(?1))
         UNION SELECT user_id FROM ai_usage WHERE user_id NOT IN (SELECT value FROM json_each(?1))
         UNION SELECT user_id FROM ai_files WHERE user_id NOT IN (SELECT value FROM json_each(?1))
         UNION SELECT owner_id FROM ai_providers
               WHERE owner_id IS NOT NULL AND owner_id NOT IN (SELECT value FROM json_each(?1))",
    )
    .bind(&known)
    .fetch_all(pool)
    .await?;
    if gone.is_empty() {
        return Ok(gone);
    }
    let ids = json!(gone).to_string();
    let mut tx = pool.begin().await?;
    for sql in [
        "DELETE FROM ai_files WHERE user_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM ai_conversations WHERE user_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM ai_usage WHERE user_id IN (SELECT value FROM json_each(?))",
        "UPDATE ai_providers SET owner_id = NULL WHERE owner_id IN (SELECT value FROM json_each(?))",
    ] {
        sqlx::query(sql).bind(&ids).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    for id in &gone {
        let dir = super::files::root(data_dir).join(id.to_string());
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_says_what_is_new_in_this_version() {
        let notes = include_str!("../../frontend/src/apps/agent/whats-new.ts");
        assert!(
            notes.contains(&format!("version: \"{VERSION}\"")),
            "add AI Agent {VERSION} to frontend/src/apps/agent/whats-new.ts"
        );
    }

    #[tokio::test]
    async fn removed_accounts_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect_with(&dir.path().join("agent.db"), 2, &MIGRATOR)
            .await
            .unwrap();
        for sql in [
            "INSERT INTO ai_providers (id, kind, name, created_at, updated_at, owner_id)
             VALUES ('p', 'custom', 'P', 1, 1, 2)",
            "INSERT INTO ai_conversations (id, user_id, created_at, updated_at) VALUES ('a', 1, 1, 1)",
            "INSERT INTO ai_conversations (id, user_id, created_at, updated_at) VALUES ('b', 2, 1, 1)",
            "INSERT INTO ai_messages (conversation_id, role, created_at) VALUES ('b', 'user', 1)",
            "INSERT INTO ai_usage (user_id, created_at) VALUES (2, 1)",
            "INSERT INTO ai_files (id, user_id, conversation_id, name, mime, kind, size_bytes, created_at)
             VALUES ('f', 2, 'b', 'n.txt', 'text/plain', 'text', 1, 1)",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let files = super::super::files::root(dir.path()).join("2");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("f"), "x").unwrap();

        assert_eq!(
            forget_all_but(&pool, dir.path(), &[1, 2]).await.unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(
            forget_all_but(&pool, dir.path(), &[1]).await.unwrap(),
            vec![2]
        );
        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count("SELECT COUNT(*) FROM ai_conversations").await, 1);
        assert_eq!(count("SELECT COUNT(*) FROM ai_messages").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM ai_usage").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM ai_files").await, 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM ai_providers WHERE owner_id IS NULL").await,
            1,
            "the provider stays, unowned"
        );
        assert!(!files.exists());
    }
}
