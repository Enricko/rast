//! RAST's desktop shell, rendered with [Rune](https://github.com/anomalyco/rune).
//!
//! This is the first slice of the rewrite: one window with the RAST identity and
//! a small interactive element, exercised headlessly by the test at the bottom.
//! The desktop model (window manager, launcher, sessions, apps) and the server
//! grow around this crate, one reviewed step at a time.

#![forbid(unsafe_code)]

use rune::tok::{self, motion, type_};
use rune::{AppSpec, Cx, El, col, el, text};

/// Window title shown by the OS window manager and by the browser page.
pub const TITLE: &str = "RAST";

/// Default window size in logical points.
pub const SIZE: (f32, f32) = (1280.0, 800.0);

/// State of the desktop shell.
///
/// Deliberately almost empty: the window manager, launcher and session model
/// land here as the rewrite grows.
#[derive(Default)]
pub struct State {
    /// Windows the user opened from the placeholder button.
    pub opened: u32,
}

fn view(state: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(cx.system_dark());

    let heading = text(TITLE).font(type_::TITLE).color(t.text);
    let tagline = text("Linux web desktop, rebuilt from scratch on Rune")
        .font(type_::BODY)
        .color(t.text_secondary);
    let counter = text(match state.opened {
        0 => "No windows yet".to_string(),
        1 => "1 window open".to_string(),
        n => format!("{n} windows open"),
    })
    .font(type_::LABEL)
    .color(t.text_secondary);

    // Not a real window yet: the card is the shell's first element and the thing
    // the window manager will move, resize and stack in the next steps.
    let card = col()
        .key("shell-card")
        .role("region")
        .label("RAST shell")
        .w(560.0)
        .gap(12.0)
        .padding(28.0)
        .radius(16.0)
        .fill(t.card)
        .child(heading)
        .child(tagline)
        .child(counter)
        .child(
            el().key("open-window")
                .role("button")
                .label("Open a window")
                .focusable(true)
                .h(40.0)
                .px(18.0)
                .center()
                .radius(10.0)
                .fill(t.control_off)
                .motion(motion::SNAPPY)
                .child(text("Open a window").font(type_::LABEL).color(t.text))
                .on_press(|state: &mut State| state.opened += 1),
        );

    col()
        .w_full()
        .h_full()
        .center()
        .padding(32.0)
        .fill(t.bg)
        .child(card)
}

/// Mount the shell: a native window on desktop targets, a canvas on the page
/// in the browser (`wasm32-unknown-unknown`).
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(start))]
pub fn start() {
    rune::run(AppSpec::<State> {
        scenes: &[("opened", |state| state.opened = 3)],
        ..AppSpec::new(TITLE, SIZE, State::default, view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    /// Layout, hit-testing and state run headlessly through Rune's `Runtime`:
    /// no GPU, no display, no flakiness. This is the quality gate for input and
    /// layout changes until e2e tests exist.
    #[test]
    fn the_open_window_button_counts_presses() {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut rt = Runtime::new(State::default(), view, SIZE, env);
        rt.frame(0.0);
        assert_eq!(rt.state.opened, 0);
        let (x, y) = rt.find("open-window").expect("the button is on screen");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..8 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.opened, 1, "one press opens one window");
    }
}
