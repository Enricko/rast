//! The client's screens: device login, account creation, the shell status card
//! and the settings form.

use rune::tok::{self, motion, type_};
use rune::{Cx, El, col, el, row, text, text_input};

use crate::State;

pub fn view(state: &State, cx: &mut Cx<State>) -> El {
    if state.signed_in {
        return crate::desktop::view(state, cx);
    }
    #[cfg(not(target_arch = "wasm32"))]
    return crate::login::view(state, cx);
    #[cfg(target_arch = "wasm32")]
    el().w_full().h_full()
}

pub(crate) fn settings_body(state: &State) -> El {
    let dark = state.desktop.is_dark();
    let t = &tok::theme(dark);
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
            text(if cfg!(target_arch = "wasm32") { "The browser preview does not probe the server. Reconnect is available in the native client." } else { "While the server is down, RAST stays open and reconnects by itself. Turn this off to wait for Reconnect." }).w_full().wrap()
                .font(type_::LABEL)
                .color(t.text_secondary),
        );
    if let Some(error) = &state.settings_error {
        card = card.child(
            text(error.clone())
                .w_full()
                .wrap()
                .font(type_::LABEL)
                .color(tok::accent(2, dark)),
        );
    }
    card = card.child(
        row()
            .gap(8.0)
            .child(button(t, "apply", "Apply", true, State::save_settings))
            .child(button(t, "cancel", "Cancel", false, State::close_settings)),
    );

    col().w_full().padding(12.0).child(card)
}

pub(crate) fn bind_sign_out(button: El) -> El {
    #[cfg(not(target_arch = "wasm32"))]
    return button.on_press(State::sign_out);
    #[cfg(target_arch = "wasm32")]
    button.on_press_tx(crate::browser::sign_out)
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
