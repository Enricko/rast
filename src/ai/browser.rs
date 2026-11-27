//! The agent's web browser: a headless Chrome per chat, so an agent can open
//! pages that need JavaScript, click, fill in forms and scroll, while the
//! person sees a screenshot of each step.
//!
//! * Chrome runs in a systemd unit of its own, started by the root terminal
//!   helper (`rast-terminal.service`, like `run_command`): as `nobody`, with
//!   Chrome's own sandbox on, home folders and RAST's data, storage and
//!   settings hidden, a private `/tmp` for its profile (gone when it stops),
//!   1 GiB of memory and at most two hours.
//! * Its only network is `127.0.0.1`, where RAST runs a proxy that connects
//!   to public internet addresses only (as `fetch_url` checks them): this
//!   server, private networks and other local services stay out of reach,
//!   whatever a page tries. QUIC is off and WebRTC only goes through the
//!   proxy.
//! * RAST speaks the DevTools protocol with it over a private socket:
//!   systemd connects Chrome's `--remote-debugging-pipe` to a socket only
//!   RAST can open, and closing it ends Chrome.
//! * One browser per chat, kept while it is used and closed after
//!   [`IDLE`] without use, when the chat is deleted, or when another chat
//!   needs its place (at most [`MAX_BROWSERS`] at a time).

use std::{
    collections::HashMap,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, LazyLock, Mutex, Once, Weak,
    },
    time::{Duration, Instant},
};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream, UnixListener, UnixStream,
    },
    sync::{oneshot, OnceCell, Semaphore},
};
use uuid::Uuid;

use super::{files, guard, tools};
use crate::{
    auth::AuthUser,
    config::Config,
    state::AppState,
    terminal::{TerminalClient, TerminalError},
};

/// Browsers open at a time on the server.
pub const MAX_BROWSERS: usize = 3;
/// A chat's browser closes after this long without use.
pub const IDLE: Duration = Duration::from_secs(10 * 60);
/// The most a browser lives, used or not.
const LIFETIME_SECS: u32 = 2 * 3600;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(25);
/// Largest DevTools message read (a screenshot is a few hundred KB).
const MAX_MESSAGE: usize = 32 * 1024 * 1024;
/// Characters of a page's text in a result.
const MAX_TEXT: usize = 16_000;
const MAX_TYPED: usize = 10_000;

/// Where Chrome may be installed (Google's package brings an AppArmor
/// profile that lets its sandbox start on Ubuntu).
const CHROMES: [&str; 3] = [
    "/opt/google/chrome/chrome",
    "/usr/lib/chromium/chromium",
    "/usr/lib/chromium-browser/chromium-browser",
];

const GONE: &str = "the browser stopped; the next step opens a new one";
const NOTHING_OPEN: &str =
    "this chat's browser has no page open yet: open one first (action \"open\" with a url)";
const NO_CHROME: &str = "the browser needs Google Chrome on this server (an administrator installs it from google.com/chrome; Linux 64-bit .deb)";

/// The Chrome RAST would start, if one is installed.
pub fn chrome() -> Option<&'static str> {
    CHROMES
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
}

/// What a step did, in one line (approval cards, activity log).
pub fn describe(input: &Value) -> String {
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let at = element_ref(input)
        .map(|n| format!("[{n}]"))
        .unwrap_or_default();
    match text("action") {
        "open" => format!("open {}", text("url")),
        "click" => format!("click {at}"),
        "type" => {
            let typed: String = text("text").chars().take(120).collect();
            let enter = if input.get("submit").and_then(Value::as_bool) == Some(true) {
                " + Enter"
            } else {
                ""
            };
            format!("type \"{typed}\" into {at}{enter}")
        }
        "scroll" => format!(
            "scroll {}",
            input
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("down")
        ),
        other => other.to_string(),
    }
}

fn element_ref(input: &Value) -> Option<u64> {
    match input.get("ref")? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_matches(['[', ']']).trim().parse().ok(),
        _ => None,
    }
}

// ------------------------------------------------------------ the sessions

struct Session {
    cdp: Arc<Cdp>,
    /// The DevTools session of the chat's one tab.
    page: String,
    last_used: AtomicI64,
    busy: tokio::sync::Mutex<()>,
}

impl Session {
    fn touch(&self) {
        self.last_used.store(crate::db::now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self, now: i64) -> i64 {
        now - self.last_used.load(Ordering::Relaxed)
    }

    fn in_use(&self) -> bool {
        self.busy.try_lock().is_err()
    }
}

static BROWSERS: LazyLock<Mutex<HashMap<String, Arc<Session>>>> = LazyLock::new(Default::default);
static REAPER: Once = Once::new();

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Session>>> {
    BROWSERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Close a chat's browser (the chat is being deleted).
pub fn close(conversation: &str) {
    let gone = sessions().remove(conversation);
    drop(gone);
}

/// Every minute, close the browsers nobody used for [`IDLE`].
fn start_reaper() {
    REAPER.call_once(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let now = crate::db::now_ms();
                let idle = IDLE.as_millis() as i64;
                let gone: Vec<Arc<Session>> = {
                    let mut map = sessions();
                    let ids: Vec<String> = map
                        .iter()
                        .filter(|(_, s)| {
                            s.cdp.is_closed() || (s.idle_for(now) > idle && !s.in_use())
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    ids.iter().filter_map(|id| map.remove(id)).collect()
                };
                drop(gone);
            }
        });
    });
}

/// Make room for one more browser: close the one used longest ago that is
/// not in the middle of a step.
fn make_room() -> Result<(), String> {
    let mut map = sessions();
    map.retain(|_, s| !s.cdp.is_closed());
    if map.len() < MAX_BROWSERS {
        return Ok(());
    }
    let now = crate::db::now_ms();
    let oldest = map
        .iter()
        .filter(|(_, s)| !s.in_use())
        .max_by_key(|(_, s)| s.idle_for(now))
        .map(|(id, _)| id.clone());
    match oldest {
        Some(id) => {
            map.remove(&id);
            Ok(())
        }
        None => Err(format!(
            "the server's browsers are all busy ({MAX_BROWSERS} at a time); try again in a moment"
        )),
    }
}

async fn session_for(
    state: &AppState,
    user: &AuthUser,
    conversation: &str,
    may_start: bool,
) -> Result<Arc<Session>, String> {
    start_reaper();
    let open = sessions()
        .get(conversation)
        .filter(|s| !s.cdp.is_closed())
        .cloned();
    if let Some(session) = open {
        return Ok(session);
    }
    if !may_start {
        return Err(NOTHING_OPEN.into());
    }
    make_room()?;
    let session = Arc::new(start(state, user).await?);
    sessions().insert(conversation.to_string(), session.clone());
    Ok(session)
}

/// Removes the socket file however starting ends.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn start(state: &AppState, user: &AuthUser) -> Result<Session, String> {
    let chrome = chrome().ok_or(NO_CHROME)?;
    let port = proxy_port().await?;
    let dir = state.data_dir().join("browser");
    std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("the browser's folder could not be made: {e}"))?;
    let id = Uuid::new_v4().simple().to_string()[..16].to_string();
    let path = dir.join(format!("{id}.sock"));
    // sun_path holds 108 bytes.
    if path.as_os_str().len() > 100 {
        return Err("RAST's data folder path is too long for the browser's socket".into());
    }
    let listener = UnixListener::bind(&path)
        .map_err(|e| format!("the browser's socket could not be made: {e}"))?;
    let socket = SocketFile(path);
    std::fs::set_permissions(&socket.0, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let script = launch_script(
        chrome,
        &format!("rast-browser-{id}"),
        &socket.0,
        port,
        &state.config,
    )?;
    let name = user.username.to_ascii_lowercase();
    let output = TerminalClient::default()
        .run(user.id, &name, &script, 30)
        .await
        .map_err(|e| match e {
            TerminalError::Off => {
                "the browser needs Terminal turned on on this server (sudo rast terminal enable)"
                    .to_string()
            }
            other => format!("the browser did not start: {other}"),
        })?;
    if output.exit_code != Some(0) {
        let said = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        return Err(format!("the browser did not start: {said}"));
    }
    let (stream, _) = tokio::time::timeout(START_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "the browser did not start in time".to_string())?
        .map_err(|e| format!("the browser did not start: {e}"))?;
    drop(listener);
    drop(socket);
    let cdp = Cdp::start(stream);
    let page = prepare(&cdp).await.map_err(|e| {
        if cdp.is_closed() {
            "Chrome stopped while starting (on Ubuntu, Google Chrome's own .deb brings the AppArmor profile its sandbox needs)".to_string()
        } else {
            format!("the browser did not start: {e}")
        }
    })?;
    let session = Session {
        cdp,
        page,
        last_used: AtomicI64::new(0),
        busy: tokio::sync::Mutex::new(()),
    };
    session.touch();
    Ok(session)
}

/// Pages that open a window go to it in the same tab instead.
const SAME_TAB: &str = "window.open = (url) => { if (url) location.href = new URL(String(url), location.href).href; return null; };";

/// The tab's DevTools session, ready for steps.
async fn prepare(cdp: &Cdp) -> Result<String, String> {
    let version = cdp.call(None, "Browser.getVersion", json!({})).await?;
    let agent = version["userAgent"]
        .as_str()
        .unwrap_or("")
        .replace("HeadlessChrome", "Chrome");
    cdp.call(None, "Target.setDiscoverTargets", json!({"discover": true}))
        .await?;
    let _ = cdp
        .call(
            None,
            "Browser.setDownloadBehavior",
            json!({"behavior": "deny"}),
        )
        .await;
    let targets = cdp.call(None, "Target.getTargets", json!({})).await?;
    let existing = targets["targetInfos"].as_array().and_then(|list| {
        list.iter()
            .find(|t| t["type"] == "page")
            .and_then(|t| t["targetId"].as_str())
            .map(str::to_string)
    });
    let target = match existing {
        Some(id) => id,
        None => cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?["targetId"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    };
    let attached = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
        )
        .await?;
    let page = attached["sessionId"]
        .as_str()
        .ok_or("the browser has no tab")?
        .to_string();
    cdp.call(Some(&page), "Page.enable", json!({})).await?;
    cdp.call(
        Some(&page),
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": SAME_TAB}),
    )
    .await?;
    if !agent.is_empty() {
        cdp.call(
            Some(&page),
            "Emulation.setUserAgentOverride",
            json!({"userAgent": agent, "acceptLanguage": "en-US,en;q=0.9"}),
        )
        .await?;
    }
    Ok(page)
}

/// The root script that starts Chrome in its unit (see the module docs).
fn launch_script(
    chrome: &str,
    unit: &str,
    socket: &Path,
    proxy_port: u16,
    config: &Config,
) -> Result<String, String> {
    let socket = socket.to_string_lossy();
    let plain = |text: &str| {
        !text
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control() || c.is_whitespace())
    };
    if !plain(&socket) {
        return Err("RAST's data folder has an unusual name; the browser is off".into());
    }
    let mut args: Vec<String> = [
        "--quiet",
        "--collect",
        "--service-type=exec",
        "--expand-environment=no",
        "--description=RAST agent browser",
        "--property=User=nobody",
        "--property=Group=nogroup",
        "--property=ProtectSystem=strict",
        "--property=ProtectHome=yes",
        "--property=PrivateTmp=yes",
        "--property=PrivateDevices=yes",
        "--property=ProtectKernelTunables=yes",
        "--property=ProtectKernelModules=yes",
        "--property=ProtectKernelLogs=yes",
        "--property=ProtectControlGroups=yes",
        "--property=ProtectClock=yes",
        "--property=ProtectHostname=yes",
        "--property=ProtectProc=invisible",
        "--property=NoNewPrivileges=yes",
        "--property=RestrictSUIDSGID=yes",
        "--property=CapabilityBoundingSet=",
        "--property=LockPersonality=yes",
        "--property=RestrictRealtime=yes",
        "--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK",
        // Only the proxy, on 127.0.0.1: see the module docs.
        "--property=IPAddressDeny=any",
        "--property=IPAddressAllow=127.0.0.1/32",
        "--property=MemoryMax=1G",
        "--property=TasksMax=512",
        "--property=CPUQuota=200%",
        "--property=TimeoutStopSec=5",
        "--property=WorkingDirectory=/tmp",
        "--property=Environment=HOME=/tmp LANG=C.UTF-8",
        "--property=StandardError=null",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--unit={unit}"));
    args.push(format!("--property=RuntimeMaxSec={LIFETIME_SECS}"));
    // One connection for both directions (systemd connects to the socket).
    args.push(format!("--property=StandardInput=file:{socket}"));
    args.push(format!("--property=StandardOutput=file:{socket}"));
    for path in guard::hidden_paths(config) {
        let text = path.to_string_lossy();
        if text
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
        {
            return Err(
                "a server folder RAST hides has an unusual name; the browser is off".into(),
            );
        }
        args.push(format!("--property=InaccessiblePaths=\"-{text}\""));
    }
    let chrome_args: Vec<String> = [
        chrome.to_string(),
        "--headless=new".into(),
        "--remote-debugging-pipe".into(),
        "--user-data-dir=/tmp/profile".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-dev-shm-usage".into(),
        "--disable-quic".into(),
        format!("--proxy-server=http://127.0.0.1:{proxy_port}"),
        "--proxy-bypass-list=<-loopback>".into(),
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--disable-extensions".into(),
        "--disable-default-apps".into(),
        "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
        "--no-pings".into(),
        "--mute-audio".into(),
        "--hide-scrollbars".into(),
        "--password-store=basic".into(),
        "--lang=en-US".into(),
        "--window-size=1280,800".into(),
        "about:blank".into(),
    ]
    .into();
    let chrome_line: Vec<String> = chrome_args.iter().map(|a| guard::shell_quote(a)).collect();
    // DevTools on fds 3 (in) and 4 (out): both the socket systemd connected.
    let inner = format!(
        "exec 3<&0 4>&1 </dev/null >/dev/null; exec {}",
        chrome_line.join(" ")
    );
    args.extend(
        ["--", "/bin/bash", "--noprofile", "--norc", "-c"]
            .iter()
            .map(|s| s.to_string()),
    );
    args.push(inner);
    let quoted: Vec<String> = args.iter().map(|a| guard::shell_quote(a)).collect();
    Ok(format!("exec systemd-run {}", quoted.join(" ")))
}

// --------------------------------------------------------- DevTools client

type Reply = oneshot::Sender<Result<Value, String>>;

struct Cdp {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Mutex<HashMap<u64, Reply>>,
    next: AtomicU64,
    closed: AtomicBool,
}

impl Cdp {
    fn start(stream: UnixStream) -> Arc<Self> {
        let (read, write) = stream.into_split();
        let cdp = Arc::new(Self {
            writer: tokio::sync::Mutex::new(write),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(read_messages(read, Arc::downgrade(&cdp)));
        cdp
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Reply>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        if self.is_closed() {
            return Err(GONE.into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let mut bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        bytes.push(0);
        let sent = self.writer.lock().await.write_all(&bytes).await;
        if sent.is_err() {
            self.pending().remove(&id);
            return Err(GONE.into());
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(GONE.into()),
            Err(_) => {
                self.pending().remove(&id);
                Err(format!("the browser did not answer in time ({method})"))
            }
        }
    }

    fn handle(self: &Arc<Self>, message: Value) {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(reply) = self.pending().remove(&id) {
                let result = match message.get("error") {
                    Some(error) => Err(error["message"]
                        .as_str()
                        .unwrap_or("the browser refused")
                        .to_string()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = reply.send(result);
            }
            return;
        }
        let params = &message["params"];
        match message["method"].as_str().unwrap_or("") {
            // alert(), confirm() and prompt() would hold the page: accept them.
            "Page.javascriptDialogOpening" => {
                let session = message["sessionId"].as_str().map(str::to_string);
                let cdp = self.clone();
                tokio::spawn(async move {
                    let _ = cdp
                        .call(
                            session.as_deref(),
                            "Page.handleJavaScriptDialog",
                            json!({"accept": true}),
                        )
                        .await;
                });
            }
            // A page opened another tab anyway: close it, the agent works in one.
            "Target.targetCreated"
                if params["targetInfo"]["type"] == "page"
                    && params["targetInfo"]["openerId"].is_string() =>
            {
                let target = params["targetInfo"]["targetId"].clone();
                let cdp = self.clone();
                tokio::spawn(async move {
                    let _ = cdp
                        .call(None, "Target.closeTarget", json!({"targetId": target}))
                        .await;
                });
            }
            _ => {}
        }
    }

    fn fail_all(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for (_, reply) in self.pending().drain() {
            let _ = reply.send(Err(GONE.into()));
        }
    }
}

/// Read NUL-separated messages until Chrome closes its end.
async fn read_messages(mut read: OwnedReadHalf, cdp: Weak<Cdp>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut scanned = 0;
    loop {
        let n = match read.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..n]);
        while let Some(at) = buffer[scanned..].iter().position(|&b| b == 0) {
            let end = scanned + at;
            let Some(cdp) = cdp.upgrade() else {
                return;
            };
            if let Ok(message) = serde_json::from_slice::<Value>(&buffer[..end]) {
                cdp.handle(message);
            }
            buffer.drain(..=end);
            scanned = 0;
        }
        scanned = buffer.len();
        if buffer.len() > MAX_MESSAGE {
            break;
        }
    }
    if let Some(cdp) = cdp.upgrade() {
        cdp.fail_all();
    }
}

// ------------------------------------------------------------------- steps

/// Run one browser step for the chat: the page as text for the model and a
/// screenshot for the person.
pub async fn act(
    state: &AppState,
    user: &AuthUser,
    conversation: &str,
    input: &Value,
) -> Result<tools::Output, String> {
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .ok_or("say which action: open, click, type, scroll, back or read")?;
    if !matches!(
        action,
        "open" | "click" | "type" | "scroll" | "back" | "read"
    ) {
        return Err(format!(
            "unknown action {action:?}: use open, click, type, scroll, back or read"
        ));
    }
    let url = match action {
        "open" => Some(web_address(
            input.get("url").and_then(Value::as_str).unwrap_or(""),
        )?),
        _ => None,
    };
    let session = session_for(state, user, conversation, url.is_some()).await?;
    let _busy = session.busy.lock().await;
    session.touch();
    match action {
        "open" => {
            let opened = session
                .cdp
                .call(Some(&session.page), "Page.navigate", json!({"url": url}))
                .await?;
            if let Some(error) = opened["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Err(not_opened(error));
            }
            settle(&session, 300).await;
        }
        "click" => click(&session, element_ref(input).ok_or(NO_REF)?).await?,
        "type" => {
            let text = input.get("text").and_then(Value::as_str).unwrap_or("");
            if text.chars().count() > MAX_TYPED {
                return Err(format!("type at most {MAX_TYPED} characters at a time"));
            }
            let submit = input.get("submit").and_then(Value::as_bool) == Some(true);
            type_into(&session, element_ref(input).ok_or(NO_REF)?, text, submit).await?;
        }
        "scroll" => {
            let up = input.get("direction").and_then(Value::as_str) == Some("up");
            let sign = if up { "-" } else { "" };
            eval(
                &session,
                &format!("window.scrollBy({{ top: {sign}Math.round(window.innerHeight * 0.85), behavior: \"instant\" }}); 0"),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        "back" => back(&session).await?,
        _ => {}
    }
    let view = eval(&session, &format!("({SNAPSHOT})({MAX_TEXT})")).await?;
    let image = screenshot(&session, state, user, conversation, &view)
        .await
        .map_err(|e| tracing::debug!(error = %e, "browser screenshot"))
        .ok();
    session.touch();
    Ok(tools::Output {
        text: render(&view),
        image,
    })
}

const NO_REF: &str = "say which element: its number from the page, like 12 for [12 …]";

fn gone_ref(n: u64) -> String {
    format!("there is no element [{n}] on the page now; use the numbers from the latest page (action \"read\" shows it again)")
}

fn not_opened(error: &str) -> String {
    let why = match error {
        "net::ERR_TUNNEL_CONNECTION_FAILED" | "net::ERR_PROXY_CONNECTION_FAILED" => {
            ": the address may not exist, or it is on this server or a private network, which the browser leaves out"
        }
        "net::ERR_NAME_NOT_RESOLVED" => ": that name does not exist",
        "net::ERR_ABORTED" => ": it was a download or the page stopped it",
        _ => "",
    };
    format!("the page did not open ({error}){why}")
}

/// An http(s) address; a bare name gets https://.
fn web_address(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("give the page's address (url)".into());
    }
    if url.len() > 4000 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("that address is not a web address".into());
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(url.to_string());
    }
    if url.contains("://")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("file:")
    {
        return Err("the browser opens http and https addresses".into());
    }
    Ok(format!("https://{url}"))
}

async fn eval(session: &Session, expression: &str) -> Result<Value, String> {
    let result = session
        .cdp
        .call(
            Some(&session.page),
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        )
        .await?;
    if let Some(error) = result.get("exceptionDetails") {
        let said = error["exception"]["description"]
            .as_str()
            .or(error["text"].as_str())
            .unwrap_or("an error");
        let line = said.lines().next().unwrap_or(said);
        return Err(format!("the page answered with {line}"));
    }
    Ok(result["result"]["value"].clone())
}

/// Wait for what a step started (a new page loading) to finish.
async fn settle(session: &Session, first_ms: u64) {
    tokio::time::sleep(Duration::from_millis(first_ms)).await;
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if let Ok(ready) = eval(session, "document.readyState").await {
            if ready == "complete" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
}

const CLICK_TARGET: &str = r##"(n) => {
  const el = document.querySelector(`[data-rast-ref="${n}"]`);
  if (!el) return null;
  const link = el.closest("a");
  if (link && link.target && link.target !== "_self") link.target = "_self";
  el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width, h: r.height };
}"##;

async fn click(session: &Session, n: u64) -> Result<(), String> {
    let at = eval(session, &format!("({CLICK_TARGET})({n})")).await?;
    if at.is_null() {
        return Err(gone_ref(n));
    }
    let (x, y) = (
        at["x"].as_f64().unwrap_or(0.0),
        at["y"].as_f64().unwrap_or(0.0),
    );
    if at["w"].as_f64().unwrap_or(0.0) < 1.0 || at["h"].as_f64().unwrap_or(0.0) < 1.0 {
        eval(
            session,
            &format!("document.querySelector('[data-rast-ref=\"{n}\"]')?.click(); 0"),
        )
        .await?;
    } else {
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let button = if kind == "mouseMoved" { "none" } else { "left" };
            session
                .cdp
                .call(
                    Some(&session.page),
                    "Input.dispatchMouseEvent",
                    json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": 1}),
                )
                .await?;
        }
    }
    settle(session, 400).await;
    Ok(())
}

const TYPE_TARGET: &str = r##"(n, text) => {
  const el = document.querySelector(`[data-rast-ref="${n}"]`);
  if (!el) return "gone";
  el.scrollIntoView({ block: "center", behavior: "instant" });
  if (el.tagName === "SELECT") {
    const want = text.trim().toLowerCase();
    const options = [...el.options];
    const option = options.find((o) => o.text.trim().toLowerCase() === want || o.value.toLowerCase() === want)
      || options.find((o) => o.text.toLowerCase().includes(want));
    if (!option) return "options: " + options.map((o) => o.text.trim()).slice(0, 40).join(" | ");
    el.value = option.value;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return "chosen";
  }
  el.focus();
  if (el.isContentEditable) {
    const range = document.createRange();
    range.selectNodeContents(el);
    const selection = getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  } else if (typeof el.select === "function") {
    el.select();
  }
  return document.activeElement === el || el.contains(document.activeElement) ? "focused" : "unfocused";
}"##;

async fn key(session: &Session, key: &str, code: u32, text: Option<&str>) -> Result<(), String> {
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": key, "code": key, "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code,
    });
    if let Some(text) = text {
        down["text"] = json!(text);
    }
    let up = json!({"type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code});
    for event in [down, up] {
        session
            .cdp
            .call(Some(&session.page), "Input.dispatchKeyEvent", event)
            .await?;
    }
    Ok(())
}

async fn type_into(session: &Session, n: u64, text: &str, submit: bool) -> Result<(), String> {
    let literal = serde_json::to_string(text).map_err(|e| e.to_string())?;
    let target = eval(session, &format!("({TYPE_TARGET})({n}, {literal})")).await?;
    match target.as_str().unwrap_or("gone") {
        "gone" => return Err(gone_ref(n)),
        "unfocused" => {
            return Err(format!(
                "[{n}] does not take typing; click it, or choose a field"
            ))
        }
        "chosen" => {}
        options if options.starts_with("options: ") => {
            return Err(format!("[{n}] has no option {text:?}; its {options}"));
        }
        _ => {
            if text.is_empty() {
                key(session, "Backspace", 8, None).await?;
            } else {
                session
                    .cdp
                    .call(
                        Some(&session.page),
                        "Input.insertText",
                        json!({"text": text}),
                    )
                    .await?;
            }
        }
    }
    if submit {
        key(session, "Enter", 13, Some("\r")).await?;
        settle(session, 500).await;
    } else {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok(())
}

async fn back(session: &Session) -> Result<(), String> {
    let history = session
        .cdp
        .call(Some(&session.page), "Page.getNavigationHistory", json!({}))
        .await?;
    let at = history["currentIndex"].as_u64().unwrap_or(0) as usize;
    let earlier = at
        .checked_sub(1)
        .and_then(|i| history["entries"].get(i))
        .filter(|entry| entry["url"] != "about:blank")
        .ok_or("there is no earlier page in this chat's browser")?;
    session
        .cdp
        .call(
            Some(&session.page),
            "Page.navigateToHistoryEntry",
            json!({"entryId": earlier["id"]}),
        )
        .await?;
    settle(session, 300).await;
    Ok(())
}

async fn screenshot(
    session: &Session,
    state: &AppState,
    user: &AuthUser,
    conversation: &str,
    view: &Value,
) -> Result<Value, String> {
    let shot = session
        .cdp
        .call(
            Some(&session.page),
            "Page.captureScreenshot",
            json!({"format": "jpeg", "quality": 70, "optimizeForSpeed": true}),
        )
        .await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().unwrap_or(""))
        .map_err(|e| e.to_string())?;
    let host = view["url"]
        .as_str()
        .and_then(|u| u.split("://").nth(1))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .filter(|h| !h.is_empty())
        .unwrap_or("page");
    let name = format!("Screenshot {host}.jpg");
    let file = files::store(
        &state.db,
        state.data_dir(),
        user.id,
        Some(&name),
        "image/jpeg",
        &bytes,
    )
    .await
    .map_err(|e| e.public_message())?;
    files::attach(
        &state.db,
        user.id,
        conversation,
        std::slice::from_ref(&file.id),
    )
    .await
    .map_err(|e| e.public_message())?;
    Ok(file.json())
}

/// The page as text: what it says, with what can be clicked or typed into
/// marked `[n kind "name"]` (`data-rast-ref` on the element), the main
/// frame only.
const SNAPSHOT: &str = r##"(maxText) => {
  for (const el of document.querySelectorAll("[data-rast-ref]")) el.removeAttribute("data-rast-ref");
  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "SVG", "CANVAS", "IFRAME", "OBJECT", "EMBED", "HEAD", "VIDEO", "AUDIO"]);
  const BLOCK = new Set(["ADDRESS", "ARTICLE", "ASIDE", "BLOCKQUOTE", "BR", "DD", "DETAILS", "DIALOG", "DIV", "DL", "DT", "FIELDSET", "FIGCAPTION", "FIGURE", "FOOTER", "FORM", "H1", "H2", "H3", "H4", "H5", "H6", "HEADER", "HR", "LI", "MAIN", "NAV", "OL", "P", "PRE", "SECTION", "TABLE", "TR", "UL"]);
  const ROLES = { button: "button", link: "link", checkbox: "checkbox", radio: "radio", tab: "tab", menuitem: "menu item", menuitemcheckbox: "menu item", menuitemradio: "menu item", switch: "switch", option: "option", combobox: "field", textbox: "field", searchbox: "field", slider: "slider", spinbutton: "field" };
  const parts = [];
  let size = 0, count = 0, cut = false;
  const push = (s) => {
    if (cut) return;
    if (size + s.length > maxText) { parts.push(s.slice(0, maxText - size)); cut = true; return; }
    parts.push(s);
    size += s.length;
  };
  const clean = (s) => String(s || "").replace(/\s+/g, " ").trim();
  const shown = (el) => {
    if (typeof el.checkVisibility === "function" ? el.checkVisibility({ visibilityProperty: true }) : el.getClientRects().length > 0) return true;
    return getComputedStyle(el).display === "contents";
  };
  const kind = (el) => {
    const tag = el.tagName;
    if (tag === "A" && el.hasAttribute("href")) return "link";
    if (tag === "BUTTON" || tag === "SUMMARY") return "button";
    if (tag === "INPUT") {
      const t = (el.getAttribute("type") || "text").toLowerCase();
      if (t === "hidden") return null;
      if (["button", "submit", "reset", "image"].includes(t)) return "button";
      if (t === "checkbox" || t === "radio") return t;
      return "field";
    }
    if (tag === "TEXTAREA") return "field";
    if (tag === "SELECT") return "list";
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return "field";
    return ROLES[(el.getAttribute("role") || "").toLowerCase()] || null;
  };
  const name = (el) => clean(
    el.getAttribute("aria-label")
    || (el.labels && el.labels[0] && el.labels[0].innerText)
    || (el.tagName === "INPUT" && ["button", "submit", "reset"].includes(el.type) ? el.value : "")
    || (el.tagName !== "INPUT" && el.tagName !== "TEXTAREA" && el.tagName !== "SELECT" ? el.innerText : "")
    || el.getAttribute("placeholder") || el.getAttribute("title") || el.getAttribute("alt")
    || (el.querySelector && el.querySelector("img[alt]") ? el.querySelector("img[alt]").alt : "")
    || el.getAttribute("name") || ""
  ).slice(0, 100).replace(/"/g, "'");
  const walk = (node) => {
    if (cut) return;
    if (node.nodeType === 3) {
      const t = node.data.replace(/\s+/g, " ");
      if (t.trim()) push(t);
      return;
    }
    if (node.nodeType !== 1) return;
    const el = node;
    const tag = el.tagName.toUpperCase();
    if (SKIP.has(tag) || !shown(el)) return;
    const block = BLOCK.has(tag);
    if (block) push("\n");
    if (/^H[1-6]$/.test(tag)) push("#".repeat(Number(tag[1])) + " ");
    if (tag === "LI") push("- ");
    if (tag === "IMG" && el.alt && !el.closest("a,button")) push(` [image: ${clean(el.alt).slice(0, 80)}] `);
    const k = kind(el);
    if (k) {
      count += 1;
      el.setAttribute("data-rast-ref", String(count));
      let s = ` [${count} ${k} "${name(el)}"`;
      if (k === "field" || k === "list") {
        const v = tag === "SELECT" ? (el.selectedOptions[0] ? el.selectedOptions[0].text : "") : el.isContentEditable ? el.innerText : el.type === "password" ? (el.value ? "••••" : "") : el.value;
        if (v) s += ` = "${clean(v).slice(0, 100).replace(/"/g, "'")}"`;
      }
      if (el.checked || el.getAttribute("aria-checked") === "true" || el.getAttribute("aria-selected") === "true") s += " checked";
      if (el.disabled || el.getAttribute("aria-disabled") === "true") s += " disabled";
      if (tag === "A") {
        const href = el.getAttribute("href") || "";
        if (href && !href.startsWith("#") && !href.toLowerCase().startsWith("javascript:")) s += ` → ${el.href.slice(0, 160)}`;
      }
      push(s + "] ");
      if (k !== "menu item" && k !== "tab" && k !== "option") {
        if (block) push("\n");
        return;
      }
    }
    if (el.shadowRoot) for (const child of el.shadowRoot.childNodes) walk(child);
    for (const child of el.childNodes) walk(child);
    if (block) push("\n");
  };
  walk(document.body || document.documentElement);
  const text = parts.join("").replace(/[ \t]+/g, " ").replace(/ ?\n ?/g, "\n").replace(/\n{3,}/g, "\n\n").trim();
  const root = document.scrollingElement || document.documentElement;
  return { title: document.title, url: location.href, text, cut, elements: count, top: Math.round(root.scrollTop), height: Math.round(root.scrollHeight), view: Math.round(window.innerHeight) };
}"##;

/// The text the model reads after a step.
fn render(view: &Value) -> String {
    let number = |key: &str| view[key].as_f64().unwrap_or(0.0).max(0.0);
    let (top, height, screen) = (number("top"), number("height"), number("view").max(1.0));
    let screens = (height / screen).ceil().max(1.0) as u64;
    // The screen most of the view is on (a scroll moves 85 % of one).
    let at = (((top + screen / 2.0) / screen).floor() as u64 + 1).min(screens);
    let title = view["title"].as_str().unwrap_or("").trim();
    let mut out = format!(
        "Title: {}\nAddress: {}\nScreen {at} of {screens} (the person sees a screenshot of it; the text below is the whole page)\n\n",
        if title.is_empty() { "(none)" } else { title },
        view["url"].as_str().unwrap_or(""),
    );
    let text = view["text"].as_str().unwrap_or("").trim();
    out.push_str(if text.is_empty() {
        "(the page shows no text)"
    } else {
        text
    });
    if view["cut"] == true {
        out.push_str(&format!(
            "\n\n[… the page goes on; this is its first {MAX_TEXT} characters]"
        ));
    }
    out
}

// ------------------------------------------------------------------- proxy

static PROXY: OnceCell<u16> = OnceCell::const_new();

/// The port of RAST's public-only proxy on 127.0.0.1 (started on first use).
async fn proxy_port() -> Result<u16, String> {
    PROXY
        .get_or_try_init(|| async {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(|e| format!("the browser's proxy did not start: {e}"))?;
            let port = listener.local_addr().map_err(|e| e.to_string())?.port();
            tokio::spawn(serve_proxy(listener));
            Ok::<u16, String>(port)
        })
        .await
        .copied()
}

async fn serve_proxy(listener: TcpListener) {
    let room = Arc::new(Semaphore::new(512));
    loop {
        let Ok((client, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(permit) = room.clone().try_acquire_owned() else {
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(Duration::from_secs(3600), proxy_one(client)).await;
        });
    }
}

const PRIVATE: &str = "RAST's browser opens public web addresses only; this one is on this server or a private network.";
const NOT_FOUND: &str = "This address could not be found.";
const UNREACHABLE: &str = "This address did not answer.";
const BAD_PORT: &str = "RAST's browser connects to ports 80, 443 and 1024 and up.";

/// Where a request goes: `host:port` (`[v6]:port`), checked.
fn split_target(target: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        (host, port)
    } else {
        match target.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (target, default_port),
        }
    };
    let fine = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    fine.then(|| (host.to_ascii_lowercase(), port))
}

/// Ports a page may reach: the web's, and none of the low service ports
/// (mail, SSH and the like).
fn port_allowed(port: u16) -> bool {
    port == 80 || port == 443 || port >= 1024
}

/// Connect to `host:port` when every address it has is public.
async fn open_public(host: &str, port: u16) -> Result<TcpStream, &'static str> {
    if !port_allowed(port) {
        return Err(BAD_PORT);
    }
    let found = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| NOT_FOUND)?
    .map_err(|_| NOT_FOUND)?;
    let addresses: Vec<SocketAddr> = found.collect();
    if addresses.is_empty() {
        return Err(NOT_FOUND);
    }
    if addresses.iter().any(|a| !tools::is_public(a.ip())) {
        return Err(PRIVATE);
    }
    for address in addresses {
        if let Ok(Ok(stream)) =
            tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address)).await
        {
            return Ok(stream);
        }
    }
    Err(UNREACHABLE)
}

async fn refuse(client: &mut TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{status}</title><p style=\"font:16px system-ui,sans-serif;margin:2em\">{message}</p>"
    );
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    client.shutdown().await
}

fn status_for(message: &str) -> &'static str {
    if message == NOT_FOUND || message == UNREACHABLE {
        "502 Bad Gateway"
    } else {
        "403 Forbidden"
    }
}

/// One proxy connection: `CONNECT host:port` (HTTPS, WebSockets) or one
/// plain `http://` request (sent on with `Connection: close`).
async fn proxy_one(mut client: TcpStream) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let end = loop {
        let n = tokio::time::timeout(Duration::from_secs(15), client.read(&mut chunk))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
        if let Some(at) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        if head.len() > 64 * 1024 {
            return refuse(
                &mut client,
                "431 Request Header Fields Too Large",
                "Too large.",
            )
            .await;
        }
    };
    let rest = head.split_off(end);
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next())
    else {
        return refuse(&mut client, "400 Bad Request", "Not a request.").await;
    };
    let connect = method.eq_ignore_ascii_case("CONNECT");
    let (host, port, path) = if connect {
        match split_target(target, 443) {
            Some((host, port)) => (host, port, String::new()),
            None => return refuse(&mut client, "400 Bad Request", "Not an address.").await,
        }
    } else {
        let Some(after) = target
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
            .map(|_| &target[7..])
        else {
            return refuse(&mut client, "400 Bad Request", "Not an http address.").await;
        };
        let (authority, path) = match after.find(['/', '?']) {
            Some(at) => (&after[..at], &after[at..]),
            None => (after, "/"),
        };
        let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
        match split_target(authority, 80) {
            Some((host, port)) => {
                let path = if path.starts_with('?') {
                    format!("/{path}")
                } else {
                    path.to_string()
                };
                (host, port, path)
            }
            None => return refuse(&mut client, "400 Bad Request", "Not an address.").await,
        }
    };
    let mut upstream = match open_public(&host, port).await {
        Ok(stream) => stream,
        Err(message) => return refuse(&mut client, status_for(message), message).await,
    };
    if connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else {
        let mut request = format!("{method} {path} {version}\r\n");
        for line in lines.filter(|l| !l.is_empty()) {
            let name = line
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "proxy-connection" | "proxy-authorization" | "connection" | "keep-alive"
            ) {
                continue;
            }
            request.push_str(line);
            request.push_str("\r\n");
        }
        request.push_str("Connection: close\r\n\r\n");
        upstream.write_all(request.as_bytes()).await?;
    }
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_parts(
            None,
            crate::config::FileConfig {
                data_dir: Some("/var/lib/rast".into()),
                storage_dir: Some("/srv/rast".into()),
                ..Default::default()
            },
            |_| None,
        )
        .expect("test config")
    }

    #[test]
    fn chrome_starts_boxed_in() {
        let script = launch_script(
            "/opt/google/chrome/chrome",
            "rast-browser-abc",
            Path::new("/var/lib/rast/browser/abc.sock"),
            41234,
            &config(),
        )
        .unwrap();
        assert!(script.starts_with("exec systemd-run '--quiet'"));
        for part in [
            "'--unit=rast-browser-abc'",
            "'--property=User=nobody'",
            "'--property=NoNewPrivileges=yes'",
            "'--property=ProtectHome=yes'",
            "'--property=IPAddressDeny=any'",
            "'--property=IPAddressAllow=127.0.0.1/32'",
            "'--property=StandardInput=file:/var/lib/rast/browser/abc.sock'",
            "'--property=StandardOutput=file:/var/lib/rast/browser/abc.sock'",
            "InaccessiblePaths=\"-/var/lib/rast\"",
            "InaccessiblePaths=\"-/srv/rast\"",
            "--remote-debugging-pipe",
            "--proxy-server=http://127.0.0.1:41234",
            "--proxy-bypass-list=<-loopback>",
            "--disable-quic",
            "disable_non_proxied_udp",
        ] {
            assert!(script.contains(part), "missing {part} in {script}");
        }
        assert!(!script.contains("no-sandbox"));
        assert!(launch_script(
            "/opt/google/chrome/chrome",
            "u",
            Path::new("/var/lib/my rast/a.sock"),
            1,
            &config()
        )
        .is_err());
    }

    #[test]
    fn steps_read_in_one_line() {
        assert_eq!(
            describe(&json!({"action": "open", "url": "https://example.com"})),
            "open https://example.com"
        );
        assert_eq!(
            describe(&json!({"action": "click", "ref": 12})),
            "click [12]"
        );
        assert_eq!(
            describe(&json!({"action": "type", "ref": "3", "text": "cats", "submit": true})),
            "type \"cats\" into [3] + Enter"
        );
        assert_eq!(
            describe(&json!({"action": "scroll", "direction": "up"})),
            "scroll up"
        );
        assert_eq!(element_ref(&json!({"ref": "[7]"})), Some(7));
    }

    #[test]
    fn web_addresses_only() {
        assert_eq!(
            web_address("example.com/a").unwrap(),
            "https://example.com/a"
        );
        assert_eq!(web_address(" HTTP://x.org ").unwrap(), "HTTP://x.org");
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "data:text/html,hi",
            "",
            "a b",
        ] {
            assert!(web_address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proxy_targets_are_checked() {
        assert_eq!(
            split_target("example.com:443", 443),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            split_target("Example.com", 80),
            Some(("example.com".into(), 80))
        );
        assert_eq!(split_target("[::1]:8080", 443), Some(("::1".into(), 8080)));
        assert_eq!(split_target("[::1]", 443), Some(("::1".into(), 443)));
        assert_eq!(split_target("a b:80", 80), None);
        assert_eq!(split_target("host:http", 80), None);
        assert!(port_allowed(443) && port_allowed(80) && port_allowed(8443));
        assert!(!port_allowed(25) && !port_allowed(22) && !port_allowed(0));
    }

    #[test]
    fn pages_read_as_text() {
        let text = render(&json!({
            "title": "Example", "url": "https://example.com/", "text": "# Hi\n [1 link \"More\" → https://iana.org]",
            "cut": true, "top": 680, "height": 2400, "view": 800,
        }));
        assert!(text.starts_with("Title: Example\nAddress: https://example.com/\nScreen 2 of 3"));
        assert!(text.contains("[1 link \"More\""));
        assert!(text.ends_with("first 16000 characters]"));
    }

    async fn ask(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn the_proxy_keeps_to_the_public_internet() {
        let port = proxy_port().await.unwrap();
        // Something on this server the browser must not reach.
        let inside = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let inside_port = inside.local_addr().unwrap().port();
        for request in [
            format!("CONNECT 127.0.0.1:{inside_port} HTTP/1.1\r\nHost: x\r\n\r\n"),
            format!("CONNECT localhost:{inside_port} HTTP/1.1\r\n\r\n"),
            format!("GET http://127.0.0.1:{inside_port}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
            "GET http://10.1.2.3:8080/admin HTTP/1.1\r\n\r\n".to_string(),
            "CONNECT [::1]:8443 HTTP/1.1\r\n\r\n".to_string(),
            "CONNECT 169.254.169.254:8080 HTTP/1.1\r\n\r\n".to_string(),
        ] {
            let answer = ask(port, &request).await;
            assert!(answer.starts_with("HTTP/1.1 403"), "{request}: {answer}");
            assert!(answer.contains("public web addresses only"), "{answer}");
        }
        // Mail and other low ports, even on public addresses.
        let answer = ask(port, "CONNECT 93.184.215.14:25 HTTP/1.1\r\n\r\n").await;
        assert!(
            answer.starts_with("HTTP/1.1 403") && answer.contains("ports 80, 443"),
            "{answer}"
        );
        let answer = ask(port, "GET https://example.com/ HTTP/1.1\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        drop(inside);
    }
}
