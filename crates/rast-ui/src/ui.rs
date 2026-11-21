//! The client's screens: device login, account creation, the shell status card
//! and the settings form.

use rune::tok::{self, motion, type_};
use rune::{Color, Cx, El, Tx, col, el, row, text, text_input};

use crate::connection::Phase;
use crate::{Screen, State};

const ONLINE: Color = Color {
    r: 0.18,
    g: 0.72,
    b: 0.45,
    a: 1.0,
};
const OFFLINE: Color = Color {
    r: 0.92,
    g: 0.33,
    b: 0.33,
    a: 1.0,
};
const WAITING: Color = Color {
    r: 0.95,
    g: 0.68,
    b: 0.25,
    a: 1.0,
};
#[cfg(not(target_arch = "wasm32"))]
const HINT: &str = "Device login: the password is stored on this device as an Argon2id hash, never in plain text. Server accounts arrive with the server phase.";
#[cfg(target_arch = "wasm32")]
const HINT: &str = "Device login: the password is stored in this browser as an Argon2id hash, never in plain text. Server accounts arrive with the server phase.";

pub fn view(state: &State, cx: &mut Cx<State>) -> El {
    let dark = cx.system_dark();
    let t = tok::theme(dark);
    match state.screen {
        Screen::CreateAccount => create_account(state, &t),
        Screen::Login => login(state, &t),
        Screen::Desktop if state.signed_in => desktop(state, &t),
        Screen::Settings if state.signed_in => settings(state, dark, &t),
        Screen::Desktop | Screen::Settings => login(state, &t),
    }
}

// --- device login -------------------------------------------------------------

fn create_account(state: &State, t: &tok::Theme) -> El {
    let username = text_input(&state.username_field, type_::BODY)
        .key("username")
        .single_line()
        .h(42.0)
        .padding(10.0)
        .radius(9.0)
        .fill(t.control_off)
        .color(t.text)
        .placeholder("username", t.text_secondary)
        .on_edit(|state: &mut State| &mut state.username_field);

    let mut password = text_input(&state.password_field, type_::BODY)
        .key("password")
        .single_line()
        .h(42.0)
        .padding(10.0)
        .radius(9.0)
        .fill(t.control_off)
        .color(t.text)
        .placeholder("device password (8+ characters)", t.text_secondary)
        .on_edit(|state: &mut State| &mut state.password_field);
    if !state.reveal_password {
        password = password.secure();
    }

    let mut confirm = text_input(&state.confirm_field, type_::BODY)
        .key("confirm")
        .single_line()
        .h(42.0)
        .padding(10.0)
        .radius(9.0)
        .fill(t.control_off)
        .color(t.text)
        .placeholder("repeat the password", t.text_secondary)
        .on_edit(|state: &mut State| &mut state.confirm_field)
        .on_submit(|state: &mut State, _tx: &mut Tx| state.submit_create());
    if !state.reveal_password {
        confirm = confirm.secure();
    }

    let mut card = auth_card(t)
        .child(text("Welcome to RAST").font(type_::TITLE).color(t.text))
        .child(
            text(if cfg!(target_arch = "wasm32") {
                "Set up the login for this browser."
            } else {
                "Set up the login for this device."
            })
            .font(type_::BODY)
            .color(t.text_secondary),
        )
        .child(labeled(t, "Username", username))
        .child(labeled(t, "Device password", password))
        .child(labeled(t, "Confirm password", confirm))
        .child(show_password_button(state, t));
    card = with_error(card, state);
    card = card
        .child(button(
            t,
            "create-account",
            "Create device login",
            true,
            State::submit_create,
        ))
        .child(
            text(HINT)
                .w(384.0)
                .wrap()
                .font(type_::LABEL)
                .color(t.text_secondary),
        );

    centered(card, t)
}

fn login(state: &State, t: &tok::Theme) -> El {
    let username = match &state.account {
        Some(account) => account.username.clone(),
        None => "no device login".to_owned(),
    };

    let mut password = text_input(&state.password_field, type_::BODY)
        .key("password")
        .single_line()
        .h(42.0)
        .padding(10.0)
        .radius(9.0)
        .fill(t.control_off)
        .color(t.text)
        .placeholder("password", t.text_secondary)
        .on_edit(|state: &mut State| &mut state.password_field)
        .on_submit(|state: &mut State, _tx: &mut Tx| state.submit_login());
    if !state.reveal_password {
        password = password.secure();
    }

    let mut card = auth_card(t)
        .child(text("RAST").font(type_::TITLE).color(t.text))
        .child(
            text("Sign in to your workspace.")
                .font(type_::BODY)
                .color(t.text_secondary),
        )
        .child(
            row().w_full().center_y().child(
                text(format!("Device account: {username}"))
                    .font(type_::LABEL)
                    .color(t.text_secondary)
                    .grow(1.0),
            ),
        )
        .child(labeled(t, "Password", password))
        .child(show_password_button(state, t));
    card = with_error(card, state);
    card = card
        .child(button(t, "sign-in", "Sign in", true, State::submit_login))
        .child(
            text(HINT)
                .w(384.0)
                .wrap()
                .font(type_::LABEL)
                .color(t.text_secondary),
        );

    centered(card, t)
}

fn auth_card(t: &tok::Theme) -> El {
    col()
        .key("auth-card")
        .w(440.0)
        .gap(14.0)
        .padding(28.0)
        .radius(16.0)
        .fill(t.card)
}

fn centered(card: El, t: &tok::Theme) -> El {
    col()
        .w_full()
        .h_full()
        .fill(t.bg)
        .center()
        .padding(32.0)
        .child(card)
}

fn labeled(t: &tok::Theme, label: &'static str, input: El) -> El {
    col()
        .w_full()
        .gap(6.0)
        .child(text(label).font(type_::LABEL).color(t.text_secondary))
        .child(input)
}

fn show_password_button(state: &State, t: &tok::Theme) -> El {
    let (key, label) = if state.reveal_password {
        ("hide-password", "Hide password")
    } else {
        ("show-password", "Show password")
    };
    button(t, key, label, false, State::toggle_password_visibility)
}

fn with_error(mut card: El, state: &State) -> El {
    if let Some(error) = &state.login_error {
        card = card.child(text(error.clone()).font(type_::LABEL).color(OFFLINE));
    }
    card
}

// --- shell --------------------------------------------------------------------

fn desktop(state: &State, t: &tok::Theme) -> El {
    let (dot, headline, detail, retry) =
        match state.status.phase {
            Phase::Setup => (
                WAITING,
                "Set up the server",
                state.status.error.clone().unwrap_or_else(|| {
                    "Open Settings and enter the RAST server address.".to_owned()
                }),
                None,
            ),
            Phase::Connecting => (
                WAITING,
                "Connecting…",
                format!("Contacting {}.", state.config.server),
                None,
            ),
            Phase::Online => (
                ONLINE,
                "Connected",
                format!("Connected to {}.", state.config.server),
                None,
            ),
            Phase::Offline => (
                OFFLINE,
                "Server unreachable",
                state
                    .status
                    .error
                    .clone()
                    .unwrap_or_else(|| "The server did not answer.".to_owned()),
                retry_line(state),
            ),
        };

    let mut card = col()
        .key("status-card")
        .w_full()
        .gap(12.0)
        .padding(24.0)
        .radius(16.0)
        .fill(t.card)
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(el().size(10.0, 10.0).radius(5.0).fill(dot))
                .child(text(headline).font(type_::TITLE).color(t.text)),
        )
        .child(text(detail).font(type_::BODY).color(t.text_secondary));
    if let Some(retry) = retry {
        card = card.child(text(retry).font(type_::LABEL).color(t.text_secondary));
    }
    if state.status.phase == Phase::Offline {
        card = card.child(row().gap(8.0).child(button(
            t,
            "reconnect",
            "Reconnect now",
            true,
            State::retry_now,
        )));
    }

    let footer = if state.config.auto_reconnect {
        "RAST keeps running while the server is down and reconnects by itself."
    } else {
        "Automatic reconnection is off: press Reconnect when the server is back."
    };
    let who = state
        .account
        .as_ref()
        .map(|account| format!("Signed in as {}", account.username))
        .unwrap_or_else(|| "Signed in".to_owned());

    col()
        .w_full()
        .h_full()
        .fill(t.bg)
        .padding(32.0)
        .gap(16.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(text("RAST").font(type_::TITLE).color(t.text).grow(1.0))
                .child(text(who).font(type_::LABEL).color(t.text_secondary))
                .child(button(t, "sign-out", "Sign out", false, State::sign_out))
                .child(button(
                    t,
                    "open-settings",
                    "Settings",
                    false,
                    State::open_settings,
                )),
        )
        .child(card)
        .child(text(footer).font(type_::LABEL).color(t.text_secondary))
}

/// The "retrying in Ns" line, when the client is waiting for its next attempt.
fn retry_line(state: &State) -> Option<String> {
    match (state.config.auto_reconnect, state.status.retry_in) {
        (true, Some(delay)) => Some(format!(
            "Retrying in {}s (attempt {}).",
            delay.as_secs().max(1),
            state.status.attempts
        )),
        (false, _) => Some("Automatic reconnection is off.".to_owned()),
        _ => None,
    }
}

fn settings(state: &State, dark: bool, t: &tok::Theme) -> El {
    let field = text_input(&state.server_field, type_::BODY)
        .key("server-field")
        .single_line()
        .h(42.0)
        .padding(10.0)
        .radius(9.0)
        .fill(t.control_off)
        .color(t.text)
        .placeholder("desktop.lufria.tech:443", t.text_secondary)
        .on_edit(|state: &mut State| &mut state.server_field)
        .on_submit(|state: &mut State, _tx| state.save_settings());

    let toggle = row()
        .w_full()
        .gap(12.0)
        .center_y()
        .child(
            text("Reconnect automatically")
                .font(type_::BODY)
                .color(t.text)
                .grow(1.0),
        )
        .child(switch(
            dark,
            t,
            "auto-reconnect",
            "Reconnect automatically",
            state.config.auto_reconnect,
            State::toggle_auto_reconnect,
        ));

    let mut card = col()
        .key("settings-card")
        .w_full()
        .gap(14.0)
        .padding(24.0)
        .radius(16.0)
        .fill(t.card)
        .child(text("Settings").font(type_::TITLE).color(t.text))
        .child(text("Server").font(type_::LABEL).color(t.text_secondary))
        .child(field)
        .child(toggle)
        .child(
            text("While the server is down, RAST stays open and reconnects by itself. Turn this off to wait for Reconnect. RAST_AUTO_RECONNECT=0 does the same for one run.")
                .font(type_::LABEL)
                .color(t.text_secondary),
        );
    if let Some(error) = &state.settings_error {
        card = card.child(text(error.clone()).font(type_::LABEL).color(OFFLINE));
    }
    card = card.child(
        row()
            .gap(8.0)
            .child(button(t, "apply", "Apply", true, State::save_settings))
            .child(button(t, "cancel", "Cancel", false, State::close_settings)),
    );

    col().w_full().h_full().fill(t.bg).padding(32.0).child(card)
}

fn button(
    t: &tok::Theme,
    key: &'static str,
    label: &'static str,
    primary: bool,
    on_press: fn(&mut State),
) -> El {
    let fill = if primary {
        t.control_track
    } else {
        t.control_off
    };
    let color = if primary { t.text } else { t.text_secondary };
    el().key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .h(38.0)
        .px(16.0)
        .center()
        .radius(9.0)
        .fill(fill)
        .motion(motion::SNAPPY)
        .child(text(label).font(type_::LABEL).color(color))
        .on_press(on_press)
}

fn switch(
    dark: bool,
    t: &tok::Theme,
    key: &'static str,
    label: &'static str,
    on: bool,
    on_press: fn(&mut State),
) -> El {
    let knob = el()
        .size(18.0, 18.0)
        .radius(9.0)
        .fill(t.knob)
        .absolute(if on { 18.0 } else { 2.0 }, 2.0);
    el().key(key)
        .role("switch")
        .label(label)
        .checked(on)
        .focusable(true)
        .size(38.0, 22.0)
        .radius(11.0)
        .fill(if on {
            tok::accent(0, dark)
        } else {
            t.control_off
        })
        .motion(motion::SNAPPY)
        .on_press(on_press)
        .child(knob)
}
