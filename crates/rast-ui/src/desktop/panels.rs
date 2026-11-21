//! The Start launcher and taskbar popovers. These expose local desktop state,
//! not the classic web shell's server-side sessions or live event stream.

use rune::{Color, El, ScrollSpec, SurfaceFit, TextStyle, col, el, row, text};

use crate::State;
#[cfg(not(target_arch = "wasm32"))]
use crate::connection::Phase;

use super::{AppId, Palette, Panel, Wallpaper, icon, sign_out_button, wallpaper};

const TITLE: TextStyle = TextStyle::new(14.0, 680.0);
const BODY: TextStyle = TextStyle::new(13.0, 440.0);
const DETAIL: TextStyle = TextStyle::new(11.5, 440.0);
const SECTION: TextStyle = TextStyle::new(10.5, 560.0).tracking(0.7);
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

pub(super) fn view(
    state: &State,
    p: &Palette,
    width: f32,
    height: f32,
    compact: bool,
) -> Option<El> {
    let panel = state.desktop.panel?;
    let margin = if compact { 8.0 } else { 12.0 };
    let bottom = 54.0 + if compact { 8.0 } else { 10.0 };
    let space = height - bottom - margin;
    let available = width - 2.0 * margin;
    if space <= 0.0 || available <= 0.0 {
        return None;
    }

    let start = matches!(panel, Panel::Start);
    let (key, label, preferred_width, content) = match panel {
        Panel::Start => ("start-panel", "Start menu", 472.0_f32, start_menu(state, p)),
        Panel::Account => (
            "account-panel",
            "Account menu",
            292.0,
            account_menu(state, p),
        ),
        Panel::Status => (
            "status-panel",
            "Connection status",
            300.0,
            status_panel(state, p),
        ),
        Panel::Clock => ("clock-panel", "Date and time", 300.0, clock_panel(state, p)),
        Panel::Appearance => (
            "appearance-panel",
            "Personalize",
            332.0,
            appearance_panel(state, p),
        ),
        Panel::Shortcuts => (
            "shortcuts-panel",
            "Keyboard shortcuts",
            318.0,
            shortcuts_panel(state, p),
        ),
        Panel::About => ("about-panel", "About RAST", 318.0, about_panel(state, p)),
    };
    let panel_width = if compact {
        available
    } else {
        preferred_width.min(available)
    };
    let align_left =
        compact || start || matches!(panel, Panel::Appearance | Panel::Shortcuts | Panel::About);
    let (left, right) = if align_left {
        (Some(margin), None)
    } else {
        (None, Some(margin))
    };
    let max_height = if start && !compact {
        space.min(660.0)
    } else {
        space
    };

    Some(
        col()
            .key(key)
            .role("dialog")
            .label(label)
            .pin(left, None, right, Some(bottom))
            .w(panel_width)
            .max_h(max_height)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .scroll_margin(8.0)
            .radius(12.0)
            .clip_rounded(12.0)
            .border(1.0, p.line)
            .fill(p.surface)
            .shadow(7.0, 24.0, Color::BLACK.alpha(0.33))
            // Treat the spaces between controls as part of this panel, not the desktop.
            .focusable(false)
            .preserve_focus_on_press(true)
            .on_press(|_: &mut State| {})
            .child(content.w_full().padding(if compact { 12.0 } else { 14.0 })),
    )
}

fn start_menu(state: &State, p: &Palette) -> El {
    let who = state.account.as_ref().map_or_else(
        || VERSION.to_owned(),
        |account| format!("{VERSION} · signed in as {}", account.username),
    );
    let header = row()
        .w_full()
        .h(46.0)
        .px(5.0)
        .center_y()
        .gap(9.0)
        .child(badge(state, p, "logo", 32.0, true))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(title(p, "RAST"))
                .child(detail(p, who).truncate()),
        )
        .child(keycap(p, "Ctrl+Space"));

    col()
        .gap(9.0)
        .child(header)
        .child(divider(p))
        .child(
            col()
                .w_full()
                .child(open_panel_action(
                    state,
                    p,
                    "start-personalize",
                    "wallpaper",
                    "Personalize…",
                    Panel::Appearance,
                ))
                .child(open_panel_action(
                    state,
                    p,
                    "start-shortcuts",
                    "keyboard",
                    "Keyboard shortcuts",
                    Panel::Shortcuts,
                ))
                .child(open_panel_action(
                    state,
                    p,
                    "start-about",
                    "info",
                    "About RAST",
                    Panel::About,
                )),
        )
        .child(divider(p))
        .child(sign_out_button(p))
}

fn account_menu(state: &State, p: &Palette) -> El {
    let username = state
        .account
        .as_ref()
        .map_or("Device user", |account| account.username.as_str());
    let initial = username
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |ch| ch.to_uppercase().collect());
    col()
        .gap(8.0)
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .px(4.0)
                .py(3.0)
                .child(
                    el().size(40.0, 40.0)
                        .radius(20.0)
                        .fill(p.accent)
                        .center()
                        .child(text(initial).font(TITLE).color(p.accent_ink)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(title(p, username).wrap_anywhere())
                        .child(detail(p, "Local device account")),
                ),
        )
        .child(divider(p))
        .child(section(p, "APPEARANCE"))
        .child(theme_action(state, p, "account-theme"))
        .child(open_panel_action(
            state,
            p,
            "account-personalize",
            "wallpaper",
            "Personalize wallpaper…",
            Panel::Appearance,
        ))
        .child(divider(p))
        .child(
            col()
                .w_full()
                .child(open_panel_action(
                    state,
                    p,
                    "account-shortcuts",
                    "keyboard",
                    "Keyboard shortcuts",
                    Panel::Shortcuts,
                ))
                .child(open_panel_action(
                    state,
                    p,
                    "account-about",
                    "info",
                    "About RAST",
                    Panel::About,
                )),
        )
        .child(divider(p))
        .child(sign_out_button(p))
}

fn appearance_panel(state: &State, p: &Palette) -> El {
    let mut content = col()
        .gap(8.0)
        .child(header(
            state,
            p,
            "wallpaper",
            "Personalize",
            "Desktop appearance",
        ))
        .child(theme_action(state, p, "appearance-theme"))
        .child(divider(p))
        .child(section(p, "WALLPAPER"));
    for &choice in Wallpaper::ALL.iter() {
        let selected = state.desktop.wallpaper == choice;
        let mut preview = el()
            .size(52.0, 30.0)
            .radius(5.0)
            .fill(p.surface_2)
            .clip_rounded(5.0)
            .a11y_hidden();
        if let Some(image) = wallpaper(state, choice) {
            preview = preview.pixels_fit(image.pixels(), SurfaceFit::Fill);
        }
        content = content.child(
            action(p, format!("wallpaper-{}", choice.key()), choice.label())
                .role("radio")
                .checked(selected)
                .h(43.0)
                .child(preview)
                .child(
                    text(choice.label())
                        .font(BODY)
                        .color(p.ink)
                        .grow(1.0)
                        .truncate(),
                )
                .child(if selected {
                    detail(p, "Selected")
                } else {
                    el()
                })
                .on_press(move |s: &mut State| s.set_wallpaper(choice)),
        );
    }
    content.child(divider(p)).child(close_action(p))
}

fn shortcuts_panel(state: &State, p: &Palette) -> El {
    col()
        .gap(10.0)
        .child(header(
            state,
            p,
            "keyboard",
            "Keyboard shortcuts",
            "Desktop and Start menu",
        ))
        .child(key_value(p, "Open Start", "Ctrl+Space"))
        .child(key_value(p, "Close a popover", "Esc"))
        .child(divider(p))
        .child(close_action(p))
}

fn about_panel(state: &State, p: &Palette) -> El {
    col().gap(10.0)
        .child(header(state, p, "logo", "About RAST", VERSION))
        .child(detail(p, "RAST is a desktop workspace. The local device unlock is separate from server authentication.").wrap_anywhere())
        .child(divider(p))
        .child(close_action(p))
}

fn status_panel(state: &State, p: &Palette) -> El {
    #[cfg(target_arch = "wasm32")]
    let (status, note) = (
        "Local workspace",
        "This browser workspace does not connect to a RAST server or display live telemetry.",
    );
    #[cfg(not(target_arch = "wasm32"))]
    let (status, note) = (
        match state.status.phase {
            Phase::Setup => "Not configured",
            Phase::Connecting => "Connecting…",
            Phase::Online => "Server reachable",
            Phase::Offline => "Server unreachable",
        },
        "Connection status reflects server reachability, not an authenticated server session.",
    );
    let mut content = col()
        .gap(9.0)
        .child(header(state, p, "network", "Connection & session", "RAST"))
        .child(key_value(p, "Connection", status))
        .child(key_value(p, "Session", "Device unlock"))
        .child(key_value(p, "Version", VERSION));
    #[cfg(not(target_arch = "wasm32"))]
    {
        content = content.child(key_value(p, "Server", &state.config.server));
        if let Some(error) = &state.status.error {
            content = content.child(detail(p, error.as_str()).wrap_anywhere());
        }
        if matches!(state.status.phase, Phase::Offline) && state.link.is_some() {
            content = content.child(
                icon_action(state, p, "status-reconnect", "network", "Reconnect now")
                    .on_press(State::retry_now),
            );
        }
    }
    content = content
        .child(detail(p, note).wrap_anywhere())
        .child(divider(p));
    content
        .child(control_panel_action(state, p, "status-control"))
        .child(close_action(p))
}

fn clock_panel(state: &State, p: &Palette) -> El {
    col()
        .gap(8.0)
        .child(header(state, p, "clock", "Date and time", "Local time"))
        .child(
            text(if state.desktop.clock.is_empty() {
                "—"
            } else {
                &state.desktop.clock
            })
            .font(TextStyle::new(30.0, 700.0).features("'tnum' 1"))
            .color(p.ink),
        )
        .child(detail(p, state.desktop.date.as_str()).wrap_anywhere())
        .child(divider(p))
        .child(close_action(p))
}

fn header(state: &State, p: &Palette, name: &str, label: &str, subtitle: &str) -> El {
    row()
        .w_full()
        .center_y()
        .gap(9.0)
        .pb(3.0)
        .child(badge(state, p, name, 32.0, name == "logo"))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(title(p, label))
                .child(detail(p, subtitle)),
        )
}

fn title(p: &Palette, label: impl Into<String>) -> El {
    text(label).font(TITLE).color(p.ink)
}

fn detail(p: &Palette, label: impl Into<String>) -> El {
    text(label).font(DETAIL).color(p.muted)
}

fn section(p: &Palette, label: &str) -> El {
    text(label).font(SECTION).color(p.muted).px(5.0)
}

fn keycap(p: &Palette, label: &str) -> El {
    el().h(20.0)
        .px(6.0)
        .center()
        .radius(4.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
        .child(detail(p, label))
}

fn key_value(p: &Palette, label: &str, value: &str) -> El {
    row()
        .w_full()
        .gap(8.0)
        .child(detail(p, label).w(104.0).wrap())
        .child(
            text(value)
                .font(BODY)
                .color(p.ink)
                .grow(1.0)
                .min_w(0.0)
                .wrap_anywhere(),
        )
}

fn badge(state: &State, p: &Palette, name: &str, size: f32, accent: bool) -> El {
    el().size(size, size)
        .center()
        .radius(if accent { 9.0 } else { 10.0 })
        .border(1.0, p.line)
        .fill(if accent { p.accent } else { p.surface_2 })
        .child(icon(state, name, size * 0.58).a11y_hidden())
}

fn divider(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line).a11y_hidden()
}

fn action(p: &Palette, key: impl Into<String>, label: &str) -> El {
    row()
        .key(key.into())
        .role("button")
        .label(label)
        .focusable(true)
        .focus_ring(2.0, -2.0, p.accent)
        .w_full()
        .h(34.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(7.0)
        .hover(p.hover)
        .press(p.hover)
}

fn icon_action(state: &State, p: &Palette, key: &str, name: &str, label: &str) -> El {
    action(p, key, label)
        .child(icon(state, name, 16.0).a11y_hidden())
        .child(text(label).font(BODY).color(p.ink).truncate())
}

fn control_panel_action(state: &State, p: &Palette, key: &str) -> El {
    icon_action(state, p, key, "settings", "Control Panel")
        .on_press(|s: &mut State| s.open_app(AppId::Control))
}

fn open_panel_action(
    state: &State,
    p: &Palette,
    key: &str,
    name: &str,
    label: &str,
    panel: Panel,
) -> El {
    icon_action(state, p, key, name, label)
        .on_press(move |s: &mut State| s.toggle_desktop_panel(panel))
}

fn theme_action(state: &State, p: &Palette, key: &str) -> El {
    icon_action(state, p, key, "theme", "Switch theme").on_press(State::toggle_desktop_theme)
}

fn close_action(p: &Palette) -> El {
    action(p, "close-panel", "Close")
        .child(text("Close").font(BODY).color(p.ink))
        .on_press(State::dismiss_panel)
}

#[cfg(test)]
mod tests {
    use rune::{Env, Runtime};

    use super::*;
    use crate::config::Config;

    fn mounted(panel: Panel, size: (f32, f32)) -> Runtime<State> {
        let mut state = State::with_account(Config::default(), None);
        state.signed_in = true;
        state.toggle_desktop_panel(panel);
        let mut rt = Runtime::new(state, super::super::view, size, Env::new(true, 1.0));
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        rt
    }

    #[test]
    fn short_compact_panels_scroll_to_sign_out() {
        for (panel, key) in [
            (Panel::Start, "start-panel"),
            (Panel::Account, "account-panel"),
        ] {
            let mut rt = mounted(panel, (320.0, 280.0));
            assert_eq!(rt.scroll_at_bottom(key), Some(false));
            let (_, initial_y) = rt
                .find("sign-out")
                .expect("the panel has a sign-out action");
            assert!(
                initial_y > 218.0,
                "sign out starts below the scroll viewport"
            );
            let (x, y) = rt.find(key).expect("panel is visible");
            rt.wheel(x, y, -10_000.0);
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            assert_eq!(rt.scroll_at_bottom(key), Some(true));
            let (_, y) = rt.find("sign-out").expect("sign out scrolls into view");
            assert!(
                (8.0..=218.0).contains(&y),
                "sign out stays above the taskbar"
            );
        }
    }
}
