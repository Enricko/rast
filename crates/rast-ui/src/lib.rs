//! RAST's client shell, rendered with [Rune](https://github.com/anomalyco/rune).
//!
//! Runs as a native window on Linux, Windows and macOS, and as a canvas in the
//! browser (`wasm32-unknown-unknown`, Rune's WebGPU target).
//!
//! Two independent things live here:
//!
//! 1. **Device login.** Before the shell opens, the user unlocks it with a
//!    password that belongs to *this device*. The password is stored as an
//!    Argon2id hash in a 0600 file; the plaintext is never stored or sent. This
//!    is the offline/standalone unlock, not server authentication — the server
//!    authenticates users itself when the server phase lands
//!    (see `docs/AUTH-STORAGE.md`).
//! 2. **Server connection.** The shell probes the configured RAST server, keeps
//!    running while it is down and reconnects by itself when it returns. Turning
//!    that off is an optional, persisted setting.

#![forbid(unsafe_code)]

mod auth;
#[cfg(target_arch = "wasm32")]
mod browser;
mod config;
mod connection;
mod desktop;
#[cfg(not(target_arch = "wasm32"))]
mod login;
mod server;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use rune::{AppSpec, TextEdit, Tx, Waker};

use auth::DeviceAccount;
use config::Config;
use connection::{Command, Link, ServerAddr, Status};
use desktop::{AppId, Desktop};

/// Window title shown by the OS window manager and by the browser page.
pub const TITLE: &str = "RAST";

/// Default window size in logical points.
pub const SIZE: (f32, f32) = (1280.0, 800.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// First run on this device: choose a username and a device password.
    CreateAccount,
    /// A device account exists; unlock the shell.
    Login,
    Desktop,
    Settings,
}

/// State of the client shell.
pub struct State {
    pub config: Config,
    pub status: Status,
    pub screen: Screen,
    /// The server field while the settings screen is open.
    pub server_field: TextEdit,
    pub settings_error: Option<String>,
    pub link: Option<Link>,
    pub(crate) wake: Option<Waker>,
    /// Tests construct in-memory states; `false` keeps them off the real files.
    pub(crate) persist: bool,
    // --- device login -----------------------------------------------------
    pub account: Option<DeviceAccount>,
    pub signed_in: bool,
    pub username_field: TextEdit,
    pub password_field: TextEdit,
    pub confirm_field: TextEdit,
    pub reveal_password: bool,
    pub login_error: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) login_dark: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) login_art: Option<login::Art>,
    pub(crate) account_path: Option<PathBuf>,
    pub(crate) desktop: Desktop,
    // --- server account (browser) ------------------------------------------
    /// The account signed in to the RAST server, if any.
    pub(crate) session: Option<server::Account>,
    pub(crate) control: desktop::control::ControlPanel,
    pub(crate) inbox: server::Inbox,
    /// What the server account may open, once the server has said.
    pub(crate) access: Option<server::Access>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self::with_account(config, DeviceAccount::load())
    }

    fn with_account(config: Config, account: Option<DeviceAccount>) -> Self {
        let status = match ServerAddr::parse(&config.server) {
            Ok(_) => Status::connecting(),
            Err(error) => Status::setup_with(error),
        };
        let account_path = DeviceAccount::path();
        let screen = if account.is_some() {
            Screen::Login
        } else {
            Screen::CreateAccount
        };
        let username = account.as_ref().map_or("", |account| &account.username);
        let username_field = TextEdit::new(username);
        Self {
            server_field: TextEdit::new(config.server.clone()),
            config,
            status,
            screen,
            settings_error: None,
            link: None,
            wake: None,
            persist: true,
            account,
            signed_in: false,
            username_field,
            password_field: TextEdit::new("").secure(),
            confirm_field: TextEdit::new("").secure(),
            reveal_password: false,
            login_error: None,
            #[cfg(not(target_arch = "wasm32"))]
            login_dark: true,
            #[cfg(not(target_arch = "wasm32"))]
            login_art: None,
            account_path,
            desktop: Desktop::default(),
            session: None,
            control: desktop::control::ControlPanel::default(),
            inbox: server::Inbox::default(),
            access: None,
        }
    }

    /// The desktop of an account signed in to the RAST server.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn with_session(config: Config, account: server::Account) -> Self {
        let mut state = Self::with_account(config, None);
        state.session = Some(account);
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state
    }

    /// The name the desktop greets: the server account's, else the device login's.
    pub(crate) fn user_name(&self) -> &str {
        match (&self.session, &self.account) {
            (Some(session), _) => session.name(),
            (None, Some(account)) => &account.username,
            (None, None) => "Local user",
        }
    }

    /// "Super Admin", "Admin" or "User" on the server; "Local account" otherwise.
    pub(crate) fn role_label(&self) -> &'static str {
        self.session
            .as_ref()
            .map_or("Local account", |session| session.role.label())
    }

    /// May the signed-in account open `app` (a server app id)? Without a server
    /// session, and for administrators, everything is open. Until the server
    /// answers, nothing is hidden; the server enforces the rules either way.
    pub(crate) fn may_open(&self, app: &str) -> bool {
        match &self.session {
            None => true,
            Some(session) if session.role.is_admin() => true,
            Some(_) => self
                .access
                .as_ref()
                .and_then(|access| access.allows(app))
                .unwrap_or(true),
        }
    }

    /// Ask the server again what this account may open (Users only).
    pub(crate) fn refresh_access(&self) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| !session.role.is_admin())
        {
            self.calls().my_access();
        }
    }

    pub(crate) fn calls(&self) -> server::Calls<'_> {
        server::Calls {
            inbox: &self.inbox,
            wake: self.wake.clone(),
        }
    }

    // --- device login -----------------------------------------------------

    /// First-run: hash the chosen password, store the device account, open the shell.
    pub fn submit_create(&mut self) {
        if self.password_field.text != self.confirm_field.text {
            self.login_error = Some("The passwords do not match.".to_owned());
            return;
        }
        match DeviceAccount::create(&self.username_field.text, &self.password_field.text) {
            Ok(account) => {
                if let Some(path) = &self.account_path
                    && let Err(error) = account.save_to(path)
                {
                    self.login_error = Some(format!("Could not save the device login: {error}"));
                    return;
                }
                self.username_field = TextEdit::new(account.username.clone());
                self.account = Some(account);
                self.signed_in = true;
                self.screen = Screen::Desktop;
                self.login_error = None;
                self.clear_secrets();
            }
            Err(error) => self.login_error = Some(error),
        }
    }

    /// Unlock the shell with the device password.
    pub fn submit_login(&mut self) {
        let Some(account) = self.account.clone() else {
            self.login_error = Some("No device login on this device; create one.".to_owned());
            self.screen = Screen::CreateAccount;
            return;
        };
        let result = if self.username_field.text.trim() == account.username {
            account.verify(&self.password_field.text)
        } else {
            Ok(false)
        };
        match result {
            Ok(true) => {
                self.signed_in = true;
                self.screen = Screen::Desktop;
                self.login_error = None;
                self.clear_secrets();
            }
            Ok(false) => {
                self.login_error = Some("The username or password is incorrect.".to_owned());
                self.clear_secrets();
            }
            Err(error) => self.login_error = Some(error),
        }
    }

    pub fn sign_out(&mut self) {
        if let Some(account) = &self.account {
            self.username_field = TextEdit::new(account.username.clone());
        }
        self.signed_in = false;
        self.screen = Screen::Login;
        self.login_error = None;
        self.desktop.close_session();
        self.clear_secrets();
    }

    pub fn toggle_password_visibility(&mut self) {
        self.reveal_password = !self.reveal_password;
    }

    fn clear_secrets(&mut self) {
        self.password_field = TextEdit::new("").secure();
        self.confirm_field = TextEdit::new("").secure();
        self.reveal_password = false;
    }

    // --- shell ------------------------------------------------------------

    pub fn open_settings(&mut self) {
        self.open_app(AppId::Control);
    }

    pub fn close_settings(&mut self) {
        self.close_app();
        self.settings_error = None;
    }

    /// Validate the typed address, store it and point the prober at it.
    pub fn save_settings(&mut self) {
        match ServerAddr::parse(&self.server_field.text) {
            Ok(addr) => {
                self.config.server = self.server_field.text.trim().to_owned();
                self.persist_config();
                self.close_settings();
                self.status = Status::connecting();
                if let Some(link) = &self.link {
                    link.send(Command::SetServer(addr));
                    link.send(Command::SetAuto(self.config.auto_reconnect));
                } else if let Some(wake) = &self.wake {
                    self.link = connection::spawn(&self.config, wake.clone());
                }
            }
            Err(error) => self.settings_error = Some(error),
        }
    }

    /// Flip automatic reconnection and persist the choice immediately: it is the
    /// setting an operator changes once, so it must survive the app being closed.
    pub fn toggle_auto_reconnect(&mut self) {
        self.config.auto_reconnect = !self.config.auto_reconnect;
        self.persist_config();
        if let Some(link) = &self.link {
            link.send(Command::SetAuto(self.config.auto_reconnect));
        }
    }

    /// "Reconnect now": one immediate attempt, even with automatic reconnection off.
    pub fn retry_now(&mut self) {
        self.status = Status::connecting();
        if let Some(link) = &self.link {
            link.send(Command::RetryNow);
        }
    }

    fn persist_config(&self) {
        if self.persist {
            self.config.save();
        }
    }
}

fn init() -> State {
    State::new(Config::load())
}

fn setup(state: &mut State, wake: Waker) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        state.login_art = Some(login::Art::load(wake.clone()));
    }
    state.desktop.load_art(wake.clone());
    state.wake = Some(wake.clone());
    state.refresh_access();
    state.load_appearance();
    state.link = connection::spawn(&state.config, wake);
}

fn poll(state: &mut State, _t: f64, tx: &mut Tx) -> bool {
    let clock_changed = state.desktop.tick(tx) && state.signed_in;
    #[cfg(target_arch = "wasm32")]
    let clock_changed = (state.desktop.refresh_network() && state.signed_in) || clock_changed;
    let mut latest = None;
    if let Some(link) = state.link.as_ref() {
        while let Ok(status) = link.statuses.try_recv() {
            latest = Some(status);
        }
    }
    let mut replied = false;
    while let Ok(reply) = state.inbox.rx.try_recv() {
        state.apply_reply(reply, tx);
        replied = true;
    }
    match latest {
        Some(status) if status != state.status => {
            state.status = status;
            true
        }
        _ => clock_changed || replied,
    }
}

// --- scenes (headless snapshots) ---------------------------------------------

fn fake_account() -> DeviceAccount {
    DeviceAccount {
        username: "admin".to_owned(),
        phc: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .to_owned(),
    }
}

fn scene_create_account(state: &mut State) {
    state.account = None;
    state.signed_in = false;
    state.screen = Screen::CreateAccount;
}

fn scene_login(state: &mut State) {
    let account = fake_account();
    state.username_field = TextEdit::new(account.username.clone());
    state.account = Some(account);
    state.signed_in = false;
    state.screen = Screen::Login;
}

fn scene_login_error(state: &mut State) {
    scene_login(state);
    state.login_error = Some("The username or password is incorrect.".to_owned());
}

fn scene_offline(state: &mut State) {
    signed_in(state);
    state.status = Status::offline(2, Some(Duration::from_secs(4)), "connection refused");
    state.toggle_desktop_panel(desktop::Panel::Status);
}

fn scene_online(state: &mut State) {
    signed_in(state);
    state.status = Status::online();
}

fn scene_start(state: &mut State) {
    scene_online(state);
    state.toggle_desktop_panel(desktop::Panel::Start);
}

fn scene_account(state: &mut State) {
    scene_online(state);
    state.toggle_desktop_panel(desktop::Panel::Account);
}

fn scene_device(state: &mut State) {
    scene_online(state);
    state.toggle_desktop_panel(desktop::Panel::Device);
}

fn scene_clock(state: &mut State) {
    scene_online(state);
    state.toggle_desktop_panel(desktop::Panel::Clock);
}

fn scene_light(state: &mut State) {
    scene_online(state);
    state.desktop.look.saved.theme = desktop::appearance::Theme::Light;
}

fn scene_dark(state: &mut State) {
    scene_online(state);
    state.desktop.look.saved.theme = desktop::appearance::Theme::Dark;
}

/// Personalize over the desktop, nothing changed yet.
fn scene_personalize(state: &mut State) {
    scene_users(state);
    state.open_personalize();
}

/// Personalize previewing a dark, Plum desktop on Forest Mist.
fn scene_personalize_dark(state: &mut State) {
    use desktop::appearance::{Accent, Theme};
    scene_users(state);
    state.open_personalize();
    state.edit_appearance(|look| {
        look.theme = Theme::Dark;
        look.accent = Accent::Plum;
        look.picture = desktop::Wallpaper::Forest;
        look.overlay = 30;
    });
}

/// Control Panel → Appearance with an unsaved change.
fn scene_appearance(state: &mut State) {
    scene_users(state);
    state.open_control_tab(desktop::control::Tab::Appearance);
    state.edit_appearance(|look| look.accent = desktop::appearance::Accent::Teal);
}

/// A solid colour wallpaper and the desktop's right-click menu.
fn scene_desktop_menu(state: &mut State) {
    scene_online(state);
    let look = &mut state.desktop.look.saved;
    look.kind = desktop::appearance::Kind::Color;
    look.color = "#2E4756".to_owned();
    state.open_desktop_menu([980.0, 220.0]);
}

fn scene_settings(state: &mut State) {
    signed_in(state);
    state.open_settings();
}

fn scene_setup(state: &mut State) {
    signed_in(state);
    state.status = Status::setup_with("the server address is empty");
    state.toggle_desktop_panel(desktop::Panel::Status);
}

fn sample_accounts() -> Vec<server::Account> {
    let account =
        |id, username: &str, display_name: &str, role, disabled, last_login_at| server::Account {
            id,
            username: username.to_owned(),
            display_name: display_name.to_owned(),
            role,
            disabled,
            created_at: 1_762_000_000 + id * 86_400,
            last_login_at,
            must_change_password: false,
            locked_until: None,
            failed_attempts: 0,
            groups: Vec::new(),
        };
    vec![
        account(
            1,
            "root",
            "Server owner",
            server::Role::SuperAdmin,
            false,
            Some(1_763_700_000),
        ),
        account(
            2,
            "ada",
            "Ada Lovelace",
            server::Role::Admin,
            false,
            Some(1_763_600_000),
        ),
        server::Account {
            must_change_password: true,
            groups: vec!["Staff".to_owned()],
            ..account(3, "bob", "", server::Role::User, false, None)
        },
        server::Account {
            groups: vec!["Interns".to_owned(), "Staff".to_owned()],
            ..account(
                4,
                "cat",
                "Cat Rivera",
                server::Role::User,
                true,
                Some(1_763_100_000),
            )
        },
        server::Account {
            locked_until: Some(now_secs() + 11 * 60),
            failed_attempts: 10,
            ..account(
                5,
                "dan",
                "Dan Okafor",
                server::Role::User,
                false,
                Some(1_763_500_000),
            )
        },
    ]
}

fn now_secs() -> i64 {
    rune::time::system_now()
        .duration_since(rune::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Where an account is signed in: this browser, a laptop and a phone.
fn sample_sessions() -> Vec<server::SessionInfo> {
    let now = now_secs();
    [
        ("3f9a", true, "Chrome on Windows", "203.0.113.5", 2 * 3_600),
        (
            "7c21",
            false,
            "Firefox on Linux",
            "198.51.100.9",
            3 * 86_400,
        ),
        ("b604", false, "Safari on iOS", "", 9 * 86_400),
    ]
    .into_iter()
    .map(|(id, current, device, ip, age)| server::SessionInfo {
        id: id.to_owned(),
        current,
        device: device.to_owned(),
        ip: ip.to_owned(),
        created_at: now - age,
        last_seen_at: if current { now } else { now - age / 3 },
    })
    .collect()
}

fn sample_apps() -> Vec<server::AppInfo> {
    [
        ("files", "File Station", true),
        ("monitor", "Resource Monitor", true),
        ("logs", "Activity Logs", true),
        ("controlpanel", "Control Panel", true),
        ("applications", "Docker", false),
        ("backup", "Backup Center", false),
        ("packages", "App Center", false),
    ]
    .into_iter()
    .map(|(id, label, default_allowed)| server::AppInfo {
        id: id.to_owned(),
        label: label.to_owned(),
        default_allowed,
    })
    .collect()
}

fn sample_groups() -> Vec<server::Group> {
    use server::Rule::{Allow, Deny};
    vec![
        server::Group {
            id: 2,
            name: "Interns".to_owned(),
            description: "Short-term accounts".to_owned(),
            member_ids: vec![4],
            apps: [("applications", Deny), ("logs", Deny)]
                .into_iter()
                .map(|(app, rule)| (app.to_owned(), rule))
                .collect(),
        },
        server::Group {
            id: 1,
            name: "Staff".to_owned(),
            description: "Everyone on the team".to_owned(),
            member_ids: vec![3, 4],
            apps: [("applications", Allow), ("backup", Allow)]
                .into_iter()
                .map(|(app, rule)| (app.to_owned(), rule))
                .collect(),
        },
    ]
}

/// What `cat` (in Staff and Interns, with Backup Center denied on the account)
/// may open, as the server would explain it.
fn sample_access() -> server::Access {
    use server::Source;
    let decision = |id: &str, allowed, source, groups: &[&str], overridden| {
        let label = sample_apps()
            .into_iter()
            .find(|app| app.id == id)
            .map_or_else(String::new, |app| app.label);
        server::Decision {
            id: id.to_owned(),
            label,
            allowed,
            source,
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            overridden,
        }
    };
    server::Access {
        groups: vec![
            server::GroupRef {
                id: 2,
                name: "Interns".to_owned(),
            },
            server::GroupRef {
                id: 1,
                name: "Staff".to_owned(),
            },
        ],
        group_ids: vec![1, 2],
        rules: [("backup".to_owned(), server::Rule::Deny)]
            .into_iter()
            .collect(),
        apps: vec![
            decision("files", true, Source::Default, &[], false),
            decision("monitor", true, Source::Default, &[], false),
            decision("logs", false, Source::Group, &["Interns"], false),
            decision("controlpanel", true, Source::Default, &[], false),
            decision("applications", false, Source::Group, &["Interns"], true),
            decision("backup", false, Source::Account, &[], true),
            decision("packages", false, Source::Default, &[], false),
        ],
    }
}

/// A Super Admin signed in to the server with Control Panel open on Users.
fn scene_users(state: &mut State) {
    let accounts = sample_accounts();
    state.session = Some(accounts[0].clone());
    state.signed_in = true;
    state.screen = Screen::Desktop;
    state.open_app(AppId::Control);
    state.open_control_tab(desktop::control::Tab::Users);
    while state.inbox.rx.try_recv().is_ok() {}
    state.control.busy = false;
    state.control.loaded = true;
    state.control.accounts = accounts;
}

/// The users table in the dark.
fn scene_users_dark(state: &mut State) {
    scene_users(state);
    state.desktop.look.saved.theme = desktop::appearance::Theme::Dark;
}

/// A row's actions menu, open on a locked account the Super Admin manages.
fn scene_user_menu(state: &mut State) {
    scene_users(state);
    state.open_row_actions(5);
}

fn scene_user(state: &mut State) {
    scene_users(state);
    state.open_account(2);
    while state.inbox.rx.try_recv().is_ok() {}
    state.control.account_sessions = Some((2, sample_sessions()[1..].to_vec()));
    state.toggle_app_maximized();
}

/// An account locked after wrong passwords, as its administrator sees it.
fn scene_locked_user(state: &mut State) {
    scene_users(state);
    state.open_account(5);
    while state.inbox.rx.try_recv().is_ok() {}
    state.control.account_sessions = Some((5, Vec::new()));
    state.toggle_app_maximized();
}

fn scene_create_user(state: &mut State) {
    scene_users(state);
    state.start_create_account();
}

fn scene_profile(state: &mut State) {
    scene_users(state);
    state.open_control_tab(desktop::control::Tab::Profile);
    while state.inbox.rx.try_recv().is_ok() {}
    state.control.sessions = Some(sample_sessions());
    state.toggle_app_maximized();
}

fn with_groups(state: &mut State) {
    while state.inbox.rx.try_recv().is_ok() {}
    state.control.groups = sample_groups();
    state.control.groups.sort_by_key(|g| g.name.to_lowercase());
    state.control.apps = sample_apps();
    state.control.groups_loaded = true;
}

fn scene_groups(state: &mut State) {
    scene_users(state);
    state.open_control_tab(desktop::control::Tab::Groups);
    with_groups(state);
}

/// A section this server doesn't offer yet, in the dark.
fn scene_storage(state: &mut State) {
    scene_users(state);
    state.desktop.look.saved.theme = desktop::appearance::Theme::Dark;
    state.open_control_tab(desktop::control::Tab::Storage);
}

fn scene_group(state: &mut State) {
    scene_groups(state);
    state.open_group(1);
    state.toggle_app_maximized();
}

fn scene_user_access(state: &mut State) {
    scene_users(state);
    state.open_access(4);
    with_groups(state);
    state.control.access = Some((4, sample_access()));
    state.toggle_app_maximized();
}

/// A User (cat) whose groups deny some apps, on Profile.
fn scene_my_apps(state: &mut State) {
    let accounts = sample_accounts();
    state.session = Some(server::Account {
        disabled: false,
        ..accounts[3].clone()
    });
    state.access = Some(sample_access());
    state.signed_in = true;
    state.screen = Screen::Desktop;
    state.open_app(AppId::Control);
    while state.inbox.rx.try_recv().is_ok() {}
}

fn signed_in(state: &mut State) {
    state.account = Some(fake_account());
    state.signed_in = true;
    state.screen = Screen::Desktop;
}

/// Mount the native shell. Browser login mounts Rune separately after unlock.
#[cfg(not(target_arch = "wasm32"))]
pub fn start() {
    rune::App::new(app_spec())
        .font_smoothing(font_smoothing())
        .run();
}

/// macOS font smoothing thickens light text on dark surfaces, so the desktop's
/// labels look bold. Draw them as `-webkit-font-smoothing: antialiased` does there;
/// other platforms keep Rune's default.
pub(crate) fn font_smoothing() -> rune::FontSmoothing {
    match rune::FontSmoothing::default() {
        rune::FontSmoothing::On => rune::FontSmoothing::Off,
        other => other,
    }
}

fn app_spec() -> AppSpec<State> {
    AppSpec::<State> {
        min_size: Some((720.0, 480.0)),
        scenes: &[
            ("create-account", scene_create_account),
            ("login", scene_login),
            ("login-error", scene_login_error),
            ("offline", scene_offline),
            ("online", scene_online),
            ("start", scene_start),
            ("account", scene_account),
            ("device", scene_device),
            ("clock", scene_clock),
            ("light", scene_light),
            ("dark", scene_dark),
            ("personalize", scene_personalize),
            ("personalize-dark", scene_personalize_dark),
            ("appearance", scene_appearance),
            ("desktop-menu", scene_desktop_menu),
            ("settings", scene_settings),
            ("setup", scene_setup),
            ("users", scene_users),
            ("users-dark", scene_users_dark),
            ("user-menu", scene_user_menu),
            ("user", scene_user),
            ("locked-user", scene_locked_user),
            ("create-user", scene_create_user),
            ("profile", scene_profile),
            ("groups", scene_groups),
            ("group", scene_group),
            ("user-access", scene_user_access),
            ("my-apps", scene_my_apps),
            ("storage", scene_storage),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(desktop::key),
        ..AppSpec::new(TITLE, SIZE, init, ui::view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Phase;
    use rune::{Env, Runtime};

    fn test_env() -> Env {
        Env {
            system_dark: true,
            scale: 2.0,
        }
    }

    fn state_with(status: Status) -> State {
        let mut state = State::new(Config::default());
        state.status = status;
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state.persist = false;
        state
    }

    /// First run: no device account, so the shell starts on the create screen.
    fn state_for_creation() -> State {
        let mut state = State::new(Config::default());
        state.account = None;
        state.signed_in = false;
        state.screen = Screen::CreateAccount;
        state.persist = false;
        state.account_path = Some(
            std::env::temp_dir()
                .join(format!("rast-ui-test-{}", std::process::id()))
                .join("device-account"),
        );
        state
    }

    fn frame_until_settled(rt: &mut Runtime<State>) {
        for _ in 0..8 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt
            .find(key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        frame_until_settled(rt);
    }

    #[test]
    fn a_new_device_creates_an_account_and_opens_the_shell() {
        let path = std::env::temp_dir().join(format!(
            "rast-ui-create-{}/device-account",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut rt = Runtime::new(state_for_creation(), ui::view, SIZE, test_env());
        rt.state.account_path = Some(path.clone());
        rt.frame(0.0);

        rt.state.username_field.text = "admin".to_owned();
        rt.state.password_field.text = "correct horse".to_owned();
        rt.state.confirm_field.text = "correct horse".to_owned();
        press(&mut rt, "create-account");

        assert!(rt.state.signed_in, "creating an account opens the shell");
        assert_eq!(rt.state.screen, Screen::Desktop);
        assert!(path.exists(), "the device account is saved on this device");
        assert_eq!(
            DeviceAccount::load_from(&path)
                .expect("the account loads")
                .username,
            "admin"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_login_screen_rejects_a_wrong_password_and_accepts_the_right_one() {
        let mut state = state_with(Status::online());
        state.account = Some(DeviceAccount::create("admin", "correct horse").unwrap());
        state.signed_in = false;
        state.screen = Screen::Login;
        state.username_field = TextEdit::new("admin");
        let mut rt = Runtime::new(state, ui::view, SIZE, test_env());
        rt.frame(0.0);

        rt.state.password_field.text = "wrong horse".to_owned();
        press(&mut rt, "sign-in");
        assert!(!rt.state.signed_in);
        assert!(rt.state.login_error.is_some());

        rt.state.username_field.text = "wrong user".to_owned();
        rt.state.password_field.text = "correct horse".to_owned();
        press(&mut rt, "sign-in");
        assert!(!rt.state.signed_in);

        rt.state.username_field.text = "admin".to_owned();
        rt.state.password_field.text = "correct horse".to_owned();
        press(&mut rt, "sign-in");
        assert!(rt.state.signed_in);
        assert_eq!(rt.state.screen, Screen::Desktop);
    }

    #[test]
    fn signing_out_keeps_the_device_account_and_returns_to_the_login_screen() {
        let mut state = state_with(Status::online());
        state.account = Some(DeviceAccount::create("admin", "correct horse").unwrap());
        let mut rt = Runtime::new(state, ui::view, SIZE, test_env());
        rt.frame(0.0);
        press(&mut rt, "desktop-account");
        press(&mut rt, "sign-out");
        assert!(!rt.state.signed_in);
        assert_eq!(rt.state.screen, Screen::Login);
        assert!(
            rt.state.account.is_some(),
            "signing out does not delete the account"
        );
    }

    #[test]
    fn a_mismatched_confirmation_does_not_create_an_account() {
        let mut rt = Runtime::new(state_for_creation(), ui::view, SIZE, test_env());
        rt.frame(0.0);
        rt.state.username_field.text = "admin".to_owned();
        rt.state.password_field.text = "correct horse".to_owned();
        rt.state.confirm_field.text = "different horse".to_owned();
        press(&mut rt, "create-account");
        assert!(!rt.state.signed_in);
        assert!(rt.state.login_error.is_some());
    }

    #[test]
    fn settings_opens_toggles_and_cancels() {
        let mut rt = Runtime::new(state_with(Status::online()), ui::view, SIZE, test_env());
        rt.frame(0.0);
        press(&mut rt, "desktop-status");
        press(&mut rt, "status-control");
        assert_eq!(rt.state.screen, Screen::Settings);

        assert!(rt.state.config.auto_reconnect);
        press(&mut rt, "auto-reconnect");
        assert!(!rt.state.config.auto_reconnect, "the switch turns it off");
        press(&mut rt, "auto-reconnect");
        assert!(rt.state.config.auto_reconnect, "and on again");

        press(&mut rt, "cancel");
        assert_eq!(rt.state.screen, Screen::Desktop);
    }

    #[test]
    fn apply_with_a_bad_address_stays_in_settings_and_explains() {
        let mut rt = Runtime::new(state_with(Status::online()), ui::view, SIZE, test_env());
        rt.frame(0.0);
        press(&mut rt, "desktop-status");
        press(&mut rt, "status-control");
        rt.state.server_field.text = "not a host name".to_owned();
        press(&mut rt, "apply");
        assert_eq!(rt.state.screen, Screen::Settings);
        assert!(rt.state.settings_error.is_some());
    }

    #[test]
    fn reconnect_now_in_offline_state_returns_to_connecting() {
        let mut rt = Runtime::new(
            state_with(Status::offline(
                3,
                Some(Duration::from_secs(8)),
                "timed out",
            )),
            ui::view,
            SIZE,
            test_env(),
        );
        rt.frame(0.0);
        rt.state.retry_now();
        assert_eq!(rt.state.status.phase, Phase::Connecting);
    }
}
