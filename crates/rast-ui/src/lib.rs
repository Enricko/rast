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
    wake: Option<Waker>,
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
    match latest {
        Some(status) if status != state.status => {
            state.status = status;
            true
        }
        _ => clock_changed,
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
    state.toggle_desktop_theme();
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
            ("settings", scene_settings),
            ("setup", scene_setup),
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
