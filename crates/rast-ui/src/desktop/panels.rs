//! The Start launcher and taskbar popovers. These expose local desktop state,
//! not the classic web shell's server-side sessions or live event stream.

use rune::{Color, El, ScrollSpec, TextStyle, col, el, row, text};

use crate::State;
#[cfg(not(target_arch = "wasm32"))]
use crate::connection::Phase;

use super::{AppId, Palette, Panel, connectivity, icon, initial, sign_out_button};

const TITLE: TextStyle = TextStyle::new(13.5, 600.0);
const BODY: TextStyle = TextStyle::new(13.0, 400.0);
const DETAIL: TextStyle = TextStyle::new(11.5, 400.0);
const SECTION: TextStyle = TextStyle::new(10.5, 500.0).tracking(0.7);
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
    let bottom = super::BAR + if compact { 6.0 } else { 8.0 };
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
        Panel::Status => ("status-panel", "Network", 300.0, status_panel(state, p)),
        Panel::Device => ("device-panel", "This device", 312.0, device_panel(state, p)),
        Panel::Clock => ("clock-panel", "Date and time", 320.0, clock_panel(state, p)),
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
    let align_left = compact || start || matches!(panel, Panel::Shortcuts | Panel::About);
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
    let who = if state.session.is_some() || state.account.is_some() {
        format!("{VERSION} · signed in as {}", state.user_name())
    } else {
        VERSION.to_owned()
    };
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
            control_panel_entry(state, p, col().w_full(), "start-control", "Control Panel")
                .child(
                    icon_action(state, p, "start-personalize", "wallpaper", "Personalize…")
                        .on_press(State::open_personalize),
                )
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
    let name = state.user_name();
    let (subtitle, credentials, manage) = match &state.session {
        Some(session) => (
            format!("{} · RAST server", session.role.label()),
            col()
                .key("account-credentials")
                .w_full()
                .gap(6.0)
                .px(5.0)
                .child(key_value(p, "Username", &session.username))
                .child(key_value(p, "Role", session.role.label()))
                .child(key_value(p, "Sign-in", "RAST server password"))
                .child(key_value(p, "Session", "Secure cookie, 7 days"))
                .child(key_value(p, "Protection", "Argon2id hash")),
            Some(if session.role.is_admin() {
                "Users & accounts…"
            } else {
                "Profile & password…"
            }),
        ),
        None => {
            let stored = if cfg!(target_arch = "wasm32") {
                "In this browser only"
            } else {
                "On this device only"
            };
            (
                "Local device account".to_owned(),
                col()
                    .key("account-credentials")
                    .w_full()
                    .gap(6.0)
                    .px(5.0)
                    .child(key_value(p, "Username", name))
                    .child(key_value(p, "Sign-in", "Device password"))
                    .child(key_value(p, "Stored", stored))
                    .child(key_value(p, "Protection", "Argon2id hash"))
                    .child(key_value(p, "Server account", "Not linked")),
                None,
            )
        }
    };
    let mut menu = col()
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
                        .child(text(initial(name)).font(TITLE).color(p.accent_ink)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(title(p, name.to_owned()).wrap_anywhere())
                        .child(detail(p, subtitle)),
                ),
        )
        .child(divider(p))
        .child(section(p, "CREDENTIALS"))
        .child(credentials);
    if let Some(label) = manage {
        menu = control_panel_entry(state, p, menu, "account-manage", label);
    }
    menu.child(open_panel_action(
        state,
        p,
        "account-device",
        "device",
        "This device",
        Panel::Device,
    ))
    .child(divider(p))
    .child(section(p, "APPEARANCE"))
    .child(theme_action(state, p, "account-theme"))
    .child(
        icon_action(
            state,
            p,
            "account-personalize",
            "wallpaper",
            "Personalize wallpaper…",
        )
        .on_press(State::open_personalize),
    )
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
    let net = connectivity(state);
    #[cfg(target_arch = "wasm32")]
    let mut content = {
        let network = &state.desktop.network;
        let mut content = col()
            .gap(9.0)
            .child(header(
                state,
                p,
                "network",
                "Network",
                if network.online {
                    "Connected to the internet"
                } else {
                    "No internet connection"
                },
            ))
            .child(key_value(p, "Internet", net.label));
        if network.online {
            if let Some(link) = network.link_label() {
                content = content.child(key_value(p, "Connection", &link));
            }
            if let Some(downlink) = network.downlink.filter(|d| *d > 0.0) {
                content = content.child(key_value(
                    p,
                    "Bandwidth",
                    &format!("about {downlink:.1} Mb/s"),
                ));
            }
            if let Some(rtt) = network.rtt.filter(|r| *r > 0.0) {
                content = content.child(key_value(p, "Latency", &format!("about {rtt:.0} ms")));
            }
            if network.save_data {
                content = content.child(key_value(p, "Data saver", "On"));
            }
        }
        content
            .child(key_value(p, "RAST server", "Not connected in this preview"))
            .child(
                detail(
                    p,
                    "Reported by this browser. Speed and latency are its estimates.",
                )
                .wrap_anywhere(),
            )
    };
    #[cfg(not(target_arch = "wasm32"))]
    let mut content = {
        let mut content = col()
            .gap(9.0)
            .child(header(
                state,
                p,
                "network",
                "Network",
                match state.status.phase {
                    Phase::Setup => "Server not configured",
                    Phase::Connecting => "Connecting to the server…",
                    Phase::Online => "Server reachable",
                    Phase::Offline => "Server unreachable",
                },
            ))
            .child(key_value(p, "Server", net.label))
            .child(key_value(p, "Address", &state.config.server));
        if let Some(error) = &state.status.error {
            content = content.child(detail(p, error.as_str()).wrap_anywhere());
        }
        if matches!(state.status.phase, Phase::Offline) && state.link.is_some() {
            content = content.child(
                icon_action(state, p, "status-reconnect", "network", "Reconnect now")
                    .on_press(State::retry_now),
            );
        }
        content.child(
            detail(
                p,
                "Shows whether the RAST server answers, not an authenticated server session.",
            )
            .wrap_anywhere(),
        )
    };
    content = content.child(divider(p));
    control_panel_entry(state, p, content, "status-control", "Control Panel").child(close_action(p))
}

fn device_panel(state: &State, p: &Palette) -> El {
    let device = &state.desktop.device;
    let mut content = col()
        .gap(9.0)
        .child(header(state, p, "device", "This device", device.kind));
    if let Some(name) = &device.name {
        content = content.child(key_value(p, "Name", name));
    }
    content = content
        .child(key_value(p, "System", &device.system))
        .child(key_value(
            p,
            if cfg!(target_arch = "wasm32") {
                "Browser"
            } else {
                "App"
            },
            &device.client,
        ));
    for (label, value) in [
        ("Screen", &device.screen),
        ("Language", &device.language),
        ("Time zone", &device.time_zone),
    ] {
        if let Some(value) = value {
            content = content.child(key_value(p, label, value));
        }
    }
    content
        .child(detail(p, "Read on this device. RAST does not send it anywhere.").wrap_anywhere())
        .child(divider(p))
        .child(close_action(p))
}

fn clock_panel(state: &State, p: &Palette) -> El {
    let clock = &state.desktop.clock;
    let zone = match &state.desktop.device.time_zone {
        Some(zone) => format!("{zone} ({})", clock.offset),
        None => clock.offset.clone(),
    };
    col()
        .gap(8.0)
        .child(header(state, p, "clock", "Date and time", "Local time"))
        .child(
            text(clock.time.clone())
                .font(TextStyle::new(28.0, 550.0).features("'tnum' 1"))
                .color(p.ink),
        )
        .child(
            text(clock.long_date.clone())
                .font(BODY)
                .color(p.ink)
                .wrap_anywhere(),
        )
        .child(key_value(p, "Time zone", &zone))
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

/// Add the Control Panel entry to `list`, unless this account's groups deny it.
fn control_panel_entry(state: &State, p: &Palette, list: El, key: &str, label: &str) -> El {
    if !state.may_open(AppId::Control.access_id()) {
        return list;
    }
    list.child(
        icon_action(state, p, key, "settings", label)
            .on_press(|s: &mut State| s.open_app(AppId::Control)),
    )
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

    fn rect(rt: &Runtime<State>, key: &str) -> Option<[f32; 4]> {
        rt.presented(key).map(|(rect, _)| rect)
    }

    #[test]
    fn the_taskbar_is_slim_and_fits_from_phone_to_desktop_widths() {
        for width in [320.0, 390.0, 700.0, 900.0, 1440.0] {
            let height = 700.0;
            let mut state = State::with_account(Config::default(), None);
            state.signed_in = true;
            let mut rt = Runtime::new(
                state,
                super::super::view,
                (width, height),
                Env::new(true, 1.0),
            );
            rt.frame(1.0 / 60.0);
            let bar = rect(&rt, "desktop-taskbar").expect("the taskbar is drawn");
            assert_eq!(
                bar[3],
                super::super::BAR,
                "{width}: the taskbar is {} pt tall",
                bar[3]
            );
            assert!(bar[3] <= 44.0);
            let start = rect(&rt, "desktop-start").expect("Start is drawn");
            let mut right = start[0] + start[2];
            assert!(
                rect(&rt, "desktop-device").is_none(),
                "{width}: the taskbar does not name the browser"
            );
            for key in [
                "desktop-status",
                "desktop-theme",
                "desktop-clock",
                "desktop-account",
            ] {
                let Some(item) = rect(&rt, key) else {
                    assert!(
                        key == "desktop-theme",
                        "{width}: {key} is always in the tray"
                    );
                    continue;
                };
                assert!(
                    item[0] >= right,
                    "{width}: {key} overlaps the item before it"
                );
                assert!(
                    item[1] >= bar[1] && item[1] + item[3] <= bar[1] + bar[3],
                    "{width}: {key} fits the bar's height"
                );
                right = item[0] + item[2];
            }
            assert!(
                right <= width,
                "{width}: the tray ends at {right}, past the window"
            );
        }
    }

    #[test]
    fn wide_taskbars_label_the_network_and_account() {
        let mut state = State::with_account(Config::default(), None);
        state.signed_in = true;
        let mut rt = Runtime::new(
            state,
            super::super::view,
            (1440.0, 900.0),
            Env::new(true, 1.0),
        );
        rt.frame(1.0 / 60.0);
        let wide = |key| rect(&rt, key).map_or(0.0, |r| r[2]);
        assert!(wide("desktop-status") > 50.0, "the network has a label");
        assert!(
            wide("desktop-account") > 60.0,
            "the account shows the user name"
        );

        let mut state = State::with_account(Config::default(), None);
        state.signed_in = true;
        let mut rt = Runtime::new(
            state,
            super::super::view,
            (390.0, 700.0),
            Env::new(true, 1.0),
        );
        rt.frame(1.0 / 60.0);
        assert_eq!(
            rect(&rt, "desktop-status").map(|r| r[2]),
            Some(32.0),
            "icon only"
        );
        assert_eq!(
            rect(&rt, "desktop-account").map(|r| r[2]),
            Some(32.0),
            "avatar only"
        );
        let rt = mounted(Panel::Account, (390.0, 700.0));
        assert!(
            rect(&rt, "account-device").is_some(),
            "the account panel links the device"
        );
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
