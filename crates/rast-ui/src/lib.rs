//! RAST's client shell, rendered with [Rune](https://github.com/anomalyco/rune).
//!
//! Runs as a native window on Linux, Windows and macOS, and as a canvas in the
//! browser (`wasm32-unknown-unknown`, Rune's WebGPU target).
//!
//! The shell is resilient to the server being away: it keeps running, probes the
//! configured RAST server on a backoff, and turns back into the connected state by
//! itself as soon as the server answers again. That behaviour is on by default and
//! can be turned off in Settings (or per run with `RAST_AUTO_RECONNECT=0`).

#![forbid(unsafe_code)]

mod config;
mod connection;
mod ui;

use std::time::Duration;

use rune::{AppSpec, TextEdit, Tx, Waker};

use config::Config;
use connection::{Command, Link, Phase, ServerAddr, Status};

/// Window title shown by the OS window manager and by the browser page.
pub const TITLE: &str = "RAST";

/// Default window size in logical points.
pub const SIZE: (f32, f32) = (1280.0, 800.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
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
    /// Tests construct in-memory states; `false` keeps them off the real config file.
    pub(crate) persist: bool,
}

impl State {
    pub fn new(config: Config) -> Self {
        // In the browser build there is no socket to probe and no server API yet,
        // so the shell says what it is instead of pretending to connect forever.
        #[cfg(target_arch = "wasm32")]
        let status = Status::setup_with(
            "Browser preview: this is the RAST client shell. The server connection and the desktop arrive in the next phase.",
        );
        #[cfg(not(target_arch = "wasm32"))]
        let status = match ServerAddr::parse(&config.server) {
            Ok(_) => Status::connecting(),
            Err(error) => Status::setup_with(error),
        };
        Self {
            server_field: TextEdit::new(config.server.clone()),
            config,
            status,
            screen: Screen::Desktop,
            settings_error: None,
            link: None,
            persist: true,
        }
    }

    pub fn open_settings(&mut self) {
        self.screen = Screen::Settings;
        self.server_field = TextEdit::new(self.config.server.clone());
        self.settings_error = None;
    }

    pub fn close_settings(&mut self) {
        self.screen = Screen::Desktop;
        self.settings_error = None;
    }

    /// Validate the typed address, store it and point the prober at it.
    pub fn save_settings(&mut self) {
        match ServerAddr::parse(&self.server_field.text) {
            Ok(addr) => {
                self.config.server = self.server_field.text.trim().to_owned();
                self.persist_config();
                self.settings_error = None;
                self.screen = Screen::Desktop;
                self.status = Status::connecting();
                if let Some(link) = &self.link {
                    link.send(Command::SetServer(addr));
                    link.send(Command::SetAuto(self.config.auto_reconnect));
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
    if state.status.phase == Phase::Setup {
        return;
    }
    state.link = connection::spawn(&state.config, wake);
}

fn poll(state: &mut State, _t: f64, _tx: &mut Tx) -> bool {
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
        _ => false,
    }
}

fn scene_offline(state: &mut State) {
    state.status = Status::offline(2, Some(Duration::from_secs(4)), "connection refused");
}

fn scene_online(state: &mut State) {
    state.status = Status::online();
}

fn scene_settings(state: &mut State) {
    state.open_settings();
}

fn scene_setup(state: &mut State) {
    state.status = Status::setup_with("the server address is empty");
}

/// Mount the shell: a native window on desktop targets, a canvas on the page in
/// the browser (`wasm32-unknown-unknown`).
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(start))]
pub fn start() {
    rune::run(AppSpec::<State> {
        min_size: Some((720.0, 480.0)),
        scenes: &[
            ("offline", scene_offline),
            ("online", scene_online),
            ("settings", scene_settings),
            ("setup", scene_setup),
        ],
        setup: Some(setup),
        poll: Some(poll),
        ..AppSpec::new(TITLE, SIZE, init, ui::view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    fn state_with(status: Status) -> State {
        let mut state = State::new(Config::default());
        state.status = status;
        state.persist = false;
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
    fn settings_opens_toggles_and_cancels() {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut rt = Runtime::new(
            state_with(Status::offline(1, Some(Duration::from_secs(1)), "refused")),
            ui::view,
            SIZE,
            env,
        );
        rt.frame(0.0);
        press(&mut rt, "open-settings");
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
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut rt = Runtime::new(state_with(Status::online()), ui::view, SIZE, env);
        rt.frame(0.0);
        press(&mut rt, "open-settings");
        rt.state.server_field.text = "not a host name".to_owned();
        press(&mut rt, "apply");
        assert_eq!(rt.state.screen, Screen::Settings);
        assert!(rt.state.settings_error.is_some());
    }

    #[test]
    fn reconnect_now_in_offline_state_returns_to_connecting() {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut rt = Runtime::new(
            state_with(Status::offline(
                3,
                Some(Duration::from_secs(8)),
                "timed out",
            )),
            ui::view,
            SIZE,
            env,
        );
        rt.frame(0.0);
        press(&mut rt, "reconnect");
        assert_eq!(rt.state.status.phase, Phase::Connecting);
    }
}
