//! What an agent can do besides talking: tools that act for the account
//! chatting, never with more rights than it has, and only once it allowed
//! the call in the chat ([`super::run`] asks; the owner may let a tool other
//! than the shell run without asking for the rest of a chat).
//!
//! * Files (`list_files`, `read_file`, `search_files`, `write_file`,
//!   `create_folder`): the account's RAST files through the same checked
//!   storage operations as File Station (home, shared folders it may open,
//!   quota), for accounts that may open File Station.
//! * `run_command`: one command in a fresh login shell, like Terminal:
//!   root on this server for administrators (needs `rast terminal enable`),
//!   the account's own sandbox container for accounts granted Terminal.
//! * `web_search` (the search service an administrator connects: Brave,
//!   Tavily or SearXNG) and `fetch_url` (public web pages only: addresses on
//!   this server or private networks are refused).

use std::{
    io::Read,
    net::{IpAddr, ToSocketAddrs},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{guard, provider::ToolSpec};
use crate::{
    auth::AuthUser,
    sandbox,
    secrets::Secrets,
    state::AppState,
    storage::ops,
    terminal::{RunOutput, TerminalClient, TerminalError},
};

pub const LIST_FILES: &str = "list_files";
pub const READ_FILE: &str = "read_file";
pub const SEARCH_FILES: &str = "search_files";
pub const WRITE_FILE: &str = "write_file";
pub const CREATE_FOLDER: &str = "create_folder";
pub const LIST_SERVER_FILES: &str = "list_server_files";
pub const READ_SERVER_FILE: &str = "read_server_file";
pub const RUN_COMMAND: &str = "run_command";
pub const WEB_SEARCH: &str = "web_search";
pub const FETCH_URL: &str = "fetch_url";

/// Every tool, in the order agents' settings list them.
pub const ALL: [&str; 10] = [
    LIST_FILES,
    READ_FILE,
    SEARCH_FILES,
    WRITE_FILE,
    CREATE_FOLDER,
    LIST_SERVER_FILES,
    READ_SERVER_FILE,
    RUN_COMMAND,
    WEB_SEARCH,
    FETCH_URL,
];

const FILE_TOOLS: [&str; 5] = [
    LIST_FILES,
    READ_FILE,
    SEARCH_FILES,
    WRITE_FILE,
    CREATE_FOLDER,
];

/// Default and longest time a command may run.
pub const COMMAND_SECS: u32 = 60;
pub const MAX_COMMAND_SECS: u32 = 600;
/// Largest file `write_file` writes.
pub const MAX_WRITE_BYTES: usize = 2 * 1024 * 1024;
/// What `read_file` reads at most.
const READ_BYTES: u64 = 128 * 1024;
/// Characters of a tool's result kept (sent to the model and stored).
pub const MAX_RESULT: usize = 40_000;
const SETTING_SEARCH: &str = "ai.search";
/// Whether accounts that are not administrators may read the server and run
/// read-only commands there ("0": no; anything else or unset: yes).
const SETTING_SERVER_ACCESS: &str = "ai.serverAccess";

pub fn label(name: &str) -> &'static str {
    match name {
        LIST_FILES => "List a folder",
        READ_FILE => "Read a file",
        SEARCH_FILES => "Find files",
        WRITE_FILE => "Write a file",
        CREATE_FOLDER => "Create a folder",
        LIST_SERVER_FILES => "List a server folder",
        READ_SERVER_FILE => "Read a server file",
        RUN_COMMAND => "Run a command",
        WEB_SEARCH => "Search the web",
        FETCH_URL => "Open a web page",
        _ => "Unknown tool",
    }
}

pub fn is_tool(name: &str) -> bool {
    ALL.contains(&name)
}

/// Whether the owner may let the tool run without asking in a chat: every
/// tool, commands included ("Allow everything"). Commands still run where
/// the account's commands run (administrators: root; others: read-only).
pub fn may_skip_asking(name: &str) -> bool {
    is_tool(name)
}

/// Where commands run for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// Administrators: root on this server.
    Root,
    /// Accounts granted Terminal: their own container.
    Sandbox,
    /// Everyone else: the server read-only, as `nobody` (see [`guard`]).
    Guest,
}

/// The tools an account may use now.
#[derive(Debug, Clone, Default)]
pub struct Available {
    pub names: Vec<&'static str>,
    pub shell: Option<Shell>,
    /// The account's home folder on the server (administrators' commands).
    pub home_on_disk: Option<String>,
    /// Not an administrator: the server only read-only, private places and
    /// secrets kept out (see [`guard`]).
    pub guarded: bool,
}

impl Available {
    pub fn has(&self, name: &str) -> bool {
        self.names.contains(&name)
    }

    /// Keep only `allowed` (an agent's tool list; `None`: all).
    pub fn restrict(mut self, allowed: Option<&[String]>) -> Self {
        if let Some(list) = allowed {
            self.names.retain(|name| list.iter().any(|a| a == name));
            if !self.has(RUN_COMMAND) {
                self.shell = None;
            }
        }
        self
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.names
            .iter()
            .filter_map(|name| spec(name, self.shell, self.guarded))
            .collect()
    }
}

pub async fn available(state: &AppState, user: &AuthUser) -> Available {
    let mut out = Available::default();
    let files = user.is_admin()
        || crate::access::app_access(&state.db, user, "files")
            .await
            .is_ok_and(|a| a.allowed);
    if files {
        out.names.extend(FILE_TOOLS);
    }
    out.guarded = !user.is_admin();
    let server = user.is_admin() || server_access(&state.db).await;
    if server {
        out.names.extend([LIST_SERVER_FILES, READ_SERVER_FILE]);
    }
    let helper = || async { TerminalClient::default().status().await.is_ok() };
    out.shell = if user.is_admin() {
        helper().await.then_some(Shell::Root)
    } else if server && helper().await {
        Some(Shell::Guest)
    } else if state.config.docker_enabled && sandbox::allowed(&state.db, user.id).await {
        Some(Shell::Sandbox)
    } else {
        None
    };
    if out.shell.is_some() {
        out.names.push(RUN_COMMAND);
    }
    if out.shell == Some(Shell::Root) {
        out.home_on_disk = Some(
            state
                .storage
                .home_dir(&user.username.to_ascii_lowercase())
                .display()
                .to_string(),
        );
    }
    if search_config(&state.db).await.is_configured() {
        out.names.push(WEB_SEARCH);
    }
    out.names.push(FETCH_URL);
    out
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

/// Whether accounts that are not administrators may read the server (and
/// run read-only commands there). On unless an administrator turned it off.
pub async fn server_access(pool: &sqlx::SqlitePool) -> bool {
    crate::db::get_setting(pool, SETTING_SERVER_ACCESS)
        .await
        .is_none_or(|value| value != "0")
}

pub async fn set_server_access(
    pool: &sqlx::SqlitePool,
    enabled: bool,
) -> crate::error::AppResult<()> {
    crate::db::set_setting(pool, SETTING_SERVER_ACCESS, if enabled { "1" } else { "0" }).await
}

pub fn spec(name: &str, shell: Option<Shell>, guarded: bool) -> Option<ToolSpec> {
    let server_path = json!({"type": "string", "description": "Absolute path on the server, such as /etc/os-release or /var/log"});
    let server_rule = if guarded {
        " Read-only. Private places (RAST's own data, people's files, home folders, account and network secrets, other processes) and files with secrets in their name are refused, and secret values are shown as [hidden]."
    } else {
        " Read as the RAST service account (commands, as root, can read the rest)."
    };
    let path = json!({"type": "string", "description": "Path in the person's files: '/' is their home folder, '/shared/<name>' a shared folder"});
    let (description, parameters) = match name {
        LIST_FILES => (
            "List a folder in the person's RAST files. Paths start with '/', their home folder; shared folders they can open are under '/shared/<name>'. One entry per line: dir or file, size, last change, name.".to_string(),
            schema(json!({"path": path}), &["path"]),
        ),
        READ_FILE => (
            "Read a text file from the person's RAST files (the first 128 KB).".to_string(),
            schema(json!({"path": path}), &["path"]),
        ),
        SEARCH_FILES => (
            "Find files and folders whose names contain the query, in a folder of the person's RAST files and its subfolders.".to_string(),
            schema(
                json!({"query": {"type": "string"}, "path": {"type": "string", "description": "Folder to search (default '/')"}}),
                &["query"],
            ),
        ),
        WRITE_FILE => (
            "Create a text file in the person's RAST files, or replace one, with the content given. Its folder must exist (create_folder makes one). Only change files the person asked you to.".to_string(),
            schema(
                json!({"path": path, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        ),
        CREATE_FOLDER => (
            "Create a folder in the person's RAST files (its parent must exist).".to_string(),
            schema(json!({"path": path}), &["path"]),
        ),
        LIST_SERVER_FILES => (
            format!("List a folder on the server itself (not the person's RAST files): one entry per line, dir or file, size, last change, name.{server_rule}"),
            schema(json!({"path": server_path}), &["path"]),
        ),
        READ_SERVER_FILE => (
            format!("Read a text file on the server itself (not the person's RAST files), the first 128 KB.{server_rule}"),
            schema(json!({"path": server_path}), &["path"]),
        ),
        RUN_COMMAND => {
            let place = match shell? {
                Shell::Root => "on this server as root (the RAST host). Use it to inspect and manage the server; be careful: changes are real and immediate",
                Shell::Sandbox => "in the person's own Linux sandbox (Ubuntu): their RAST home folder is the working directory, there is no network, and the server itself is out of reach",
                Shell::Guest => "on this server, read-only, as an account without privileges: use it to look (ls, cat, df, free, uptime, systemctl status, journal entries it may read) — nothing it does can change the server. Everything but a scratch /tmp (thrown away afterwards) is read-only; there is no network; RAST's own data, people's files, home folders and secrets are hidden; other processes are not visible; secret values in the output are shown as [hidden]",
            };
            (
                format!(
                    "Run a bash command {place}. Each call is a fresh login shell with no input: the working directory and variables do not carry over, and interactive programs cannot be used (use non-interactive flags such as -y or --no-pager). Returns the exit code and output."
                ),
                schema(
                    json!({
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "description": format!("Stop after this many seconds (default {COMMAND_SECS}, at most {MAX_COMMAND_SECS})")},
                    }),
                    &["command"],
                ),
            )
        }
        WEB_SEARCH => (
            "Search the web. Returns titles, links and short extracts; read a page with fetch_url.".to_string(),
            schema(json!({"query": {"type": "string"}}), &["query"]),
        ),
        FETCH_URL => (
            "Read a public web page (http or https) as plain text.".to_string(),
            schema(json!({"url": {"type": "string"}}), &["url"]),
        ),
        _ => return None,
    };
    Some(ToolSpec {
        name: name.to_string(),
        description,
        parameters,
    })
}

fn text_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing the text argument '{key}'"))
}

/// What a call is about, in one line (approval cards, activity log).
pub fn summary(name: &str, input: &Value) -> String {
    let key = match name {
        RUN_COMMAND => "command",
        WEB_SEARCH | SEARCH_FILES => "query",
        FETCH_URL => "url",
        _ => "path",
    };
    let value = input.get(key).and_then(Value::as_str).unwrap_or("");
    let line: String = value.chars().take(300).collect();
    format!("{name}: {line}")
}

/// Keep the start (and for command output the end) of a long result.
pub fn cap(text: &str, max: usize, keep_end: bool) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if keep_end {
        let head: String = text.chars().take(max / 2).collect();
        let tail: String = text.chars().skip(count - max / 2).collect();
        format!("{head}\n[… {} characters left out …]\n{tail}", count - max)
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}\n[… {} more characters left out]", count - max)
    }
}

/// Run one tool call for `user`: the result text for the model, or why it
/// failed (also for the model).
pub async fn execute(
    state: &AppState,
    user: &AuthUser,
    available: &Available,
    name: &str,
    input: &Value,
) -> Result<String, String> {
    if !available.has(name) {
        return Err(format!("the tool {name} is not available to this account"));
    }
    let public = |e: crate::error::AppError| e.public_message();
    match name {
        LIST_FILES => {
            let path = text_arg(input, "path")?;
            let listing = ops::list(
                &state.db,
                &state.storage,
                user,
                path,
                None,
                "name",
                "asc",
                1,
                500,
            )
            .await
            .map_err(public)?;
            if listing.items.is_empty() {
                return Ok(format!("{} is empty.", listing.path));
            }
            let mut lines = Vec::with_capacity(listing.items.len() + 1);
            for item in &listing.items {
                let dir = item.kind == "dir";
                let when = chrono::DateTime::from_timestamp_millis(item.modified_at)
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default();
                lines.push(format!(
                    "{}\t{}\t{}\t{}{}",
                    if dir { "dir" } else { "file" },
                    if dir {
                        String::new()
                    } else {
                        human_size(item.size)
                    },
                    when,
                    item.name,
                    if dir { "/" } else { "" },
                ));
            }
            let shown = listing.items.len() as i64;
            if listing.total > shown {
                lines.push(format!("… and {} more", listing.total - shown));
            }
            Ok(lines.join("\n"))
        }
        READ_FILE => {
            let path = text_arg(input, "path")?;
            let preview =
                ops::text_preview(&state.db, &state.storage, user, path, Some(READ_BYTES))
                    .await
                    .map_err(public)?;
            if preview.encoding == "binary" {
                return Err(format!("{path} is not a text file"));
            }
            let mut content = preview.content;
            if preview.truncated {
                content.push_str("\n[… the rest of the file was not read (128 KB limit)]");
            }
            Ok(content)
        }
        SEARCH_FILES => {
            let query = text_arg(input, "query")?;
            let path = input.get("path").and_then(Value::as_str).unwrap_or("/");
            let found = ops::search(&state.db, &state.storage, user, path, query, None, 1, 100)
                .await
                .map_err(public)?;
            if found.items.is_empty() {
                return Ok("Nothing found.".into());
            }
            let mut lines: Vec<String> = found
                .items
                .iter()
                .map(|item| format!("{}{}", item.path, if item.kind == "dir" { "/" } else { "" }))
                .collect();
            if found.truncated || found.total > found.items.len() as i64 {
                lines.push(
                    "… more matches left out; search a smaller folder or a longer name".into(),
                );
            }
            Ok(lines.join("\n"))
        }
        WRITE_FILE => {
            let path = text_arg(input, "path")?;
            let content = text_arg(input, "content")?;
            if content.len() > MAX_WRITE_BYTES {
                return Err(format!(
                    "files are limited to {} MB here",
                    MAX_WRITE_BYTES / 1024 / 1024
                ));
            }
            let (dir, name) = split_path(path)?;
            let existed = ops::stat(&state.db, &state.storage, user, path)
                .await
                .is_ok();
            let mut plan = ops::plan_upload(
                &state.db,
                &state.storage,
                user,
                &dir,
                &name,
                true,
                content.len() as u64,
            )
            .await
            .map_err(public)?;
            use tokio::io::AsyncWriteExt;
            if let Err(error) = plan.file.write_all(content.as_bytes()).await {
                ops::abort_upload(&state.db, user, plan).await;
                return Err(format!("cannot write the file: {error}"));
            }
            let entry =
                ops::commit_upload(&state.db, &state.storage, user, plan, content.len() as u64)
                    .await
                    .map_err(public)?;
            crate::realtime::publish_commit(
                &state.events,
                &state.db,
                user,
                "upload",
                std::slice::from_ref(&entry.path),
                Some(&entry),
                true,
            )
            .await;
            Ok(format!(
                "{} {} ({}).",
                if existed { "Replaced" } else { "Created" },
                entry.path,
                human_size(content.len() as i64)
            ))
        }
        CREATE_FOLDER => {
            let path = text_arg(input, "path")?;
            let entry = ops::mkdir(&state.db, &state.storage, user, path)
                .await
                .map_err(public)?;
            crate::realtime::publish_commit(
                &state.events,
                &state.db,
                user,
                "mkdir",
                std::slice::from_ref(&entry.path),
                Some(&entry),
                false,
            )
            .await;
            Ok(format!("Created the folder {}.", entry.path))
        }
        LIST_SERVER_FILES | READ_SERVER_FILE => {
            let path = text_arg(input, "path")?.to_string();
            let hidden = guard::hidden_paths(&state.config);
            let guarded = available.guarded;
            let listing = name == LIST_SERVER_FILES;
            tokio::task::spawn_blocking(move || {
                if listing {
                    list_server(&path, guarded, &hidden)
                } else {
                    read_server(&path, guarded, &hidden)
                }
            })
            .await
            .map_err(|e| e.to_string())?
        }
        RUN_COMMAND => {
            let command = text_arg(input, "command")?;
            if command.trim().is_empty() {
                return Err("the command is empty".into());
            }
            if command.len() > crate::terminal::MAX_RUN_COMMAND {
                return Err("the command is too long".into());
            }
            let secs = input
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .map(|s| s.clamp(1, u64::from(MAX_COMMAND_SECS)) as u32)
                .unwrap_or(COMMAND_SECS);
            // The helper's own limit is the most a guest command plus its
            // start and stop may take.
            let secs = if available.shell == Some(Shell::Guest) {
                secs.min(crate::terminal::MAX_RUN_SECS - 20)
            } else {
                secs
            };
            let name = user.username.to_ascii_lowercase();
            let result = match available.shell {
                Some(Shell::Root) => {
                    TerminalClient::default()
                        .run(user.id, &name, command, secs)
                        .await
                }
                Some(Shell::Sandbox) => {
                    sandbox::ready_home(&state.db, &state.storage, user)
                        .await
                        .map_err(public)?;
                    sandbox::client(&state.config)
                        .run(user.id, &name, command, secs)
                        .await
                }
                Some(Shell::Guest) => {
                    let hidden = guard::hidden_paths(&state.config);
                    let script = guard::guest_script(command, secs, &hidden)?;
                    if script.len() > crate::terminal::MAX_RUN_COMMAND {
                        return Err("the command is too long".into());
                    }
                    // Longer than the command's own limit: it is stopped
                    // inside first (and starting it takes a moment).
                    let started = std::time::Instant::now();
                    let mut output = TerminalClient::default()
                        .run(user.id, &name, &script, secs + 20)
                        .await;
                    if let Ok(output) = &mut output {
                        let ran = started.elapsed().as_secs() + 1 >= u64::from(secs);
                        if guard::timed_out(output.exit_code) && ran {
                            output.timed_out = true;
                        }
                    }
                    output
                }
                None => return Err("commands are not available to this account".into()),
            };
            match result {
                Ok(output) if available.guarded => {
                    let (text, hidden) = guard::redact(&command_result(&output, secs));
                    Ok(with_hidden_note(text, hidden))
                }
                Ok(output) => Ok(command_result(&output, secs)),
                Err(TerminalError::Off) if available.shell != Some(Shell::Sandbox) => Err(
                    "commands on this server need Terminal turned on (sudo rast terminal enable)".into(),
                ),
                Err(TerminalError::Off) => Err("commands need Docker on this server, and it is not running".into()),
                Err(TerminalError::Refused { code, .. }) if code == "invalid" && available.shell != Some(Shell::Sandbox) => Err(
                    "the server's terminal helper is older than RAST and cannot run commands yet; an administrator restarts it once with: sudo systemctl restart rast-terminal".into(),
                ),
                Err(error) => Err(error.to_string()),
            }
        }
        WEB_SEARCH => {
            let query = text_arg(input, "query")?.trim().to_string();
            if query.is_empty() || query.len() > 400 {
                return Err("give a search query (at most 400 characters)".into());
            }
            let config = search_config(&state.db).await;
            let key = config.key(&state.secrets);
            let hits =
                tokio::task::spawn_blocking(move || web_search(&config, key.as_deref(), &query))
                    .await
                    .map_err(|e| e.to_string())??;
            if hits.is_empty() {
                return Ok("No results.".into());
            }
            Ok(hits
                .iter()
                .enumerate()
                .map(|(i, hit)| {
                    format!(
                        "{}. {}\n   {}\n   {}",
                        i + 1,
                        hit.title,
                        hit.url,
                        hit.snippet
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        FETCH_URL => {
            let url = text_arg(input, "url")?.trim().to_string();
            tokio::task::spawn_blocking(move || fetch_page(&url))
                .await
                .map_err(|e| e.to_string())?
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

fn split_path(path: &str) -> Result<(String, String), String> {
    let trimmed = path.trim().trim_end_matches('/');
    let (dir, name) = trimmed
        .rsplit_once('/')
        .ok_or_else(|| "paths start with '/'".to_string())?;
    if name.is_empty() {
        return Err("give the file's path, including its name".into());
    }
    Ok((
        if dir.is_empty() {
            "/".into()
        } else {
            dir.to_string()
        },
        name.to_string(),
    ))
}

fn human_size(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    } else {
        format!("{:.1} GB", b / 1024.0 / 1024.0 / 1024.0)
    }
}

/// A note after text whose secrets were hidden.
fn with_hidden_note(mut text: String, hidden: usize) -> String {
    if hidden > 0 {
        text.push_str(&format!(
            "\n[{hidden} secret value{} hidden]",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    text
}

/// `list_server_files`: a folder on the server (see [`guard`] when
/// `guarded`).
fn list_server(raw: &str, guarded: bool, hidden: &[std::path::PathBuf]) -> Result<String, String> {
    let dir = if guarded {
        guard::allowed_path(raw, hidden)?
    } else {
        std::fs::canonicalize(raw).map_err(|e| format!("{raw}: {e}"))?
    };
    if !dir.is_dir() {
        return Err(format!(
            "{} is a file; read it with read_server_file",
            dir.display()
        ));
    }
    let mut entries = Vec::new();
    let mut left_out = 0usize;
    for entry in
        std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?
    {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if guarded && guard::refusal(&dir.join(&name), hidden).is_some() {
            left_out += 1;
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let size = meta
            .as_ref()
            .filter(|_| !is_dir)
            .map(|m| human_size(m.len() as i64))
            .unwrap_or_default();
        let when = meta
            .and_then(|m| m.modified().ok())
            .map(|t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        entries.push((is_dir, name, size, when));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = entries.len();
    let mut lines: Vec<String> = entries
        .into_iter()
        .take(500)
        .map(|(is_dir, name, size, when)| {
            format!(
                "{}\t{size}\t{when}\t{name}{}",
                if is_dir { "dir" } else { "file" },
                if is_dir { "/" } else { "" }
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push(format!("{} is empty.", dir.display()));
    } else {
        lines.insert(0, format!("{}:", dir.display()));
    }
    if total > 500 {
        lines.push(format!("[{} more not shown]", total - 500));
    }
    if left_out > 0 {
        lines.push(format!(
            "[{left_out} private entr{} left out]",
            if left_out == 1 { "y" } else { "ies" }
        ));
    }
    Ok(lines.join("\n"))
}

/// `read_server_file`: the start of a text file on the server (see
/// [`guard`] when `guarded`).
fn read_server(raw: &str, guarded: bool, hidden: &[std::path::PathBuf]) -> Result<String, String> {
    use std::io::Read;
    let path = if guarded {
        guard::allowed_path(raw, hidden)?
    } else {
        std::fs::canonicalize(raw).map_err(|e| format!("{raw}: {e}"))?
    };
    if path.is_dir() {
        return Err(format!(
            "{} is a folder; list it with list_server_files",
            path.display()
        ));
    }
    let file =
        std::fs::File::open(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.take(READ_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.contains(&0) {
        return Err(format!("{} is not a text file", path.display()));
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if guarded {
        if text.contains("PRIVATE KEY-----") {
            return Err(format!(
                "{} cannot be read: it holds a private key",
                path.display()
            ));
        }
        let (clean, count) = guard::redact(&text);
        text = with_hidden_note(clean, count);
    }
    if size > READ_BYTES {
        text.push_str(&format!(
            "\n[only the first {} of {} were read]",
            human_size(READ_BYTES as i64),
            human_size(size as i64)
        ));
    }
    Ok(text)
}

pub fn command_result(output: &RunOutput, secs: u32) -> String {
    let mut text = match (output.timed_out, output.exit_code) {
        (true, _) => format!("Stopped after {secs} s (time limit)."),
        (false, Some(code)) => format!("Exit code {code}."),
        (false, None) => "Ended by a signal.".to_string(),
    };
    if !output.stdout.is_empty() {
        text.push('\n');
        text.push_str(output.stdout.trim_end());
    }
    if !output.stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(output.stderr.trim_end());
    }
    if output.stdout.is_empty() && output.stderr.trim().is_empty() {
        text.push_str(" No output.");
    }
    if output.truncated {
        text.push_str("\n[the output was longer; only its first 64 KB per stream were kept]");
    }
    text
}

// --------------------------------------------------------------- web search

/// The search service an administrator connected (`kind` empty: none).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchConfig {
    /// `brave`, `tavily` or `searxng`.
    #[serde(default)]
    pub kind: String,
    /// SearXNG: the instance's address.
    #[serde(default)]
    pub url: String,
    /// Brave, Tavily: the API key, sealed with the server secret.
    #[serde(default)]
    pub key: String,
}

impl SearchConfig {
    pub fn is_configured(&self) -> bool {
        match self.kind.as_str() {
            "brave" | "tavily" => !self.key.is_empty(),
            "searxng" => !self.url.is_empty(),
            _ => false,
        }
    }

    pub fn key(&self, secrets: &Secrets) -> Option<String> {
        (!self.key.is_empty())
            .then(|| secrets.decrypt_string(&self.key).ok())
            .flatten()
    }

    pub fn public_json(&self, secrets: &Secrets) -> Value {
        let hint = self
            .key(secrets)
            .map(|k| {
                let tail: String = k
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("…{tail}")
            })
            .unwrap_or_default();
        json!({"kind": self.kind, "url": self.url, "hasKey": !self.key.is_empty(), "keyHint": hint, "configured": self.is_configured()})
    }
}

pub async fn search_config(pool: &sqlx::SqlitePool) -> SearchConfig {
    crate::db::get_setting(pool, SETTING_SEARCH)
        .await
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub async fn save_search_config(
    pool: &sqlx::SqlitePool,
    config: &SearchConfig,
) -> crate::error::AppResult<()> {
    crate::db::set_setting(
        pool,
        SETTING_SEARCH,
        &serde_json::to_string(config).unwrap_or_else(|_| "{}".into()),
    )
    .await
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn http_config(timeout: Duration, redirects: u32) -> ureq::config::Config {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; RAST/",
            env!("CARGO_PKG_VERSION"),
            " AI Agent)"
        ))
        .max_redirects(redirects)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .proxy(None)
        .build()
}

fn http_agent(timeout: Duration, redirects: u32) -> ureq::Agent {
    http_config(timeout, redirects).new_agent()
}

/// Looks names up like ureq does, and refuses addresses that are not public
/// at the moment of connecting, so a name that answers differently after
/// [`check_public`] (DNS rebinding) cannot lead to this server.
#[derive(Debug, Default)]
struct PublicOnly(ureq::unversioned::resolver::DefaultResolver);

impl ureq::unversioned::resolver::Resolver for PublicOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addresses = self.0.resolve(uri, config, timeout)?;
        if let Some(private) = addresses.iter().find(|a| !is_public(a.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is on this server or a private network", private.ip()),
            )));
        }
        Ok(addresses)
    }
}

/// An agent that only connects to public addresses (no proxy, no redirects:
/// [`fetch_page`] follows them itself).
fn public_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::with_parts(
        http_config(timeout, 0),
        ureq::unversioned::transport::DefaultConnector::default(),
        PublicOnly::default(),
    )
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

pub fn web_search(
    config: &SearchConfig,
    key: Option<&str>,
    query: &str,
) -> Result<Vec<SearchHit>, String> {
    let agent = http_agent(Duration::from_secs(20), 3);
    let (status, body) = match config.kind.as_str() {
        "brave" => {
            let key = key.ok_or("the web search key cannot be read")?;
            let response = agent
                .get(&format!(
                    "https://api.search.brave.com/res/v1/web/search?count=8&q={}",
                    encode(query)
                ))
                .header("Accept", "application/json")
                .header("X-Subscription-Token", key)
                .call()
                .map_err(|e| format!("cannot reach Brave Search: {e}"))?;
            read_response(response)
        }
        "tavily" => {
            let key = key.ok_or("the web search key cannot be read")?;
            let body = json!({"query": query, "max_results": 8}).to_string();
            let response = agent
                .post("https://api.tavily.com/search")
                .header("Content-Type", "application/json")
                .header("Authorization", &format!("Bearer {key}"))
                .send(body.as_bytes())
                .map_err(|e| format!("cannot reach Tavily: {e}"))?;
            read_response(response)
        }
        "searxng" => {
            let base = config.url.trim_end_matches('/');
            let response = agent
                .get(&format!("{base}/search?format=json&q={}", encode(query)))
                .header("Accept", "application/json")
                .call()
                .map_err(|e| format!("cannot reach SearXNG at {base}: {e}"))?;
            read_response(response)
        }
        _ => return Err("no web search service is connected".into()),
    };
    if !(200..300).contains(&status) {
        return Err(super::provider::refusal(status, &body).replace("provider", "search service"));
    }
    parse_search(&config.kind, &body)
}

fn read_response(response: ureq::http::Response<ureq::Body>) -> (u16, String) {
    let status = response.status().as_u16();
    let mut body = String::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut body);
    (status, body)
}

pub fn parse_search(kind: &str, body: &str) -> Result<Vec<SearchHit>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "the search service did not answer JSON".to_string())?;
    let (list, snippet_key) = match kind {
        "brave" => (value.pointer("/web/results"), "description"),
        _ => (value.get("results"), "content"),
    };
    let hits = list
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let url = item.get("url")?.as_str()?.to_string();
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(&url)
                        .to_string();
                    let snippet = item.get(snippet_key).and_then(Value::as_str).unwrap_or("");
                    Some(SearchHit {
                        title: strip_tags(&title),
                        url,
                        snippet: cap(&strip_tags(snippet), 400, false),
                    })
                })
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    Ok(hits)
}

/// Brave marks matches with `<strong>`: plain text.
fn strip_tags(text: &str) -> String {
    let (_, plain) = html_to_text(text);
    plain.replace('\n', " ").trim().to_string()
}

// ---------------------------------------------------------------- web pages

#[derive(Debug, PartialEq)]
pub struct WebUrl {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query (from `/`).
    pub rest: String,
}

impl WebUrl {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (https, after) = if let Some(rest) = strip_prefix_ignore_case(raw, "https://") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(raw, "http://") {
            (false, rest)
        } else {
            return Err("only http:// and https:// addresses can be opened".into());
        };
        if raw.len() > 4000 || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("that is not a web address".into());
        }
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let (authority, rest) = after.split_at(end);
        if authority.contains('@') {
            return Err("addresses with a user name or password are not opened".into());
        }
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let close = inner.find(']').ok_or("bad IPv6 address")?;
            let port = inner[close + 1..].strip_prefix(':');
            (inner[..close].to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), Some(port)),
                None => (authority.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => p.parse::<u16>().map_err(|_| "bad port".to_string())?,
            None if https => 443,
            None => 80,
        };
        let host = host.to_ascii_lowercase();
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
        {
            return Err(
                "that is not a web address (international names: use their xn-- form)".into(),
            );
        }
        let rest = rest.split('#').next().unwrap_or("");
        let rest = if rest.is_empty() || rest.starts_with('?') {
            format!("/{rest}")
        } else {
            rest.to_string()
        };
        Ok(Self {
            https,
            host,
            port,
            rest,
        })
    }

    pub fn to_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default = if self.https { 443 } else { 80 };
        let port = if self.port == default {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!(
            "{}://{host}{port}{}",
            if self.https { "https" } else { "http" },
            self.rest
        )
    }

    /// Where a redirect's `Location` points from here.
    pub fn join(&self, location: &str) -> Result<Self, String> {
        let location = location.trim();
        if location.starts_with("http://") || location.starts_with("https://") {
            Self::parse(location)
        } else if let Some(rest) = location.strip_prefix("//") {
            Self::parse(&format!(
                "{}://{rest}",
                if self.https { "https" } else { "http" }
            ))
        } else if location.starts_with('/') {
            Ok(Self {
                rest: location.to_string(),
                ..self.clone_base()
            })
        } else {
            let base = self.rest.split('?').next().unwrap_or("/");
            let dir = &base[..base.rfind('/').map(|i| i + 1).unwrap_or(1)];
            Ok(Self {
                rest: format!("{dir}{location}"),
                ..self.clone_base()
            })
        }
    }

    fn clone_base(&self) -> Self {
        Self {
            https: self.https,
            host: self.host.clone(),
            port: self.port,
            rest: String::new(),
        }
    }
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    (value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &value[prefix.len()..])
}

/// Addresses on the public internet (not this server, private networks,
/// link-local, carrier-grade NAT, documentation or reserved ranges).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0 && v4.octets()[2] == 0))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first == 0x2001 && v6.segments()[1] == 0x0db8)
                || first == 0x0064)
        }
    }
}

fn check_public(url: &WebUrl) -> Result<(), String> {
    let addresses: Vec<IpAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot find {}: {e}", url.host))?
        .map(|a| a.ip())
        .collect();
    if addresses.is_empty() {
        return Err(format!("cannot find {}", url.host));
    }
    if let Some(private) = addresses.iter().find(|ip| !is_public(**ip)) {
        return Err(format!(
            "{} is on this server or a private network ({private}); only public web pages are opened",
            url.host
        ));
    }
    Ok(())
}

/// Read a public web page as text (redirects followed and checked too).
pub fn fetch_page(raw: &str) -> Result<String, String> {
    let mut url = WebUrl::parse(raw)?;
    let agent = public_agent(Duration::from_secs(30));
    for _ in 0..6 {
        check_public(&url)?;
        let address = url.to_url();
        let response = agent
            .get(&address)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain;q=0.9,application/json;q=0.8,*/*;q=0.5",
            )
            .call()
            .map_err(|e| format!("cannot open {address}: {e}"))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{address} redirects without saying where"))?
                .to_string();
            url = url.join(&location)?;
            continue;
        }
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut bytes = Vec::new();
        let _ = response
            .into_body()
            .into_reader()
            .take(3 * 1024 * 1024)
            .read_to_end(&mut bytes);
        if status >= 400 {
            return Err(format!("{address} answered HTTP {status}"));
        }
        let textual = kind.is_empty()
            || kind.starts_with("text/")
            || kind.contains("json")
            || kind.contains("xml")
            || kind.contains("javascript");
        if !textual || bytes.contains(&0) {
            return Err(format!("{address} is not a text page ({kind})"));
        }
        let body = String::from_utf8_lossy(&bytes);
        let html = kind.contains("html") || (kind.is_empty() && body.trim_start().starts_with('<'));
        let (title, text) = if html {
            html_to_text(&body)
        } else {
            (String::new(), body.into_owned())
        };
        let mut out = format!("Address: {address}\n");
        if !title.is_empty() {
            out.push_str(&format!("Title: {title}\n"));
        }
        out.push('\n');
        out.push_str(&text);
        return Ok(cap(&out, MAX_RESULT, false));
    }
    Err("too many redirects".into())
}

/// The title and readable text of an HTML page.
pub fn html_to_text(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let mut title = String::new();
    let mut out = String::with_capacity(html.len() / 3);
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        if bytes[i] == b'<' {
            if lower[i..].starts_with("<!--") {
                i = lower[i..]
                    .find("-->")
                    .map(|e| i + e + 3)
                    .unwrap_or(html.len());
                continue;
            }
            let end = match lower[i..].find('>') {
                Some(e) => i + e + 1,
                None => break,
            };
            let tag = &lower[i + 1..end - 1];
            let closing = tag.starts_with('/');
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !closing
                && matches!(
                    name.as_str(),
                    "script"
                        | "style"
                        | "noscript"
                        | "svg"
                        | "template"
                        | "iframe"
                        | "head"
                        | "title"
                )
            {
                let close = format!("</{name}");
                let stop = lower[end..]
                    .find(&close)
                    .map(|e| end + e)
                    .unwrap_or(html.len());
                if name == "title" && title.is_empty() {
                    title = collapse(&decode_entities(&html[end..stop]));
                } else if name == "head" {
                    if let Some(t) = lower[end..stop].find("<title") {
                        let start = end + t;
                        if let Some(gt) = lower[start..stop].find('>') {
                            let from = start + gt + 1;
                            let to = lower[from..stop]
                                .find("</title")
                                .map(|e| from + e)
                                .unwrap_or(stop);
                            title = collapse(&decode_entities(&html[from..to]));
                        }
                    }
                }
                i = lower[stop..]
                    .find('>')
                    .map(|e| stop + e + 1)
                    .unwrap_or(html.len());
                continue;
            }
            match name.as_str() {
                "br" => out.push('\n'),
                "li" if !closing => out.push_str("\n- "),
                "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav"
                | "aside" | "ul" | "ol" | "table" | "tr" | "pre" | "blockquote" | "form"
                | "figure" | "dl" | "dt" | "dd" => out.push('\n'),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    out.push_str(if closing { "\n" } else { "\n\n# " })
                }
                "td" | "th" if !closing => out.push_str(" | "),
                _ => {}
            }
            i = end;
            continue;
        }
        let next = lower[i..].find('<').map(|e| i + e).unwrap_or(html.len());
        out.push_str(&decode_entities(&html[i..next]));
        i = next;
    }
    // Spaces from the markup collapse; the lines tags made stay.
    let mut text = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let line = collapse(line);
        let line = line.trim_start_matches("| ").trim();
        if line.is_empty() || line == "-" || line == "#" {
            blank += 1;
            if blank == 1 && !text.is_empty() {
                text.push('\n');
            }
            continue;
        }
        blank = 0;
        text.push_str(line);
        text.push('\n');
    }
    (title, text.trim().to_string())
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_files_for_people_who_are_not_administrators() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = dir.path();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let write = |name: &str, text: &str, mode: u32| {
            let path = root.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let app = write("app.conf", "port = 80\ndb_password = hunter2\n", 0o644);
        let closed = write("closed.conf", "x", 0o600);
        let named = write("api_token.txt", "abc", 0o644);
        let key = write(
            "deploy",
            "-----BEGIN RSA PRIVATE KEY-----\nMII\n-----END RSA PRIVATE KEY-----\n",
            0o644,
        );
        let private = root.join("rast-data");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hidden = vec![private.clone()];

        let text = read_server(&app, true, &hidden).unwrap();
        assert_eq!(
            text,
            "port = 80\ndb_password = [hidden]\n\n[1 secret value hidden]"
        );
        // Administrators read it as it is.
        assert_eq!(
            read_server(&app, false, &hidden).unwrap(),
            "port = 80\ndb_password = hunter2\n"
        );
        for path in [&closed, &named, &key] {
            assert!(read_server(path, true, &hidden).is_err(), "{path}");
        }
        assert!(read_server(&private.to_string_lossy(), true, &hidden).is_err());
        assert!(read_server("/proc/self/environ", true, &hidden).is_err());
        // Through a link: the real place decides.
        let link = root.join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(list_server(&link.to_string_lossy(), true, &hidden).is_err());

        let listing = list_server(&root.to_string_lossy(), true, &hidden).unwrap();
        assert!(
            listing.contains("\tapp.conf") && listing.contains("\tclosed.conf"),
            "{listing}"
        );
        assert!(
            !listing.contains("api_token") && !listing.contains("rast-data"),
            "{listing}"
        );
        assert!(
            listing.ends_with("[2 private entries left out]"),
            "{listing}"
        );
        assert!(read_server("/etc/hostname", true, &hidden).is_ok());
    }

    #[test]
    fn only_public_addresses_are_opened() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        let err =
            check_public(&WebUrl::parse("http://127.0.0.1:8720/healthz").unwrap()).unwrap_err();
        assert!(err.contains("private network"), "{err}");
        assert!(fetch_page("http://localhost:8720/")
            .unwrap_err()
            .contains("private network"));
        // Also when connecting (a name may answer differently by then).
        let err = public_agent(Duration::from_secs(5))
            .get("http://127.0.0.1:9/")
            .call()
            .unwrap_err()
            .to_string();
        assert!(err.contains("private network"), "{err}");
    }

    #[test]
    fn web_addresses_are_parsed_strictly() {
        let url = WebUrl::parse("HTTPS://Example.com/a/b?x=1#frag").unwrap();
        assert_eq!(
            url,
            WebUrl {
                https: true,
                host: "example.com".into(),
                port: 443,
                rest: "/a/b?x=1".into()
            }
        );
        assert_eq!(url.to_url(), "https://example.com/a/b?x=1");
        assert_eq!(
            WebUrl::parse("http://[::1]:8080").unwrap().to_url(),
            "http://[::1]:8080/"
        );
        assert!(WebUrl::parse("ftp://x").is_err());
        assert!(WebUrl::parse("http://user:pw@example.com/").is_err());
        assert!(WebUrl::parse("file:///etc/passwd").is_err());
        assert_eq!(url.join("/c").unwrap().to_url(), "https://example.com/c");
        assert_eq!(
            url.join("d?y").unwrap().to_url(),
            "https://example.com/a/d?y"
        );
        assert_eq!(
            url.join("//other.org/z").unwrap().to_url(),
            "https://other.org/z"
        );
    }

    #[test]
    fn pages_read_as_text() {
        let (title, text) = html_to_text(
            "<html><head><title>My &amp; Page</title><style>p{}</style></head><body>\
             <h1>Hello</h1><p>One  two<br>three &lt;b&gt; &#x41;</p><script>evil()</script>\
             <ul><li>a</li><li>b</li></ul><!-- hidden --><table><tr><td>x</td><td>y</td></tr></table></body></html>",
        );
        assert_eq!(title, "My & Page");
        assert_eq!(text, "# Hello\n\nOne two\nthree <b> A\n\n- a\n- b\n\nx | y");
    }

    #[test]
    fn search_answers_are_read() {
        let brave = r#"{"web":{"results":[{"title":"Rust <strong>lang</strong>","url":"https://rust-lang.org","description":"A <strong>language</strong>"}]}}"#;
        assert_eq!(
            parse_search("brave", brave).unwrap(),
            vec![SearchHit {
                title: "Rust lang".into(),
                url: "https://rust-lang.org".into(),
                snippet: "A language".into()
            }]
        );
        let tavily = r#"{"results":[{"title":"T","url":"https://t.io","content":"c"}]}"#;
        assert_eq!(parse_search("tavily", tavily).unwrap()[0].snippet, "c");
        assert!(parse_search("searxng", "<html>").is_err());
    }

    #[test]
    fn results_are_capped_and_summarised() {
        assert_eq!(
            cap("abcdef", 4, false),
            "abcd\n[… 2 more characters left out]"
        );
        assert_eq!(
            cap("abcdefgh", 4, true),
            "ab\n[… 4 characters left out …]\ngh"
        );
        assert_eq!(
            summary(RUN_COMMAND, &json!({"command": "ls -la"})),
            "run_command: ls -la"
        );
        assert_eq!(
            split_path("/notes/a.txt").unwrap(),
            ("/notes".into(), "a.txt".into())
        );
        assert_eq!(split_path("/a.txt").unwrap(), ("/".into(), "a.txt".into()));
        assert!(split_path("/").is_err());
        let out = RunOutput {
            exit_code: Some(1),
            stdout: "x\n".into(),
            stderr: "bad\n".into(),
            truncated: false,
            timed_out: false,
        };
        assert_eq!(command_result(&out, 60), "Exit code 1.\nx\n[stderr]\nbad");
    }
}
