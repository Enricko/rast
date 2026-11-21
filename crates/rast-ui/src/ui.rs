//! The client's screens: device login, account creation, the shell status card
//! and the settings form.

use rune::tok::{self, motion, type_};
use rune::{Color, Cx, El, col, el, row, text, text_input};

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
pub fn view(state: &State, cx: &mut Cx<State>) -> El {
    let dark = cx.system_dark();
    let t = tok::theme(dark);
    match state.screen {
        Screen::Desktop if state.signed_in => desktop(state, &t),
        Screen::Settings if state.signed_in => settings(state, dark, &t),
        #[cfg(not(target_arch = "wasm32"))]
        _ => crate::login::view(state, cx),
        #[cfg(target_arch = "wasm32")]
        _ => el().w_full().h_full().fill(t.bg),
    }
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
                .child(sign_out_button(t))
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

fn sign_out_button(t: &tok::Theme) -> El {
    let button = button_look(t, "sign-out", "Sign out", false);
    #[cfg(not(target_arch = "wasm32"))]
    let button = button.on_press(State::sign_out);
    #[cfg(target_arch = "wasm32")]
    let button = button.on_press_tx(crate::browser::sign_out);
    button
}

fn button(
    t: &tok::Theme,
    key: &'static str,
    label: &'static str,
    primary: bool,
    on_press: fn(&mut State),
) -> El {
    button_look(t, key, label, primary).on_press(on_press)
}

fn button_look(t: &tok::Theme, key: &'static str, label: &'static str, primary: bool) -> El {
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
